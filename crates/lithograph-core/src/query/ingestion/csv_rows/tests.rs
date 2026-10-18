use std::cell::Cell;
use std::io::Cursor;
use std::rc::Rc;

use super::*;
use crate::query::resources::{QueryResources, allocation_bytes};

struct CountedReader {
    input: Cursor<Vec<u8>>,
    read: Rc<Cell<usize>>,
}

impl Read for CountedReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let count = self.input.read(output)?;
        self.read.set(self.read.get() + count);
        Ok(count)
    }
}

fn reader(bytes: Vec<u8>) -> (Box<dyn Read>, Rc<Cell<usize>>) {
    let read = Rc::new(Cell::new(0));
    (
        Box::new(CountedReader {
            input: Cursor::new(bytes),
            read: read.clone(),
        }),
        read,
    )
}

#[test]
fn oversized_physical_line_is_refused_before_reading_or_allocating_it_all() {
    let resources = QueryResources::new(Some(64 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let (input, read) = reader(vec![120; 1024 * 1024]);
    let mut rows = CsvRows::new(input, ',', &control).expect("CSV reader");
    let error = rows
        .next_record(&control)
        .err()
        .expect("line must exceed budget");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert!(
        read.get() < 128 * 1024,
        "whole hostile line must not be read first"
    );
    assert_eq!(resources.used(), READER_BYTES as u64);
    drop(rows);
    assert_eq!(resources.used(), 0);
    assert!(resources.peak() <= 64 * 1024);
}

#[test]
fn multiline_field_is_refused_before_accumulating_the_full_record() {
    let resources = QueryResources::new(Some(64 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut csv = String::from("\"");
    for _ in 0..16_384 {
        csv.push_str("abcdefghijklmnopqrstuvwxyz\n");
    }
    csv.push_str("\",tail\n");
    let (input, read) = reader(csv.into_bytes());
    let mut rows = CsvRows::new(input, ',', &control).expect("CSV reader");
    let error = rows
        .next_record(&control)
        .err()
        .expect("quoted field must exceed budget");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert!(read.get() < 128 * 1024);
    assert_eq!(resources.used(), READER_BYTES as u64);
}

#[test]
fn cancellation_is_observed_while_reading_a_single_large_line() {
    let resources = QueryResources::new(None);
    let checks = Cell::new(0);
    let interrupted = || {
        let next = checks.get() + 1;
        checks.set(next);
        next >= 12
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    let (input, read) = reader(vec![120; 1024 * 1024]);
    let mut rows = CsvRows::new(input, ',', &control).expect("CSV reader");
    let error = rows
        .next_record(&control)
        .err()
        .expect("cancelled line read");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert!(read.get() <= 32 * 1024);
    assert_eq!(resources.used(), READER_BYTES as u64);
}

#[test]
fn record_leases_cover_capacity_and_streaming_releases_each_record() {
    let resources = QueryResources::new(Some(64 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let (input, _) = reader("a,b,c\n".repeat(10_000).into_bytes());
    let mut rows = CsvRows::new(input, ',', &control).expect("CSV reader");
    let mut count = 0;
    while let Some(record) = rows.next_record(&control).expect("record") {
        let actual = allocation_bytes::<String>(record.fields.capacity()).expect("field slots")
            + record
                .fields
                .iter()
                .map(|field| field.capacity() as u64)
                .sum::<u64>();
        assert_eq!(record.memory.bytes(), actual);
        count += 1;
        drop(record);
        assert_eq!(resources.used(), READER_BYTES as u64);
    }
    assert_eq!(count, 10_000);
}

#[test]
fn utf8_chunk_boundaries_quotes_and_embedded_bom_preserve_csv_values() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let text = "界".repeat(5_000);
    let csv = format!("\u{feff}\"{text}\n\u{feff}second\",\"a\"\"b\"\n");
    let (input, _) = reader(csv.into_bytes());
    let mut rows = CsvRows::new(input, ',', &control).expect("CSV reader");
    let record = rows
        .next_record(&control)
        .expect("record")
        .expect("CSV data");
    assert_eq!(
        record.fields,
        vec![format!("{text}\n\u{feff}second"), "a\"b".to_owned()]
    );
    drop(record);
    assert!(rows.next_record(&control).expect("EOF").is_none());
}
