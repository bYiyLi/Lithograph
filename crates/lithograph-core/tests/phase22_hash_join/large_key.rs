//! A fixed CP-backed boundary: the same String bag changes only NodeId order.
//! This fixed group currently selects a legitimate NestedLoop fallback. It
//! proves single-value capacity and complete empty-bag semantics, not successful
//! Hash preflush; a future safe physical choice need not keep the same name.
use super::*;
use lithograph_core::query::HashJoinRuntimeMetrics;

const N: usize = 128;
const LARGE: usize = 512 * 1024;
const LIMIT: u64 = 5 * 256 * 1024;

#[derive(Debug)]
struct Evidence {
    plan: String,
    peak: u64,
    result: Result<(Vec<Vec<Value>>, Vec<HashJoinRuntimeMetrics>), QueryError>,
}

fn run(db: &Connection, text: &str, params: BTreeMap<String, Value>) -> Evidence {
    let _scope = ReadScope::new(db);
    let mut options = ExecutionOptions::default();
    options.memory_limit_bytes = Some(LIMIT);
    let prepared = prepare(db, text, params, options).unwrap();
    let plan = prepared.physical.explain();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let result = (|| {
        let mut rows = Vec::new();
        loop {
            let batch = cursor.next_batch(db, 3)?;
            let done = batch.done;
            rows.extend(batch.rows);
            if done {
                let summary = cursor.complete(db)?;
                let profile = summary.metrics.operator_profile().unwrap();
                assert_eq!(
                    profile.iter().map(|op| op.db_hits).sum::<u64>(),
                    summary.metrics.db_hits
                );
                let hashes = profile.iter().filter_map(|op| op.hash).collect();
                return Ok((rows, hashes));
            }
        }
    })();
    drop(cursor);
    assert_eq!(
        resources.used(),
        0,
        "success/error releases all engine leases"
    );
    Evidence {
        plan,
        peak: resources.peak(),
        result,
    }
}

fn fixture(big_first: bool) -> (Connection, Value) {
    let db = empty_db();
    drop(seed(
        &db,
        "Build",
        (0..N)
            .map(|n| Value::String(format!("outer-{n:03}")))
            .collect(),
    ));
    let mut keys = (0..N - 1)
        .map(|n| Value::String(format!("inner-{n:03}")))
        .collect::<Vec<_>>();
    keys.insert(
        if big_first { 0 } else { N - 1 },
        Value::String("x".repeat(LARGE)),
    );
    let inner = seed(&db, "Probe", keys);
    let id = inner[if big_first { 0 } else { N - 1 }].id.clone();
    drop(inner);
    constraints(&db, "STRING", "STRING"); // Includes a complete checkpoint at latest head.
    assert!(db.is_autocommit());
    (db, id)
}

fn check(big_first: bool) {
    let (db, id) = fixture(big_first);
    let head = storage::branch_head(&db, "main").unwrap();
    let solo = run(
        &db,
        "PROFILE MATCH(n:Probe) WHERE elementId(n)=$id RETURN size(n.k)",
        BTreeMap::from([("id".to_owned(), id)]),
    );
    assert!(solo.peak <= LIMIT, "single-key working set exceeded quota");
    assert_eq!(solo.result.unwrap().0, [vec![Value::Integer(LARGE as i64)]]);
    let joined = run(&db, &format!("PROFILE {JOIN}"), BTreeMap::new());
    assert!(
        joined.peak <= LIMIT,
        "complete join working set exceeded quota"
    );
    let (rows, hashes) = joined
        .result
        .unwrap_or_else(|error| panic!("single-key fit; fixed-order join failed: {error:?}"));
    assert!(
        rows.is_empty(),
        "the two complete String domains are disjoint"
    );
    if joined.plan.contains("HashJoin") {
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0].build_rows, N as u64);
        assert_eq!(hashes[0].probe_rows, N as u64);
    } else {
        assert!(
            hashes.is_empty(),
            "legal cheaper fallback never reports Hash work"
        );
    }
    assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
}

#[test]
fn large_string_key_first_preserves_complete_empty_bag() {
    check(true);
}

#[test]
fn large_string_key_last_preserves_complete_empty_bag() {
    check(false);
}
