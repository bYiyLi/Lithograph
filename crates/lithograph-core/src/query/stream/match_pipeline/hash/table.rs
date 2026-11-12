//! Chained candidates for one bounded Hash build. Digest equality and Cypher
//! key equality remain the probe consumer's responsibility.

use super::key::{EvaluatedHashKey, hash_key_inline_bytes};
use crate::query::expression::BindingRow;
use crate::query::resources::{
    ExecutionControl, MemoryReservation, allocation_bytes, checked_add, memory_error,
};
use crate::query::{QueryError, QueryResult};

#[derive(Debug)]
pub(super) struct HashEntry {
    pub(super) row: BindingRow,
    pub(super) row_memory: MemoryReservation,
    pub(super) key: EvaluatedHashKey,
    pub(super) next: Option<usize>,
}

#[derive(Debug)]
pub(in crate::query::stream::match_pipeline) struct HashTable {
    // Allocations precede their leases so drop frees the allocations first.
    entries: Vec<HashEntry>,
    heads: Vec<Option<usize>>,
    entry_memory: MemoryReservation,
    head_memory: MemoryReservation,
    payload_bytes: u64,
    owned_bytes: u64,
    peak_bytes: u64,
}

impl HashTable {
    pub(super) fn new(control: &ExecutionControl<'_>) -> QueryResult<Self> {
        Ok(Self {
            entries: Vec::new(),
            heads: Vec::new(),
            entry_memory: control.reserve(0)?,
            head_memory: control.reserve(0)?,
            payload_bytes: 0,
            owned_bytes: 0,
            peak_bytes: 0,
        })
    }

    /// Absolute table peak for another already-reserved row/key. The caller
    /// compares this with its build threshold before consuming that input.
    pub(super) fn projected_bytes(&self, row_mem: u64, key_mem: u64) -> QueryResult<u64> {
        let incoming = checked_add(row_mem, key_mem)?;
        let mut live = checked_add(self.owned_bytes, incoming)?;
        let mut peak = live;
        let needed = self.next_len()?;
        if needed > self.entries.capacity() {
            let bytes = allocation_bytes::<HashEntry>(next_capacity(needed)?)?;
            peak = peak.max(checked_add(live, bytes)?);
            live = checked_add(live - self.entry_memory.bytes(), bytes)?;
        }
        if needed > self.heads.len() {
            let bytes = allocation_bytes::<Option<usize>>(next_capacity(needed)?)?;
            peak = peak.max(checked_add(live, bytes)?);
        }
        Ok(peak)
    }

    /// Consumes fresh row/key claims, including their inline headers. Drained
    /// entries already have normalized heap-only claims and are not fresh input.
    pub(super) fn insert_reserved(
        &mut self,
        row: BindingRow,
        row_memory: MemoryReservation,
        key: EvaluatedHashKey,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        // The same drop order must hold before insertion, including errors.
        let mut entry = HashEntry {
            row,
            row_memory,
            key,
            next: None,
        };
        let incoming =
            self.validate_leases(&mut entry.row_memory, &mut entry.key.memory, control)?;
        let (row_heap, key_heap) = entry_heap_bytes(&entry)?;
        self.reserve_slot(incoming, control)?;
        control.work(1)?;
        let payload = checked_add(self.payload_bytes, checked_add(row_heap, key_heap)?)?;
        let owned = checked_add(payload, self.buffer_bytes()?)?;
        let bucket = bucket(&entry.key.digest, self.heads.len());
        let index = self.entries.len();
        entry.next = self.heads[bucket];
        self.entries.push(entry);
        self.heads[bucket] = Some(index);
        // The Vec slot now accounts for these moved inline values. The full
        // incoming claims remained held throughout allocation and the move.
        self.entries[index].row_memory.resize(row_heap)?;
        self.entries[index].key.memory.resize(key_heap)?;
        self.payload_bytes = payload;
        self.owned_bytes = owned;
        self.peak_bytes = self.peak_bytes.max(owned);
        Ok(())
    }

    fn validate_leases(
        &mut self,
        row_memory: &mut MemoryReservation,
        key_memory: &mut MemoryReservation,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<u64> {
        // Zero-byte transfers verify that all claims belong to this execution.
        self.entry_memory.merge(control.reserve(0)?)?;
        self.entry_memory.merge(row_memory.split(0)?)?;
        self.entry_memory.merge(key_memory.split(0)?)?;
        checked_add(row_memory.bytes(), key_memory.bytes())
    }

    fn reserve_slot(&mut self, incoming: u64, control: &ExecutionControl<'_>) -> QueryResult<()> {
        self.record_peak(incoming)?;
        let needed = self.next_len()?;
        if needed > self.entries.capacity() {
            self.grow_entries(next_capacity(needed)?, incoming, control)?;
        }
        if needed > self.heads.len() {
            self.grow_heads(next_capacity(needed)?, incoming, control)?;
        }
        Ok(())
    }

    pub(super) fn head(&self, digest: &[u8; 32]) -> Option<usize> {
        if self.heads.is_empty() {
            None
        } else {
            self.heads[bucket(digest, self.heads.len())]
        }
    }

    pub(super) fn entry(&self, index: usize) -> Option<&HashEntry> {
        self.entries.get(index)
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn owned_bytes(&self) -> u64 {
        self.owned_bytes
    }

    pub(super) fn peak_bytes(&self) -> u64 {
        self.peak_bytes
    }

    pub(super) fn flush(self) -> HashDrain {
        let Self {
            entries,
            heads,
            entry_memory,
            head_memory,
            ..
        } = self;
        drop(heads);
        drop(head_memory);
        HashDrain {
            entries: entries.into_iter(),
            _entry_memory: entry_memory,
        }
    }

    fn next_len(&self) -> QueryResult<usize> {
        self.entries
            .len()
            .checked_add(1)
            .ok_or_else(|| memory_error("Hash table row count overflow"))
    }

    fn buffer_bytes(&self) -> QueryResult<u64> {
        checked_add(self.entry_memory.bytes(), self.head_memory.bytes())
    }

    fn refresh_owned(&mut self) -> QueryResult<()> {
        self.owned_bytes = checked_add(self.payload_bytes, self.buffer_bytes()?)?;
        Ok(())
    }

    fn record_peak(&mut self, additional: u64) -> QueryResult<()> {
        self.peak_bytes = self
            .peak_bytes
            .max(checked_add(self.owned_bytes, additional)?);
        Ok(())
    }

    fn grow_entries(
        &mut self,
        capacity: usize,
        incoming: u64,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let mut memory = control.reserve(allocation_bytes::<HashEntry>(capacity)?)?;
        self.record_peak(checked_add(incoming, memory.bytes())?)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| memory_error("Hash entry buffer allocation failed"))?;
        memory.resize(allocation_bytes::<HashEntry>(entries.capacity())?)?;
        self.record_peak(checked_add(incoming, memory.bytes())?)?;
        // A bounded, allocation-free bulk move keeps every row/lease intact.
        control.work(self.entries.len() as u64)?;
        entries.append(&mut self.entries);
        let old_entries = std::mem::replace(&mut self.entries, entries);
        let old_memory = std::mem::replace(&mut self.entry_memory, memory);
        drop(old_entries);
        drop(old_memory);
        self.refresh_owned()
    }

    fn grow_heads(
        &mut self,
        capacity: usize,
        incoming: u64,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let mut memory = control.reserve(allocation_bytes::<Option<usize>>(capacity)?)?;
        self.record_peak(checked_add(incoming, memory.bytes())?)?;
        let mut heads = Vec::new();
        heads
            .try_reserve_exact(capacity)
            .map_err(|_| memory_error("Hash bucket buffer allocation failed"))?;
        memory.resize(allocation_bytes::<Option<usize>>(heads.capacity())?)?;
        self.record_peak(checked_add(incoming, memory.bytes())?)?;
        for _ in 0..capacity {
            control.work(1)?;
            heads.push(None);
        }
        if let Err(error) = rebuild_chains(&mut self.entries, &mut heads, control) {
            // Interrupted builds are terminal for the caller. Restoring old
            // links also leaves this owner safe to inspect/drop on that path.
            restore_chains(&mut self.entries, &mut self.heads);
            return Err(error);
        }
        let old_heads = std::mem::replace(&mut self.heads, heads);
        let old_memory = std::mem::replace(&mut self.head_memory, memory);
        drop(old_heads);
        drop(old_memory);
        self.refresh_owned()
    }
}

/// Conservative build peak using the table's real entry/bucket layout. Payload
/// excludes inline row/key headers, which the owning entry buffer already holds.
pub(super) fn estimate_peak(rows: u64, payload_per_row: u64) -> Option<u64> {
    if rows == 0 {
        return Some(0);
    }
    let capacity = next_capacity(usize::try_from(rows).ok()?).ok()?;
    let concurrent_capacity = capacity.checked_add(capacity / 2)?;
    let entries = allocation_bytes::<HashEntry>(concurrent_capacity).ok()?;
    let heads = allocation_bytes::<Option<usize>>(concurrent_capacity).ok()?;
    // Simultaneously allowing both replacement buffers is conservative for
    // their sequential growth. One input's full inline headers also coexist.
    rows.checked_mul(payload_per_row)?
        .checked_add(entries)?
        .checked_add(heads)?
        .checked_add(allocation_bytes::<HashEntry>(1).ok()?)
}

fn entry_heap_bytes(entry: &HashEntry) -> QueryResult<(u64, u64)> {
    let row = entry
        .row_memory
        .bytes()
        .checked_sub(allocation_bytes::<BindingRow>(1)?)
        .ok_or_else(|| QueryError::internal("Hash input row is missing its inline claim"))?;
    let key = entry
        .key
        .memory
        .bytes()
        .checked_sub(hash_key_inline_bytes(&entry.key.value))
        .ok_or_else(|| QueryError::internal("Hash input key is missing its inline claim"))?;
    Ok((row, key))
}

fn next_capacity(needed: usize) -> QueryResult<usize> {
    needed
        .checked_next_power_of_two()
        .ok_or_else(|| memory_error("Hash table capacity overflow"))
}

fn bucket(digest: &[u8; 32], count: usize) -> usize {
    debug_assert!(count.is_power_of_two());
    let mut low = [0_u8; 8];
    low.copy_from_slice(&digest[..8]);
    // The mask bounds the value by a usize-sized bucket count on all targets.
    (u64::from_le_bytes(low) & (count as u64 - 1)) as usize
}

fn rebuild_chains(
    entries: &mut [HashEntry],
    heads: &mut [Option<usize>],
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    for (index, entry) in entries.iter_mut().enumerate() {
        control.work(1)?;
        let bucket = bucket(&entry.key.digest, heads.len());
        entry.next = heads[bucket];
        heads[bucket] = Some(index);
    }
    Ok(())
}

fn restore_chains(entries: &mut [HashEntry], heads: &mut [Option<usize>]) {
    heads.fill(None);
    for (index, entry) in entries.iter_mut().enumerate() {
        let bucket = bucket(&entry.key.digest, heads.len());
        entry.next = heads[bucket];
        heads[bucket] = Some(index);
    }
}

#[derive(Debug)]
pub(super) struct HashDrain {
    entries: std::vec::IntoIter<HashEntry>,
    // IntoIter still owns its buffer even after its last entry was moved out.
    _entry_memory: MemoryReservation,
}

impl Iterator for HashDrain {
    type Item = HashEntry;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}

impl ExactSizeIterator for HashDrain {}
impl std::iter::FusedIterator for HashDrain {}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::super::key::HashKeyValue;
    use super::*;
    use crate::cypher::Value;
    use crate::query::QueryErrorKind;
    use crate::query::expression::{BindingValue, binding_row_bytes};
    use crate::query::resources::{QueryResources, value_bytes};

    fn input(
        control: &ExecutionControl<'_>,
        id: i64,
        digest: [u8; 32],
    ) -> (BindingRow, MemoryReservation, EvaluatedHashKey) {
        let row = BindingRow::default();
        let row_memory = control.reserve(binding_row_bytes(&row).unwrap()).unwrap();
        let key = EvaluatedHashKey {
            digest,
            value: HashKeyValue::NodeId(id),
            memory: control.reserve(0).unwrap(),
        };
        (row, row_memory, key)
    }

    fn insert(table: &mut HashTable, control: &ExecutionControl<'_>, id: i64, digest: [u8; 32]) {
        let (row, memory, key) = input(control, id, digest);
        table.insert_reserved(row, memory, key, control).unwrap();
    }

    fn assert_internal_input_rejected(
        table: &mut HashTable,
        input: (BindingRow, MemoryReservation, EvaluatedHashKey),
        control: &ExecutionControl<'_>,
    ) {
        let (row, memory, key) = input;
        let error = table
            .insert_reserved(row, memory, key, control)
            .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Internal);
        assert_eq!(table.len(), 0);
        assert_eq!(table.owned_bytes(), 0);
        assert_eq!(control.resources().used(), 0);
    }

    fn chain(table: &HashTable, digest: &[u8; 32]) -> Vec<usize> {
        let mut next = table.head(digest);
        let mut output = Vec::new();
        while let Some(index) = next {
            output.push(index);
            next = table.entry(index).unwrap().next;
        }
        output
    }

    #[test]
    fn bucket_chains_retain_duplicate_keys_and_full_digest_collisions() {
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let mut table = HashTable::new(&control).unwrap();
        let first = [0; 32];
        let mut other = first;
        other[31] = 1;
        insert(&mut table, &control, 7, first);
        insert(&mut table, &control, 7, first);
        insert(&mut table, &control, 8, first);
        insert(&mut table, &control, 9, other);

        assert_eq!(table.len(), 4);
        assert_eq!(chain(&table, &first), vec![3, 2, 1, 0]);
        assert_eq!(table.entry(3).unwrap().key.digest, other);
        assert!(matches!(
            table.entry(2).unwrap().key.value,
            HashKeyValue::NodeId(8)
        ));
        assert_eq!(resources.used(), table.owned_bytes());
        drop(table);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn projection_includes_old_and_new_buffers_at_every_growth() {
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let mut table = HashTable::new(&control).unwrap();
        for id in 0..17 {
            let previous_peak = table.peak_bytes();
            let (row, memory, key) = input(&control, id, [0; 32]);
            let projected = table
                .projected_bytes(memory.bytes(), key.memory.bytes())
                .unwrap();
            table.insert_reserved(row, memory, key, &control).unwrap();
            assert_eq!(resources.used(), table.owned_bytes());
            assert_eq!(resources.peak(), table.peak_bytes());
            assert!(table.peak_bytes() <= previous_peak.max(projected));
            assert!(table.peak_bytes() <= estimate_peak(id as u64 + 1, 0).unwrap());
        }
        assert!(table.peak_bytes() > table.owned_bytes());
        assert!(table.projected_bytes(u64::MAX, 1).is_err());
        drop(table);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn moved_row_and_scalar_headers_are_charged_by_the_entry_buffer_once() {
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let mut table = HashTable::new(&control).unwrap();
        let (row, memory, _) = input(&control, 1, [0; 32]);
        let text = String::with_capacity(37);
        let heap_bytes = text.capacity() as u64;
        let value = Value::String(text);
        let key = EvaluatedHashKey {
            digest: [0; 32],
            memory: control.reserve(value_bytes(&value).unwrap()).unwrap(),
            value: HashKeyValue::Scalar(value),
        };

        table.insert_reserved(row, memory, key, &control).unwrap();

        let entry = table.entry(0).unwrap();
        assert_eq!(entry.row_memory.bytes(), 0);
        assert_eq!(entry.key.memory.bytes(), heap_bytes);
        assert_eq!(table.payload_bytes, heap_bytes);
        assert_eq!(resources.used(), table.owned_bytes());
        assert!(resources.peak() <= estimate_peak(1, heap_bytes).unwrap());
        drop(table);
        assert_eq!(resources.used(), 0);
        assert_eq!(estimate_peak(0, u64::MAX), Some(0));
        assert_eq!(estimate_peak(u64::MAX, 0), None);
        assert_eq!(estimate_peak(1, u64::MAX), None);
    }

    #[test]
    fn inputs_missing_full_inline_claims_are_rejected_before_buffer_growth() {
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let mut table = HashTable::new(&control).unwrap();
        let (row, mut memory, key) = input(&control, 1, [0; 32]);
        memory.resize(0).unwrap();
        assert_internal_input_rejected(&mut table, (row, memory, key), &control);
        let (row, memory, mut key) = input(&control, 2, [0; 32]);
        key.value = HashKeyValue::Scalar(Value::Integer(2));
        assert_internal_input_rejected(&mut table, (row, memory, key), &control);
    }

    #[test]
    fn entry_growth_limit_failure_keeps_the_existing_table_intact() {
        let incoming = allocation_bytes::<BindingRow>(1).unwrap();
        let old_buffers = allocation_bytes::<HashEntry>(1).unwrap()
            + allocation_bytes::<Option<usize>>(1).unwrap();
        let replacement = allocation_bytes::<HashEntry>(2).unwrap();
        let resources = QueryResources::new(Some(old_buffers + incoming + replacement - 1));
        let control = ExecutionControl::new(&resources, &|| false);
        let mut table = HashTable::new(&control).unwrap();
        insert(&mut table, &control, 3, [0; 32]);
        let before = table.owned_bytes();
        let (row, memory, key) = input(&control, 4, [0; 32]);

        let error = table
            .insert_reserved(row, memory, key, &control)
            .unwrap_err();

        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(table.len(), 1);
        assert_eq!(chain(&table, &[0; 32]), vec![0]);
        assert_eq!(table.owned_bytes(), before);
        assert_eq!(resources.used(), before);
        drop(table);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn flush_moves_rows_and_holds_the_entry_buffer_until_drain_drop() {
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let mut table = HashTable::new(&control).unwrap();
        let (mut row, mut memory, key) = input(&control, 1, [0; 32]);
        row.insert("first".to_owned(), BindingValue::Node(7));
        row.insert("second".to_owned(), BindingValue::Node(8));
        row.used_relationships.insert(19);
        memory.resize(binding_row_bytes(&row).unwrap()).unwrap();
        table.insert_reserved(row, memory, key, &control).unwrap();
        insert(&mut table, &control, 2, [0; 32]);
        let buffers = table.entry_memory.bytes();
        let buckets = table.head_memory.bytes();
        let before = resources.used();
        let mut drain = table.flush();
        assert_eq!(resources.used(), before - buckets);
        let held = drain.next().unwrap();
        let held_payload = held.row_memory.bytes() + held.key.memory.bytes();
        assert_eq!(held.row.order, vec!["first", "second"]);
        assert_eq!(
            held.row
                .used_relationships
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![19]
        );
        assert_eq!(drain.len(), 1);
        drain.by_ref().for_each(drop);
        assert_eq!(resources.used(), buffers + held_payload);
        assert!(drain.next().is_none());
        drop(drain);
        assert_eq!(resources.used(), held_payload);
        drop(held);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn foreign_execution_claims_are_rejected_before_allocating_buffers() {
        let resources = QueryResources::default();
        let foreign = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let other = ExecutionControl::new(&foreign, &|| false);
        let mut table = HashTable::new(&control).unwrap();
        let (row, memory, key) = input(&other, 1, [0; 32]);
        assert_internal_input_rejected(&mut table, (row, memory, key), &control);
        assert_eq!(foreign.used(), 0);
    }

    #[test]
    fn interrupted_rehash_restores_old_links_and_releases_new_buckets() {
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let mut table = HashTable::new(&control).unwrap();
        for id in 0_i64..256 {
            let mut digest = [0; 32];
            digest[..8].copy_from_slice(&id.to_le_bytes());
            insert(&mut table, &control, id, digest);
        }
        let heads = table.heads.clone();
        let links = table
            .entries
            .iter()
            .map(|entry| entry.next)
            .collect::<Vec<_>>();
        let before = resources.used();
        let checks = Cell::new(0);
        let interrupted = || {
            checks.set(checks.get() + 1);
            checks.get() == 4
        };
        let control = ExecutionControl::new(&resources, &interrupted);

        let error = table.grow_heads(512, 0, &control).unwrap_err();

        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert_eq!(checks.get(), 4);
        assert_eq!(table.heads, heads);
        assert_eq!(
            table
                .entries
                .iter()
                .map(|entry| entry.next)
                .collect::<Vec<_>>(),
            links
        );
        assert_eq!(resources.used(), before);
        assert_eq!(resources.used(), table.owned_bytes());
        drop(table);
        assert_eq!(resources.used(), 0);
    }
}
