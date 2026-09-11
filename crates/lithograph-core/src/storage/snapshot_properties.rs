//! Streaming property resolution.

use std::collections::{BTreeMap, btree_map};
use std::iter::Peekable;

use super::layer::{DeltaOp, PropertyDelta};
use super::property::PropertyColumns;
use super::snapshot::Snapshot;
use super::{OwnerKind, PropertyKeyId, PropertyValue, StorageResult};

type PropertyKey = (OwnerKind, i64, PropertyKeyId);
type PropertyOverlayIter<'a> = Peekable<btree_map::Iter<'a, PropertyKey, PropertyDelta>>;

impl Snapshot<'_> {
    /// Returns all visible properties for one graph element in PropertyKeyId order.
    /// This is a direct owner lookup and never scans unrelated property owners.
    pub fn properties(
        &self,
        owner_kind: OwnerKind,
        owner_id: i64,
    ) -> StorageResult<Vec<(PropertyKeyId, PropertyValue)>> {
        let mut values = BTreeMap::new();
        self.load_base_properties(owner_kind, owner_id, &mut values)?;
        self.apply_property_overlay(owner_kind, owner_id, &mut values)?;
        Ok(values.into_iter().collect())
    }

    fn load_base_properties(
        &self,
        owner_kind: OwnerKind,
        owner_id: i64,
        values: &mut BTreeMap<PropertyKeyId, PropertyValue>,
    ) -> StorageResult<()> {
        let Some(checkpoint) = self.checkpoint else {
            return Ok(());
        };
        let mut statement = self.connection.prepare(
            "SELECT key_id, type_tag, int_value, real_value, text_value, blob_value, aux_value FROM main._lithograph_cp_properties WHERE commit_id = ?1 AND owner_kind = ?2 AND owner_id = ?3 ORDER BY key_id",
        )?;
        let mut rows = statement.query(rusqlite::params![
            checkpoint.as_bytes().as_slice(),
            owner_kind as i64,
            owner_id,
        ])?;
        while let Some(row) = rows.next()? {
            let key_id = row.get(0)?;
            values.insert(
                key_id,
                property_columns_from_row_offset(row, 1)?.to_value()?,
            );
        }
        Ok(())
    }

    fn apply_property_overlay(
        &self,
        owner_kind: OwnerKind,
        owner_id: i64,
        values: &mut BTreeMap<PropertyKeyId, PropertyValue>,
    ) -> StorageResult<()> {
        for ((_, _, key_id), delta) in self
            .overlay
            .properties
            .range((owner_kind, owner_id, 0)..=(owner_kind, owner_id, i64::MAX))
        {
            apply_property_delta(values, *key_id, delta)?;
        }
        Ok(())
    }

    /// Streams visible properties in `(owner kind, owner id, key id)` order.
    pub fn visit_properties(
        &self,
        mut visitor: impl FnMut(OwnerKind, i64, PropertyKeyId, PropertyValue) -> StorageResult<()>,
    ) -> StorageResult<()> {
        let Some(checkpoint) = self.checkpoint else {
            return drain_properties(self.overlay.properties.iter(), &mut visitor);
        };
        let mut statement = self.connection.prepare(
            "SELECT owner_kind, owner_id, key_id, type_tag, int_value, real_value, text_value, blob_value, aux_value FROM main._lithograph_cp_properties WHERE commit_id = ?1 ORDER BY owner_kind, owner_id, key_id",
        )?;
        let mut rows = statement.query([checkpoint.as_bytes().as_slice()])?;
        let mut overlay = self.overlay.properties.iter().peekable();
        while let Some(row) = rows.next()? {
            let key = (OwnerKind::from_i64(row.get(0)?)?, row.get(1)?, row.get(2)?);
            let value = property_columns_from_row(row)?.to_value()?;
            merge_property_before(key, value, &mut overlay, &mut visitor)?;
        }
        drain_properties(overlay, &mut visitor)
    }
}

fn merge_property_before(
    base_key: PropertyKey,
    base_value: PropertyValue,
    overlay: &mut PropertyOverlayIter<'_>,
    visitor: &mut impl FnMut(OwnerKind, i64, PropertyKeyId, PropertyValue) -> StorageResult<()>,
) -> StorageResult<()> {
    while let Some((key, delta)) = overlay.peek() {
        let key = **key;
        if key > base_key {
            break;
        }
        let delta = (*delta).clone();
        overlay.next();
        visit_property_delta(key, &delta, visitor)?;
        if key == base_key {
            return Ok(());
        }
    }
    visitor(base_key.0, base_key.1, base_key.2, base_value)
}

fn drain_properties<'a>(
    overlay: impl IntoIterator<Item = (&'a PropertyKey, &'a PropertyDelta)>,
    visitor: &mut impl FnMut(OwnerKind, i64, PropertyKeyId, PropertyValue) -> StorageResult<()>,
) -> StorageResult<()> {
    for (key, delta) in overlay {
        visit_property_delta(*key, delta, visitor)?;
    }
    Ok(())
}

fn visit_property_delta(
    key: PropertyKey,
    delta: &PropertyDelta,
    visitor: &mut impl FnMut(OwnerKind, i64, PropertyKeyId, PropertyValue) -> StorageResult<()>,
) -> StorageResult<()> {
    if delta.op == DeltaOp::Add {
        let value = delta
            .value
            .clone()
            .ok_or_else(|| super::StorageError::corrupt("set property delta is missing value"))?;
        visitor(key.0, key.1, key.2, value)?;
    }
    Ok(())
}

fn apply_property_delta(
    values: &mut BTreeMap<PropertyKeyId, PropertyValue>,
    key_id: PropertyKeyId,
    delta: &PropertyDelta,
) -> StorageResult<()> {
    if delta.op == DeltaOp::Add {
        let value = delta
            .value
            .clone()
            .ok_or_else(|| super::StorageError::corrupt("set property delta is missing value"))?;
        values.insert(key_id, value);
    } else {
        values.remove(&key_id);
    }
    Ok(())
}

fn property_columns_from_row_offset(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<PropertyColumns> {
    Ok(PropertyColumns {
        type_tag: row.get(offset)?,
        int_value: row.get(offset + 1)?,
        real_value: row.get(offset + 2)?,
        text_value: row.get(offset + 3)?,
        blob_value: row.get(offset + 4)?,
        aux_value: row.get(offset + 5)?,
    })
}

fn property_columns_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PropertyColumns> {
    property_columns_from_row_offset(row, 3)
}
