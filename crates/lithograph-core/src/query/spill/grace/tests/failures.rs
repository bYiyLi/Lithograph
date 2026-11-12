use super::*;

#[test]
fn private_temp_readonly_is_resource_and_cleans_up() {
    let resources = fixture_resources(&value_row(0, 0));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut store = GracePartitions::create(resources.clone(), &control).unwrap();
    store
        .connection()
        .unwrap()
        .pragma_update(None, "query_only", true)
        .unwrap();
    let direct = store
        .connection()
        .unwrap()
        .execute("CREATE TABLE main.forbidden(id INTEGER)", [])
        .unwrap_err();
    let canonical = QueryError::from(direct);
    assert_eq!(canonical.kind, QueryErrorKind::Io);
    assert_eq!(canonical.sqlite_code, Some(rusqlite::ffi::SQLITE_READONLY));
    let row = value_row(1, 0);
    let memory = control
        .reserve(binding_row_bytes(&row, &control).unwrap())
        .unwrap();
    let error = store
        .append_reserved(
            PartitionId::ROOT,
            HashSide::Build,
            [0; 32],
            row,
            memory,
            &control,
        )
        .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert!(error.message.contains("SQLite code 8"));
    assert_closed(&store);
    drop(store);
    assert_eq!(resources.used(), 0);
}

#[test]
fn private_temp_mapping_preserves_unrelated_sqlite_failures() {
    for code in [
        rusqlite::ffi::SQLITE_READONLY,
        rusqlite::ffi::SQLITE_CANTOPEN,
        rusqlite::ffi::SQLITE_PERM,
    ] {
        let original = QueryError::from(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some("private path/value must not escape".to_owned()),
        ));
        let mapped = private_temp_error(original);
        assert_eq!(mapped.kind, QueryErrorKind::Resource);
        assert_eq!(mapped.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
        assert!(mapped.message.contains(&format!("SQLite code {code}")));
        assert!(!mapped.message.contains("path/value"));
    }
    for (code, kind) in [
        (rusqlite::ffi::SQLITE_IOERR, QueryErrorKind::Io),
        (rusqlite::ffi::SQLITE_CORRUPT, QueryErrorKind::Storage),
        (rusqlite::ffi::SQLITE_INTERRUPT, QueryErrorKind::Interrupted),
        (rusqlite::ffi::SQLITE_BUSY, QueryErrorKind::Busy),
        (rusqlite::ffi::SQLITE_LOCKED, QueryErrorKind::Busy),
        (rusqlite::ffi::SQLITE_AUTH, QueryErrorKind::AccessDenied),
        (rusqlite::ffi::SQLITE_FULL, QueryErrorKind::Resource),
    ] {
        let original = QueryError::from(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some("unchanged".to_owned()),
        ));
        let mapped = private_temp_error(original);
        assert_eq!(mapped.kind, kind);
        assert_eq!(mapped.sqlite_code, Some(code));
        assert_eq!(mapped.message, "unchanged");
    }
}

#[test]
fn resource_limited_read_returns_a_partial_page_then_retries_the_same_next_row() {
    let resources = fixture_resources(&value_row(0, 512));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut store = GracePartitions::create(resources.clone(), &control).unwrap();
    for id in 0..2 {
        append(
            &mut store,
            HashSide::Build,
            [0; 32],
            value_row(id, 512),
            &control,
        );
    }
    let encoded: String = store
        .connection()
        .unwrap()
        .query_row(
            &format!("SELECT binding_json FROM temp.{} WHERE seq=0", store.table),
            [],
            |row| row.get(0),
        )
        .unwrap();
    let allowance = decoding_bytes(&encoded, &control).unwrap();
    let row_bytes = binding_row_bytes(&value_row(0, 512), &control).unwrap();
    let available = allowance + allocation_bytes::<SpilledBinding>(4).unwrap() + row_bytes / 2;
    let hold = resources
        .reserve(resources.limit().unwrap() - resources.used() - available)
        .unwrap();
    let page = store
        .read_batch_reserved(
            PartitionId::ROOT,
            HashSide::Build,
            -1,
            limit(256, u64::MAX),
            &control,
        )
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page.after, 0);
    drop(page);
    drop(hold);
    let mut page = store
        .read_batch_reserved(
            PartitionId::ROOT,
            HashSide::Build,
            0,
            limit(256, u64::MAX),
            &control,
        )
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page.next().unwrap().row, value_row(1, 512));
    drop(page);
    store.abort().unwrap();
    assert_eq!(resources.used(), 0);
}

#[test]
fn first_row_resource_refusal_keeps_the_store_retriable() {
    let resources = fixture_resources(&value_row(0, 512));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut store = seeded_build_store(&resources, &control, [0; 32], value_row(1, 512));
    let hold = resources
        .reserve(resources.limit().unwrap() - resources.used() - 1)
        .unwrap();
    let error = store
        .read_batch_reserved(
            PartitionId::ROOT,
            HashSide::Build,
            -1,
            limit(1, 1),
            &control,
        )
        .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert!(store.connection.is_some());
    drop(hold);
    let page = store
        .read_batch_reserved(
            PartitionId::ROOT,
            HashSide::Build,
            -1,
            limit(1, 1),
            &control,
        )
        .unwrap();
    assert_eq!(page.len(), 1);
    drop(page);
    store.abort().unwrap();
    assert_eq!(resources.used(), 0);
}

#[test]
fn malformed_digest_or_payload_closes_private_connection() {
    for assignment in [
        "digest=x'00'",
        "digest='text'",
        "binding_json='{'",
        "binding_json=x'00'",
    ] {
        let resources = fixture_resources(&value_row(0, 0));
        let control = ExecutionControl::new(&resources, &|| false);
        let mut store = seeded_build_store(&resources, &control, [0; 32], value_row(1, 0));
        store
            .connection()
            .unwrap()
            .execute(&format!("UPDATE temp.{} SET {assignment}", store.table), [])
            .unwrap();
        let error = store
            .read_batch_reserved(
                PartitionId::ROOT,
                HashSide::Build,
                -1,
                limit(256, 64 * 1024),
                &control,
            )
            .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Internal);
        assert_closed(&store);
        drop(store);
        assert_eq!(resources.used(), 0);
    }
}

#[test]
fn interrupted_repartition_and_retirement_do_not_leave_reusable_partial_state() {
    for retire in [false, true] {
        let resources = fixture_resources(&value_row(0, 0));
        let control = ExecutionControl::new(&resources, &|| false);
        let mut store = GracePartitions::create(resources.clone(), &control).unwrap();
        for id in 0..512 {
            append(
                &mut store,
                HashSide::Probe,
                [0; 32],
                value_row(id, 0),
                &control,
            );
        }
        let calls = std::cell::Cell::new(0);
        let interrupted = || {
            calls.set(calls.get() + 1);
            calls.get() >= 10
        };
        let cancel = ExecutionControl::new(&resources, &interrupted);
        let error = if retire {
            store.retire_probe_run(&cancel).unwrap_err()
        } else {
            store.repartition(PartitionId::ROOT, &cancel).unwrap_err()
        };
        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert_closed(&store);
        assert!(calls.get() >= 10);
        if retire {
            assert!(store.counters().retired_probe_batches > 0);
        } else {
            assert!(store.counters().repartition_rows > 0);
        }
        drop(store);
        assert_eq!(resources.used(), 0);
    }
}

#[test]
fn append_lease_mismatch_and_encoding_pressure_release_moved_input() {
    for mismatch in [false, true] {
        let resources = QueryResources::new(Some(16 * 1024));
        let control = ExecutionControl::new(&resources, &|| false);
        let mut store = GracePartitions::create(resources.clone(), &control).unwrap();
        let other = QueryResources::default();
        let mut row = BindingRow::default();
        row.insert("n".to_owned(), BindingValue::Node(1));
        let owner = if mismatch { &other } else { &resources };
        let error = append_build_reserved(&mut store, row, owner, &control).unwrap_err();
        assert_eq!(
            error.kind,
            if mismatch {
                QueryErrorKind::Internal
            } else {
                QueryErrorKind::Resource
            }
        );
        assert_closed(&store);
        assert_eq!(other.used(), 0);
        drop(store);
        assert_eq!(resources.used(), 0);
    }
}

#[test]
fn private_sqlite_full_preserves_resource_code_and_closes_store() {
    let resources = QueryResources::new(Some(128 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut store = GracePartitions::create(resources.clone(), &control).unwrap();
    let pages: i64 = store
        .connection()
        .unwrap()
        .query_row("PRAGMA temp.page_count", [], |row| row.get(0))
        .unwrap();
    store
        .connection()
        .unwrap()
        .pragma_update(Some("temp"), "max_page_count", pages)
        .unwrap();
    let mut failure = None;
    for id in 0..128 {
        // Keep the original 128KiB quota, leaving enough for one fully
        // accounted row and its encoding after the private-cache prepayment.
        let row = value_row(id, 0);
        if let Err(error) = append_build_reserved(&mut store, row, &resources, &control) {
            failure = Some(error);
            break;
        }
    }
    let error = failure.expect("bounded TEMP page count must really fail");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_FULL));
    assert_closed(&store);
    drop(store);
    assert_eq!(resources.used(), 0);
}

#[test]
fn metadata_contradiction_and_sequence_overflow_fail_closed() {
    let resources = fixture_resources(&value_row(0, 0));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut store = seeded_build_store(&resources, &control, [0; 32], value_row(1, 0));
    store
        .connection()
        .unwrap()
        .execute(&format!("DELETE FROM temp.{}", store.table), [])
        .unwrap();
    let error = store.repartition(PartitionId::ROOT, &control).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Internal);
    assert_closed(&store);
    drop(store);
    let mut store = GracePartitions::create(resources.clone(), &control).unwrap();
    store.sequence = i64::MAX;
    let row = value_row(1, 0);
    let error = append_build_reserved(&mut store, row, &resources, &control).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(store.counters().written_rows, [0; 2]);
    assert_closed(&store);
    drop(store);
    assert_eq!(resources.used(), 0);
}
