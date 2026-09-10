//! Streaming property resolution.

use std::collections::btree_map;
use std::iter::Peekable;

use super::layer::{DeltaOp, PropertyDelta};
use super::property::PropertyColumns;
use super::snapshot::Snapshot;
use super::{OwnerKind, PropertyKeyId, PropertyValue, StorageResult};

type PropertyKey = (OwnerKind, i64, PropertyKeyId);
type PropertyOverlayIter<'a> = Peekable<btree_map::Iter<'a, PropertyKey, PropertyDelta>>;

impl Snapshot<'_> {
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

fn property_columns_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PropertyColumns> {
    Ok(PropertyColumns {
        type_tag: row.get(3)?,
        int_value: row.get(4)?,
        real_value: row.get(5)?,
        text_value: row.get(6)?,
        blob_value: row.get(7)?,
        aux_value: row.get(8)?,
    })
}
