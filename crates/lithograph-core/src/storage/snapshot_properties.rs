//! Streaming property resolution.

use std::collections::btree_map;
use std::iter::Peekable;

use rusqlite::{params_from_iter, types::Value as SqlValue};

use super::layer::{DeltaOp, PropertyDelta};
use super::snapshot::{CachedProperty, Snapshot};
use super::snapshot_read::{self, SnapshotReadObserver};
use super::{OwnerKind, PropertyKeyId, PropertyValue, StorageResult};

type PropertyKey = (OwnerKind, i64, PropertyKeyId);
type PropertyVisitor<'a> = dyn FnMut(
        OwnerKind,
        i64,
        PropertyKeyId,
        PropertyValue,
        Option<snapshot_read::ReadLease>,
    ) -> StorageResult<()>
    + 'a;
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
        let Some(checkpoint) = self.state.checkpoint else {
            return Ok(());
        };
        if owner_ids.is_empty() || key_ids.is_empty() {
            return Ok(());
        }
        // This is a pipeline-page cache, not retained query state. Clearing it
        // here keeps large Index scans bounded by the current page while
        // `property()` still observes Overlay values before checkpoint data.
        self.state.property_cache.borrow_mut().clear();
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
        if !self.reserve_prefetch_slots(owner_kind, owner_ids, key_ids)? {
            return Ok(());
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
        let mut rows = statement.query(params_from_iter(parameters))?;
        while let Some(row) = rows.next()? {
            let (owner_id, key_id) = (row.get::<_, i64>(0)?, row.get::<_, i64>(1)?);
            let property = snapshot_read::property_from_row(
                row,
                2,
                self.state.read_observer.as_deref(),
                &|| self.read_checkpoint(),
            )?;
            let mut cache = self.state.property_cache.borrow_mut();
            let cached = cache
                .get_mut(&(owner_kind, owner_id, key_id))
                .ok_or_else(|| {
                    super::StorageError::corrupt("prefetched property has no reserved cache slot")
                })?;
            cached.value = Some(property.value);
            // Preserve the preallocated map slot's claim when replacing None
            // with a decoded value; both allocations live until cache eviction.
            let memory = (cached._memory.take(), property.memory);
            cached._memory = match memory {
                (None, None) => None,
                memory => Some(std::rc::Rc::new(memory)),
            };
        }
        Ok(())
    }

    fn reserve_prefetch_slots(
        &self,
        owner_kind: OwnerKind,
        owner_ids: &[i64],
        key_ids: &[PropertyKeyId],
    ) -> StorageResult<bool> {
        let mut cache = self.state.property_cache.borrow_mut();
        let mut added = false;
        for owner_id in owner_ids {
            for key_id in key_ids {
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    cache.entry((owner_kind, *owner_id, *key_id))
                {
                    let bytes = snapshot_read::checked_add(
                        super::snapshot_staged::tree_entry_bytes::<PropertyKey, CachedProperty>()?,
                        256,
                    )?;
                    let memory =
                        snapshot_read::reserve(self.state.read_observer.as_deref(), bytes)?;
                    entry.insert(CachedProperty {
                        value: None,
                        _memory: memory,
                    });
                    added = true;
                }
            }
        }
        Ok(added)
    }

    /// Returns all visible properties for one graph element in PropertyKeyId order.
    /// This is a direct owner lookup and never scans unrelated property owners.
    pub fn properties(
        &self,
        owner_kind: OwnerKind,
        owner_id: i64,
    ) -> StorageResult<Vec<(PropertyKeyId, PropertyValue)>> {
        let mut values = Vec::new();
        let mut memory = Vec::new();
        self.visit_owner_properties(owner_kind, owner_id, |key, value| {
            memory.push(snapshot_read::reserve_value(
                value,
                self.state.read_observer.as_deref(),
                &|| self.read_checkpoint(),
            )?);
            values.push((
                key,
                snapshot_read::clone_value_checked(value, &|| self.read_checkpoint())?,
            ));
            Ok::<_, super::StorageError>(())
        })?;
        Ok(values)
    }

    /// Streams visible properties in `(owner kind, owner id, key id)` order.
    pub fn visit_properties(
        &self,
        mut visitor: impl FnMut(OwnerKind, i64, PropertyKeyId, PropertyValue) -> StorageResult<()>,
    ) -> StorageResult<()> {
        self.visit_properties_observed(
            &|| self.read_checkpoint(),
            |owner, id, key, value, memory| {
                let _memory = memory;
                visitor(owner, id, key, value)
            },
        )
    }

    /// Transfers both decoded values and their allocation leases to a materialized reader.
    pub(crate) fn visit_properties_observed(
        &self,
        check: &dyn Fn() -> StorageResult<()>,
        mut visitor: impl FnMut(
            OwnerKind,
            i64,
            PropertyKeyId,
            PropertyValue,
            Option<snapshot_read::ReadLease>,
        ) -> StorageResult<()>,
    ) -> StorageResult<()> {
        let mut overlay = self.state.overlay.properties.iter().peekable();
        if let Some(checkpoint) = self.state.checkpoint {
            self.visit_checkpoint_properties_observed(
                checkpoint,
                &mut overlay,
                check,
                &mut visitor,
            )?;
        }
        for (key, delta) in overlay {
            visit_property_delta(
                *key,
                delta,
                self.state.read_observer.as_deref(),
                check,
                &mut visitor,
            )?;
        }
        Ok(())
    }
    fn visit_checkpoint_properties_observed(
        &self,
        checkpoint: super::HashId,
        overlay: &mut PropertyOverlayIter<'_>,
        check: &dyn Fn() -> StorageResult<()>,
        visitor: &mut PropertyVisitor<'_>,
    ) -> StorageResult<()> {
        let mut statement = self.connection.prepare(
            "SELECT owner_kind, owner_id, key_id, type_tag, int_value, real_value, text_value, blob_value, aux_value FROM main._lithograph_cp_properties WHERE commit_id = ?1 ORDER BY owner_kind, owner_id, key_id",
        )?;
        let mut rows = statement.query([checkpoint.as_bytes().as_slice()])?;
        while let Some(row) = rows.next()? {
            check()?;
            let key = (OwnerKind::from_i64(row.get(0)?)?, row.get(1)?, row.get(2)?);
            if visit_overlay_through(
                key,
                overlay,
                self.state.read_observer.as_deref(),
                check,
                visitor,
            )? {
                continue;
            }
            let property = snapshot_read::property_from_row(
                row,
                3,
                self.state.read_observer.as_deref(),
                check,
            )?;
            visitor(key.0, key.1, key.2, property.value, property.memory)?;
        }
        Ok(())
    }
}

fn visit_overlay_through(
    base_key: PropertyKey,
    overlay: &mut PropertyOverlayIter<'_>,
    observer: Option<&dyn SnapshotReadObserver>,
    check: &dyn Fn() -> StorageResult<()>,
    visitor: &mut PropertyVisitor<'_>,
) -> StorageResult<bool> {
    while let Some((key, delta)) = overlay.peek() {
        let key = **key;
        if key > base_key {
            break;
        }
        let delta = *delta;
        overlay.next();
        visit_property_delta(key, delta, observer, check, visitor)?;
        if key == base_key {
            return Ok(true);
        }
    }
    Ok(false)
}

fn visit_property_delta(
    key: PropertyKey,
    delta: &PropertyDelta,
    observer: Option<&dyn SnapshotReadObserver>,
    check: &dyn Fn() -> StorageResult<()>,
    visitor: &mut PropertyVisitor<'_>,
) -> StorageResult<()> {
    check()?;
    if delta.op == DeltaOp::Add {
        let borrowed = delta
            .value
            .as_ref()
            .ok_or_else(|| super::StorageError::corrupt("set property delta is missing value"))?;
        let memory = snapshot_read::reserve_value(borrowed, observer, check)?;
        visitor(
            key.0,
            key.1,
            key.2,
            snapshot_read::clone_value_checked(borrowed, check)?,
            memory,
        )?;
    }
    Ok(())
}
