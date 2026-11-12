use super::*;
use std::mem::size_of;

const BUILD_ROWS: usize = 1000;
const HALF_MIB: u64 = 512 * 1024;
const DEFAULT_RESIDENT_TARGET: u64 = 1024 * 1024;

fn temp_catalog(db: &Connection) -> Vec<(String, String)> {
    let mut statement = db
        .prepare("SELECT name,coalesce(sql,'') FROM sqlite_temp_master ORDER BY name")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn retained_binding_floor(rows: usize) -> u64 {
    // BindingValue contains Scalar(Value), so it is at least as large as Value.
    // Each singleton BindingRow pays map_entry_bytes::<BindingValue>(): sixteen
    // inline (String, BindingValue) slots plus 256 bytes. Ignore the enum tag,
    // tuple padding, keys, row, hash entry and buckets for a conservative floor
    // on the existing reservation, rather than an allocator/RSS estimate.
    let entry = 16 * (size_of::<String>() + size_of::<Value>()) as u64 + 256;
    (rows as u64).checked_mul(entry).unwrap()
}

fn successful_grace_read(
    db: &Connection,
    limit: Option<u64>,
    expected: &[Vec<Value>],
    expected_build_rows: u64,
    expected_probe_rows: u64,
    batches: (usize, usize),
) -> bool {
    let head = storage::branch_head(db, "main").unwrap();
    let catalog = temp_catalog(db);
    let scope = ReadScope::new(db);
    let mut options = ExecutionOptions::default();
    options.memory_limit_bytes = limit;
    let prepared = prepare(db, &format!("PROFILE {JOIN}"), BTreeMap::new(), options).unwrap();
    let plan = prepared.physical.explain();
    // Check the very cursor we will pull. An unavailable Hash backend must not
    // turn this bounded test into a million-candidate canonical execution.
    assert!(
        plan.contains("HashJoin"),
        "required actual Grace plan: {plan}"
    );
    assert!(!plan.contains("IndexNestedLoop"), "{plan}");
    assert_eq!(prepared.columns, ["build", "probe", "buildKey", "probeKey"]);
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let result = (|| -> Result<_, QueryError> {
        let mut rows = Vec::new();
        let mut first = true;
        let mut partial = false;
        loop {
            let done = {
                let requested = if first { batches.0 } else { batches.1 };
                first = false;
                let batch = cursor.next_batch(db, requested)?;
                let done = batch.done;
                partial |= requested == 256
                    && !done
                    && !batch.rows.is_empty()
                    && batch.rows.len() < requested;
                rows.extend(batch.rows);
                done
            };
            if done {
                let summary = cursor.complete(db)?;
                let profile = summary
                    .metrics
                    .operator_profile()
                    .expect("actual PROFILE")
                    .to_vec();
                assert_eq!(
                    profile.iter().map(|operator| operator.db_hits).sum::<u64>(),
                    summary.metrics.db_hits,
                    "exact storage accounting: {plan}; {profile:?}"
                );
                return Ok((rows, profile, partial));
            }
        }
    })();
    drop(cursor);
    assert_eq!(
        resources.used(),
        0,
        "complete/error must release Grace leases"
    );
    if let Some(limit) = limit {
        assert!(
            resources.peak() <= limit,
            "query peak {} > {limit}",
            resources.peak()
        );
    }
    let (rows, profile, partial) =
        result.unwrap_or_else(|error| panic!("successful Grace read: {plan}; {error:?}"));
    assert_bag(&rows, expected);
    let hashes = profile
        .iter()
        .filter(|operator| operator.operator == "HashJoin")
        .collect::<Vec<_>>();
    assert_eq!(
        hashes.len(),
        1,
        "one selected equijoin: {plan}; {profile:?}"
    );
    assert_grace_operator(
        hashes[0],
        limit,
        expected.len(),
        expected_build_rows,
        expected_probe_rows,
    );
    drop(scope);
    assert!(db.is_autocommit());
    assert_eq!(storage::branch_head(db, "main").unwrap(), head);
    assert_eq!(temp_catalog(db), catalog);
    partial
}

fn assert_grace_operator(
    operator: &OperatorRuntimeMetrics,
    limit: Option<u64>,
    expected_rows: usize,
    expected_build_rows: u64,
    expected_probe_rows: u64,
) {
    assert_eq!(operator.rows, expected_rows as u64);
    let hash = operator
        .hash
        .as_ref()
        .expect("actual Hash runtime counters");
    let actual_inputs = (hash.build_rows, hash.probe_rows);
    assert!(
        actual_inputs == (expected_build_rows, expected_probe_rows)
            || actual_inputs == (expected_probe_rows, expected_build_rows),
        "the cost-selected orientation consumes both complete inputs: {hash:?}"
    );
    assert_eq!(
        hash.build_rows + hash.probe_rows,
        expected_build_rows + expected_probe_rows
    );
    assert!(
        hash.spill_partitions > 1,
        "real repartition beyond the root: {hash:?}"
    );
    assert!(
        hash.spill_batches > 0,
        "real completed spill batches: {hash:?}"
    );
    assert!(
        hash.spill_vm_steps > 0,
        "real SQLite spill statements: {hash:?}"
    );
    assert!(hash.spill_encoded_bytes > 0, "real typed payload: {hash:?}");
    assert!(
        hash.temp_allocated_bytes > 0,
        "real allocated TEMP pages: {hash:?}"
    );
    assert!(
        hash.memory_peak_bytes > 0,
        "real Hash reservations: {hash:?}"
    );
    if let Some(limit) = limit {
        assert!(hash.memory_peak_bytes <= limit, "{hash:?}; limit={limit}");
    }
    if expected_build_rows == BUILD_ROWS as u64 && expected_probe_rows == 2000 {
        // Either build orientation retains at least 999 identical keys.
        // Repartition cannot divide that hot key, whose reservation floor
        // cannot fit into even two budgets.
        assert!(retained_binding_floor(BUILD_ROWS - 1) > 2 * HALF_MIB);
        assert!(
            hash.spill_batches >= 3,
            "finite multi-chunk hot-key work: {hash:?}"
        );
    }
}

#[test]
fn uniform_grace_spill_succeeds_with_default_target_and_512k_budget() {
    let db = empty_db();
    let keys = (1..=BUILD_ROWS as i64)
        .map(Value::Integer)
        .collect::<Vec<_>>();
    let build = seed(&db, "Build", keys.clone());
    let probe = seed(&db, "Probe", keys);
    constraints(&db, "INTEGER", "INTEGER");
    let expected = expected_pairs(
        &build,
        &probe,
        &(0..BUILD_ROWS).map(|seq| (seq, seq)).collect::<Vec<_>>(),
    );
    assert_eq!(expected.len(), BUILD_ROWS);
    assert!(retained_binding_floor(BUILD_ROWS) > DEFAULT_RESIDENT_TARGET);
    assert!(retained_binding_floor(BUILD_ROWS) > HALF_MIB);
    // None exercises the implementation's existing 1 MiB soft resident target;
    // the second read additionally proves a build larger than a hard query cap.
    successful_grace_read(
        &db,
        None,
        &expected,
        BUILD_ROWS as u64,
        BUILD_ROWS as u64,
        (3, 3),
    );
    successful_grace_read(
        &db,
        Some(HALF_MIB),
        &expected,
        BUILD_ROWS as u64,
        BUILD_ROWS as u64,
        (3, 3),
    );
}

fn hot_fixture() -> (Connection, Vec<Vec<Value>>) {
    let db = empty_db();
    let mut keys = vec![Value::Integer(1); BUILD_ROWS - 1];
    keys.push(Value::Integer(2));
    let build = seed(&db, "Build", keys);
    let mut keys = vec![Value::Integer(1), Value::Integer(1)];
    keys.resize(2000, Value::Integer(2));
    let probe = seed(&db, "Probe", keys);
    constraints(&db, "INTEGER", "INTEGER");
    let pairs = (0..BUILD_ROWS - 1)
        .flat_map(|a| [(a, 0), (a, 1)])
        .chain((2..2000).map(|b| (BUILD_ROWS - 1, b)))
        .collect::<Vec<_>>();
    let expected = expected_pairs(&build, &probe, &pairs);
    assert_eq!(expected.len(), 3996);
    assert!(retained_binding_floor(BUILD_ROWS - 1) > 2 * HALF_MIB);
    (db, expected)
}

#[test]
fn hot_key_grace_spill_preserves_multiplicity_across_more_than_two_chunks() {
    let (db, expected) = hot_fixture();
    // Either legitimate build choice exercises the bounded hot-key path.
    successful_grace_read(
        &db,
        Some(HALF_MIB),
        &expected,
        BUILD_ROWS as u64,
        2000,
        (3, 3),
    );
}

#[test]
fn grace_returns_budget_bounded_partial_batches_for_a_256_row_request() {
    let (db, expected) = hot_fixture();
    // The caller keeps the acceptance protocol: first one row, then requests
    // 256. A loaded hot chunk must share the same 512 KiB ledger with output;
    // max_rows is an upper bound, not a requirement to fill before yielding.
    assert!(successful_grace_read(
        &db,
        Some(HALF_MIB),
        &expected,
        BUILD_ROWS as u64,
        2000,
        (1, 256),
    ));
}
