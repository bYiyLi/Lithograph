//! Streaming property resolution.

use std::collections::{BTreeMap, btree_map};
use std::iter::Peekable;

use rusqlite::{params_from_iter, types::Value as SqlValue};

use super::layer::{DeltaOp, PropertyDelta};
use super::property::PropertyColumns;
use super::snapshot::Snapshot;
use super::{OwnerKind, PropertyKeyId, PropertyValue, StorageResult};

type PropertyKey = (OwnerKind, i64, PropertyKeyId);
type PropertyOverlayIter<'a> = Peekable<btree_map::Iter<'a, PropertyKey, PropertyDelta>>;

impl Snapshot<'_> {
    /// Prefetches canonical checkpoint properties for one bounded owner page.
    /// Overlay values remain authoritative because `property()` checks them first.
    pub(crate) fn prefetch_properties(
        &self,
        owner_kind: OwnerKind,
        owner_ids: &[i64],
        key_ids: &[PropertyKeyId],
    ) -> StorageResult<()> {
        let Some(checkpoint) = self.checkpoint else {
            return Ok(());
        };
        if owner_ids.is_empty() || key_ids.is_empty() {
            return Ok(());
        }
        // This is a pipeline-page cache, not retained query state. Clearing it
        // here keeps large Index scans bounded by the current page while
        // `property()` still observes Overlay values before checkpoint data.
        self.property_cache.borrow_mut().clear();
        for owners in owner_ids.chunks(128) {
            for keys in key_ids.chunks(16) {
                self.prefetch_property_chunk(checkpoint, owner_kind, owners, keys)?;
            }
        }
        Ok(())
    }

    fn prefetch_property_chunk(
        &self,
        checkpoint: super::HashId,
        owner_kind: OwnerKind,
        owner_ids: &[i64],
        key_ids: &[PropertyKeyId],
    ) -> StorageResult<()> {
        let needs_query = {
            let cache = self.property_cache.borrow();
            owner_ids.iter().any(|owner_id| {
                key_ids
                    .iter()
                    .any(|key_id| !cache.contains_key(&(owner_kind, *owner_id, *key_id)))
            })
        };
        if !needs_query {
            return Ok(());
        }
        {
            let mut cache = self.property_cache.borrow_mut();
            for owner_id in owner_ids {
                for key_id in key_ids {
                    cache
                        .entry((owner_kind, *owner_id, *key_id))
                        .or_insert(None);
                }
            }
        }
        let owner_parameters = vec!["?"; owner_ids.len()].join(",");
        let key_parameters = vec!["?"; key_ids.len()].join(",");
        let sql = format!(
            "SELECT owner_id, key_id, type_tag, int_value, real_value, text_value, blob_value, aux_value \
             FROM main._lithograph_cp_properties \
             WHERE commit_id = ? AND owner_kind = ? \
               AND owner_id IN ({owner_parameters}) AND key_id IN ({key_parameters})"
        );
        let mut parameters = Vec::with_capacity(2 + owner_ids.len() + key_ids.len());
        parameters.push(SqlValue::Blob(checkpoint.as_bytes().to_vec()));
        parameters.push(SqlValue::Integer(owner_kind as i64));
        parameters.extend(owner_ids.iter().copied().map(SqlValue::Integer));
        parameters.extend(key_ids.iter().copied().map(SqlValue::Integer));
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(parameters), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                property_columns_from_row_offset(row, 2)?,
            ))
        })?;
        for row in rows {
            let (owner_id, key_id, columns) = row?;
            self.property_cache
                .borrow_mut()
                .insert((owner_kind, owner_id, key_id), Some(columns.to_value()?));
        }
        Ok(())
    }

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
