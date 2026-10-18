use rusqlite::{Connection, Params, types::ValueRef};

use crate::query::resources::{
    ExecutionControl, MemoryReservation, QueryResources, allocation_bytes, checked_add,
    memory_error,
};
use crate::query::{QueryError, QueryResult};

pub(super) struct Budgeted<T> {
    pub(super) value: T,
    pub(super) memory: MemoryReservation,
}

/// The inline Vec capacity and the heaps owned by its elements have one lease.
/// Clearing the elements releases their heaps, while retained slots stay billed.
pub(super) struct Buffer<T> {
    pub(super) values: Vec<T>,
    pub(super) memory: MemoryReservation,
    heap_bytes: u64,
}

impl<T> Buffer<T> {
    pub(super) fn new(resources: &QueryResources) -> QueryResult<Self> {
        Ok(Self {
            values: Vec::new(),
            memory: resources.reserve(0)?,
            heap_bytes: 0,
        })
    }

    /// Precharge an element's owned heap before constructing or copying it.
    /// Pass zero as that element's heap cost to `push` after this succeeds.
    pub(super) fn reserve(&mut self, bytes: u64) -> QueryResult<()> {
        let next_heap = checked_add(self.heap_bytes, bytes)?;
        self.memory.grow(bytes)?;
        self.heap_bytes = next_heap;
        Ok(())
    }

    pub(super) fn absorb(&mut self, memory: MemoryReservation) -> QueryResult<()> {
        let next_heap = checked_add(self.heap_bytes, memory.bytes())?;
        self.memory.merge(memory)?;
        self.heap_bytes = next_heap;
        Ok(())
    }

    pub(super) fn push(&mut self, value: T, heap_bytes: u64) -> QueryResult<()> {
        let next_heap = checked_add(self.heap_bytes, heap_bytes)?;
        let previous = self.memory.bytes();
        let capacity = self.next_capacity()?;
        self.memory
            .resize(checked_add(allocation_bytes::<T>(capacity)?, next_heap)?)?;
        if let Err(error) = self.values.try_reserve_exact(capacity - self.values.len()) {
            self.memory.resize(previous)?;
            return Err(memory_error(&format!(
                "cannot allocate semantic execution buffer: {error}"
            )));
        }
        self.values.push(value);
        self.heap_bytes = next_heap;
        Ok(())
    }

    fn next_capacity(&self) -> QueryResult<usize> {
        let capacity = self.values.capacity();
        if self.values.len() < capacity {
            return Ok(capacity);
        }
        capacity
            .checked_mul(2)
            .map(|capacity| capacity.max(4))
            .ok_or_else(|| memory_error("semantic execution buffer capacity overflow"))
    }

    pub(super) fn clear(&mut self) -> QueryResult<()> {
        self.values.clear();
        // This only shrinks an already valid reservation: capacity was billed
        // before allocation, and clear has now dropped every owned element.
        let retained = self.memory.bytes() - self.heap_bytes;
        self.memory.resize(retained)?;
        self.heap_bytes = 0;
        Ok(())
    }

    pub(super) fn into_parts(self) -> (Vec<T>, MemoryReservation) {
        (self.values, self.memory)
    }
}

pub(super) fn vector_bytes(dimensions: usize) -> QueryResult<u64> {
    allocation_bytes::<f32>(dimensions)
}

pub(super) fn encode_vector(
    vector: &[f32],
    control: &ExecutionControl<'_>,
) -> QueryResult<Budgeted<Vec<u8>>> {
    let bytes = vector_bytes(vector.len())?;
    let memory = control.reserve(bytes)?;
    let capacity = usize::try_from(bytes)
        .map_err(|_| memory_error("semantic vector encoding size overflow"))?;
    let mut blob = Vec::new();
    blob.try_reserve_exact(capacity)
        .map_err(|_| memory_error("cannot allocate semantic vector encoding"))?;
    for value in vector {
        control.work(1)?;
        blob.extend_from_slice(&value.to_le_bytes());
    }
    control.check()?;
    Ok(Budgeted {
        value: blob,
        memory,
    })
}

pub(super) fn decode_vector(
    blob: &[u8],
    dimensions: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<Budgeted<Vec<f32>>> {
    let bytes = vector_bytes(dimensions)?;
    if u64::try_from(blob.len()).ok() != Some(bytes) {
        return Err(QueryError::internal(
            "semantic TEMP vector has invalid dimensions",
        ));
    }
    let memory = control.reserve(bytes)?;
    let mut vector = Vec::new();
    vector
        .try_reserve_exact(dimensions)
        .map_err(|_| memory_error("cannot allocate semantic vector decoding"))?;
    for chunk in blob.as_chunks::<4>().0 {
        control.work(1)?;
        let value = f32::from_le_bytes(*chunk);
        if !value.is_finite() {
            return Err(QueryError::internal(
                "semantic TEMP vector contains a non-finite coordinate",
            ));
        }
        vector.push(value);
    }
    control.check()?;
    Ok(Budgeted {
        value: vector,
        memory,
    })
}

pub(super) fn read_vector(
    connection: &Connection,
    sql: &str,
    params: impl Params,
    dimensions: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<Budgeted<Vec<f32>>>> {
    control.check()?;
    let mut statement = connection.prepare(sql)?;
    let mut rows = statement.query(params)?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let dimension: i64 = row.get(0)?;
    if usize::try_from(dimension).ok() != Some(dimensions) {
        return Err(QueryError::internal(
            "semantic TEMP vector dimension is corrupt",
        ));
    }
    let ValueRef::Blob(blob) = row.get_ref(1)? else {
        return Err(QueryError::internal(
            "semantic TEMP vector payload is not a blob",
        ));
    };
    decode_vector(blob, dimensions, control).map(Some)
}

pub(super) fn copy_vector(values: &[f32], control: &ExecutionControl<'_>) -> QueryResult<Vec<f32>> {
    let mut output = Vec::with_capacity(values.len());
    for chunk in values.chunks(256) {
        control.check()?;
        output.extend_from_slice(chunk);
    }
    Ok(output)
}

pub(super) fn load_schema(
    connection: &Connection,
    commit: crate::storage::HashId,
    control: &ExecutionControl<'_>,
) -> QueryResult<(
    crate::storage::SchemaState,
    std::rc::Rc<dyn std::fmt::Debug>,
)> {
    let observer = crate::query::graph::snapshot_observer(control.resources());
    let check = || {
        control.check().map_err(|error| {
            crate::storage::StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(error.sqlite_code.unwrap_or(rusqlite::ffi::SQLITE_ERROR)),
                Some(error.message),
            ))
        })
    };
    crate::storage::SchemaState::load_observed(connection, commit, observer.as_ref(), &check)
        .map_err(Into::into)
}

pub(super) fn push_text(
    buffer: &mut Buffer<String>,
    text: &str,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    buffer.reserve(text.len() as u64)?;
    let text = crate::query::expression::clone_string_controlled(text, Some(control))?;
    buffer.push(text, 0)
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::*;
    use crate::query::QueryErrorKind;

    #[test]
    fn buffer_clear_retains_capacity_and_drop_releases_everything() {
        let resources = QueryResources::new(Some(1_024));
        let mut buffer = Buffer::new(&resources).unwrap();
        buffer.push(String::from("hello"), 5).unwrap();
        assert_eq!(buffer.values.capacity(), 4);
        assert_eq!(resources.used(), (size_of::<String>() * 4 + 5) as u64);
        buffer.clear().unwrap();
        assert_eq!(resources.used(), (size_of::<String>() * 4) as u64);
        for _ in 0..5 {
            buffer.push(String::new(), 0).unwrap();
        }
        assert_eq!(buffer.values.capacity(), 8);
        let (values, memory) = buffer.into_parts();
        assert_eq!(memory.bytes(), (size_of::<String>() * 8) as u64);
        drop(values);
        drop(memory);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn rejected_buffer_growth_preserves_existing_values_and_budget() {
        let resources = QueryResources::new(Some(40));
        let mut buffer = Buffer::new(&resources).unwrap();
        buffer.push(7_u64, 0).unwrap();
        let error = buffer.push(8_u64, 9).unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(buffer.values, [7]);
        assert_eq!(resources.used(), 32);
        drop(buffer);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn buffer_precharges_copy_heaps_and_failed_copy_claim_preserves_budget() {
        let capacity_bytes = (size_of::<String>() * 4) as u64;
        let resources = QueryResources::new(Some(capacity_bytes + 8));
        let mut buffer = Buffer::new(&resources).unwrap();
        buffer.reserve(8).unwrap();
        buffer.push(String::from("original"), 0).unwrap();
        assert_eq!(resources.used(), capacity_bytes + 8);
        assert!(buffer.reserve(8).is_err());
        assert_eq!(resources.used(), capacity_bytes + 8);
        buffer.clear().unwrap();
        assert_eq!(resources.used(), capacity_bytes);
        drop(buffer);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn vector_encoding_and_decoding_keep_distinct_live_claims() {
        let resources = QueryResources::new(Some(16));
        let control = ExecutionControl::new(&resources, &|| false);
        let encoded = encode_vector(&[1.0, -2.0], &control).unwrap();
        let decoded = decode_vector(&encoded.value, 2, &control).unwrap();
        assert_eq!(decoded.value, [1.0, -2.0]);
        assert_eq!(resources.used(), 16);
        assert!(encode_vector(&[0.0], &control).is_err());
        drop(decoded);
        drop(encoded);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn invalid_vector_payloads_release_their_reservations() {
        let resources = QueryResources::new(Some(16));
        let control = ExecutionControl::new(&resources, &|| false);
        assert!(decode_vector(&[0; 3], 1, &control).is_err());
        assert!(decode_vector(&f32::NAN.to_le_bytes(), 1, &control).is_err());
        assert_eq!(resources.used(), 0);
    }
    #[test]
    fn moved_vector_memory_survives_buffer_growth_and_clear_releases_it() {
        let resources = QueryResources::new(Some(1024));
        let mut buffer = Buffer::new(&resources).unwrap();
        buffer.absorb(resources.reserve(128).unwrap()).unwrap();
        buffer.push((7_i64, Vec::<f32>::new()), 0).unwrap();
        let capacity = (std::mem::size_of::<(i64, Vec<f32>)>() * 4) as u64;
        assert_eq!(resources.used(), capacity + 128);
        buffer.clear().unwrap();
        assert_eq!(resources.used(), capacity);
        drop(buffer);
        assert_eq!(resources.used(), 0);
    }
}
