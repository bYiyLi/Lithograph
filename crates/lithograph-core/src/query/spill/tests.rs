use super::*;
use crate::query::resources::{owned_row_bytes, row_bytes};

#[test]
fn sort_spills_on_byte_pressure_and_merges_with_bounded_heads() {
    let connection = open_spill_connection().expect("spill connection");
    let resources = QueryResources::new(Some(64 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut spill =
        SortSpill::create_with_resources(&connection, false, vec![false], resources.clone())
            .expect("sort spill");
    for ordinal in 0..2048_i64 {
        spill
            .push(
                &connection,
                vec![Value::Integer(ordinal)],
                vec![Value::Integer(ordinal % 17)],
                &control,
            )
            .expect("budgeted sort row");
    }
    assert!(
        spill.run > 2,
        "byte budget must produce multiple early runs"
    );
    let total = spill.finish(&connection, &control).expect("sort merge");
    assert_eq!(total, 2048);
    spill
        .cleanup_aux(&connection)
        .expect("sort auxiliary cleanup");
    let mut output = spill.into_output(total);
    assert_eq!(
        resources.used(),
        0,
        "sort must release chunks and merge heads"
    );
    let mut expected = (0..2048_i64).collect::<Vec<_>>();
    expected.sort_by_key(|ordinal| ordinal % 17);
    for ordinal in expected {
        let (seq, row, memory) =
            read_output_row_with_resources(&connection, &output.table, output.after, &control)
                .expect("read sorted output")
                .expect("sorted row");
        assert_eq!(row, vec![Value::Integer(ordinal)]);
        assert_eq!(
            memory.bytes(),
            owned_row_bytes(&row).expect("owning output bytes")
        );
        assert!(resources.used() > 0);
        output.after = seq;
        drop(row);
        drop(memory);
        assert_eq!(resources.used(), 0);
    }
    drop_temp_table(&connection, &output.table).expect("sort output cleanup");
    assert!(resources.peak() <= 64 * 1024);
}

#[test]
fn sort_moves_existing_payload_reservations_without_double_charging() {
    let connection = open_spill_connection().expect("spill connection");
    let row = vec![Value::String("x".repeat(512))];
    let keys = vec![Value::Integer(1)];
    let sizing_resources = QueryResources::default();
    let sizing_control = ExecutionControl::new(&sizing_resources, &|| false);
    let payload = sort_payload_bytes(&row, &keys, &sizing_control).expect("payload bytes");
    let limit = payload
        + serialization_bytes(payload).expect("scratch bytes") * 2
        + allocation_bytes::<SortRecord>(1).expect("sort slot")
        + 128;
    let resources = QueryResources::new(Some(limit));
    let control = ExecutionControl::new(&resources, &|| false);
    let row_memory = resources
        .reserve(row_bytes(&row).expect("row bytes"))
        .expect("row lease");
    let key_memory = resources
        .reserve(row_bytes(&keys).expect("key bytes"))
        .expect("key lease");
    let mut spill =
        SortSpill::create_with_resources(&connection, false, vec![false], resources.clone())
            .expect("sort spill");
    spill
        .push_reserved(&connection, row, keys, row_memory, key_memory, &control)
        .expect("move must fit without charging a second payload");
    assert_eq!(spill.ordinal, 1);
    spill.abort(&connection).expect("sort abort");
    drop(spill);
    assert_eq!(resources.used(), 0);
}

#[test]
fn spill_serialization_refuses_before_inserting_and_releases_claim() {
    let connection = open_spill_connection().expect("spill connection");
    let resources = QueryResources::new(Some(16 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut spill =
        RowSpill::create_with_resources(&connection, resources.clone()).expect("row spill");
    let error = spill
        .push(
            &connection,
            &[Value::String("x".repeat(16 * 1024))],
            &control,
        )
        .expect_err("encoding scratch must exceed the limit");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert_eq!(spill.sequence, 0);
    assert_eq!(resources.used(), 0);
    drop_temp_table(&connection, &spill.table).expect("row spill cleanup");
}

#[test]
fn binding_batches_hold_live_memory_and_release_between_reads() {
    let connection = open_spill_connection().expect("spill connection");
    let resources = QueryResources::new(Some(1024 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut spill =
        BindingSpill::create_with_resources(&connection, resources.clone()).expect("binding spill");
    for ordinal in 0..512_i64 {
        let mut row = BindingRow::default();
        row.insert(
            "value".to_owned(),
            BindingValue::Scalar(Value::String(format!("{ordinal}:{}", "x".repeat(64)))),
        );
        spill
            .push(&connection, &row, &control)
            .expect("binding spill row");
    }
    let mut after = -1;
    loop {
        let (batch, memory) = spill
            .read_batch_reserved(&connection, after, 16, &control)
            .expect("budgeted batch read");
        if batch.is_empty() {
            break;
        }
        assert!(resources.used() >= binding_row_bytes(&batch[0].1, &control).expect("row bytes"));
        after = batch.last().expect("batch tail").0;
        drop(batch);
        drop(memory);
        assert_eq!(
            resources.used(),
            0,
            "consumed batches must release the budget"
        );
    }
    assert_eq!(after, 511);
    assert_eq!(resources.used(), 0);
    spill.abort(&connection).expect("binding cleanup");
}

#[test]
fn cancelled_merge_releases_heads_and_removes_scratch_table() {
    let connection = open_spill_connection().expect("spill connection");
    let resources = QueryResources::new(Some(64 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut spill =
        SortSpill::create_with_resources(&connection, false, vec![false], resources.clone())
            .expect("sort spill");
    for ordinal in 0..2048_i64 {
        spill
            .push(
                &connection,
                vec![Value::Integer(ordinal)],
                vec![Value::Integer(ordinal)],
                &control,
            )
            .expect("sort row");
    }
    spill
        .flush(&connection, &control)
        .expect("finish pending run before merge cancellation");
    let checks = std::cell::Cell::new(0);
    let interrupted = || {
        let count = checks.get() + 1;
        checks.set(count);
        count >= 8
    };
    let interrupted_control = ExecutionControl::new(&resources, &interrupted);
    let error = spill
        .finish(&connection, &interrupted_control)
        .expect_err("cancelled merge");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(resources.used(), 0);
    let scratch = connection
        .query_row(
            "SELECT count(*) FROM temp.sqlite_schema WHERE name LIKE '_lithograph_query_merge_%'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("scratch inventory");
    assert_eq!(scratch, 0);
    spill.abort(&connection).expect("sort cleanup");
}

#[test]
fn binding_spill_uses_file_backed_temp_pages() {
    let connection = open_spill_connection().expect("spill connection");
    let temp_store = connection
        .query_row("PRAGMA temp_store", [], |row| row.get::<_, i64>(0))
        .expect("temp_store");
    assert_eq!(temp_store, 1, "spill connection must force FILE temp store");

    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut spill =
        BindingSpill::create_with_resources(&connection, resources.clone()).expect("binding spill");
    let payload = "x".repeat(1024);
    for ordinal in 0..4096_i64 {
        let mut row = BindingRow::default();
        row.insert(
            "payload".to_owned(),
            BindingValue::Scalar(Value::String(format!("{ordinal}:{payload}"))),
        );
        spill.push(&connection, &row, &control).expect("spill row");
    }

    let pages = connection
        .query_row("PRAGMA temp.page_count", [], |row| row.get::<_, i64>(0))
        .expect("temp page count");
    let page_size = connection
        .query_row("PRAGMA temp.page_size", [], |row| row.get::<_, i64>(0))
        .expect("temp page size");
    let pages = u64::try_from(pages).expect("non-negative temp page count");
    let page_size = u64::try_from(page_size).expect("non-negative temp page size");
    let temp_bytes = pages.saturating_mul(page_size);
    eprintln!("phase15 TEMP spill logical bytes: {temp_bytes}");
    assert!(
        temp_bytes >= 4 * 1024 * 1024,
        "spill did not allocate the expected file-backed TEMP pages: {temp_bytes}"
    );
}

#[test]
fn byte_budget_spill_records_private_temp_and_early_close_cleans_output() {
    let connection = open_spill_connection().unwrap();
    let resources = QueryResources::new(Some(64 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut spill =
        SortSpill::create_with_resources(&connection, false, vec![true], resources.clone())
            .unwrap();
    for ordinal in 0..2048_i64 {
        spill
            .push(
                &connection,
                vec![Value::Integer(ordinal)],
                vec![Value::Integer(ordinal)],
                &control,
            )
            .unwrap();
    }
    assert!(
        spill.run > 2,
        "byte pressure must flush before row threshold"
    );
    let total = spill.finish(&connection, &control).unwrap();
    spill.cleanup_aux(&connection).unwrap();
    let output = spill.into_output(total);
    let pages: u64 = connection
        .query_row("PRAGMA temp.page_count", [], |row| row.get::<_, i64>(0))
        .unwrap()
        .try_into()
        .unwrap();
    let page_size: u64 = connection
        .query_row("PRAGMA temp.page_size", [], |row| row.get::<_, i64>(0))
        .unwrap()
        .try_into()
        .unwrap();
    let private_temp_bytes = pages * page_size;
    eprintln!("phase19 private sort TEMP logical bytes before early close: {private_temp_bytes}");
    assert!(private_temp_bytes >= 4096);
    let (_, first, memory) =
        read_output_row_with_resources(&connection, &output.table, -1, &control)
            .unwrap()
            .unwrap();
    assert_eq!(first, vec![Value::Integer(2047)]);
    drop(first);
    drop(memory);
    drop_temp_table(&connection, &output.table).unwrap();
    let tables: i64 = connection
        .query_row(
            "SELECT count(*) FROM temp.sqlite_schema WHERE name LIKE '_lithograph_query_%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "closing output must remove private TEMP tables");
    assert_eq!(resources.used(), 0);
    drop(connection);
    assert_eq!(resources.used(), 0);
}

#[test]
fn sort_flushes_before_next_producer_projection_runs_out_of_budget() {
    let connection = open_spill_connection().unwrap();
    let limit = 4 * 1024 * 1024;
    let resources = QueryResources::new(Some(limit));
    let control = ExecutionControl::new(&resources, &|| false);
    let upstream = control
        .reserve(allocation_bytes::<Value>(4_000).unwrap())
        .unwrap();
    let mut spill =
        SortSpill::create_with_resources(&connection, false, vec![true], resources.clone())
            .unwrap();
    for ordinal in 0..512_i64 {
        // A producer needs temporary bindings/projections/aliases before it can
        // give its owned row to push. Flush must leave this room in advance.
        let producer = control
            .reserve(80 * 1024)
            .expect("next producer must fit before push");
        spill
            .push(
                &connection,
                vec![Value::Integer(ordinal), Value::String("x".repeat(8192))],
                vec![Value::Integer(ordinal)],
                &control,
            )
            .unwrap();
        drop(producer);
    }
    assert!(
        spill.run > 0,
        "byte pressure should flush before materializing the next row"
    );
    let total = spill.finish(&connection, &control).unwrap();
    assert_eq!(total, 512);
    spill.cleanup_aux(&connection).unwrap();
    let mut output = spill.into_output(total);
    for ordinal in (0..512_i64).rev() {
        let (sequence, row, memory) =
            read_output_row_with_resources(&connection, &output.table, output.after, &control)
                .unwrap()
                .unwrap();
        assert_eq!(row[0], Value::Integer(ordinal));
        output.after = sequence;
        drop(row);
        drop(memory);
    }
    drop_temp_table(&connection, &output.table).unwrap();
    drop(upstream);
    assert_eq!(resources.used(), 0);
    assert!(resources.peak() <= limit);
}
