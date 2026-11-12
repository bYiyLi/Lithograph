use super::*;

#[test]
fn grace_transaction_remains_private_until_finish_abort_or_drop() {
    let host = Connection::open_in_memory().unwrap();
    host.execute_batch("CREATE TABLE owned(value INTEGER); BEGIN; INSERT INTO owned VALUES(7)")
        .unwrap();
    for exit in 0..3 {
        let resources = fixture_resources(&value_row(0, 0));
        let control = ExecutionControl::new(&resources, &|| false);
        let mut store = seeded_build_store(&resources, &control, [0; 32], value_row(1, 0));
        assert!(!store.connection().unwrap().is_autocommit());
        append(
            &mut store,
            HashSide::Probe,
            [0; 32],
            value_row(2, 0),
            &control,
        );
        store.repartition(PartitionId::ROOT, &control).unwrap();
        store.retire_probe_run(&control).unwrap();
        assert!(!store.connection().unwrap().is_autocommit());
        let partition = store.route_digest(&[0; 32]).unwrap();
        let mut page = store
            .read_batch_reserved(partition, HashSide::Build, -1, limit(256, 4096), &control)
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page.next().unwrap().row, value_row(1, 0));
        drop(page);
        match exit {
            0 => {
                let counters = store.finish().unwrap();
                assert!(counters.temp_logical_bytes_peak > 0);
                assert!(counters.sqlite_vm_steps > 0);
            }
            1 => store.abort().unwrap(),
            _ => drop(store),
        }
        assert_eq!(resources.used(), 0);
        assert!(
            !host.is_autocommit(),
            "private close must not finish the host transaction"
        );
        assert_eq!(
            host.query_row("SELECT value FROM owned", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            7
        );
    }
    host.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        host.query_row("SELECT count(*) FROM owned", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn sqlite_toobig_during_read_closes_owner_instead_of_retrying_local_budget() {
    let resources = fixture_resources(&value_row(0, 0));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut store = seeded_build_store(&resources, &control, [0; 32], value_row(1, 0));
    // This disposable derived fixture forces a real SQLite step error, not a
    // constructed QueryError or an allocation of a giant Rust value. The blob
    // length exceeds SQLite's signed 32-bit maximum even if MAX_LENGTH is raised.
    let sql = format!(
        "DROP TABLE temp.{0}; CREATE TEMP VIEW {0} AS SELECT \
         0 AS partition,0 AS side,0 AS seq,zeroblob(32) AS digest,\
         zeroblob(2147483648) AS binding_json",
        store.table
    );
    store.connection().unwrap().execute_batch(&sql).unwrap();
    let error = store
        .read_batch_reserved(
            PartitionId::ROOT,
            HashSide::Build,
            -1,
            limit(256, 4096),
            &control,
        )
        .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert_closed(&store);
    assert!(store.stats(PartitionId::ROOT).is_err());
    drop(store);
    assert_eq!(resources.used(), 0);
}
