//! Complete identity bags prove that a keyed tail neither replays its prefix
//! nor loses duplicate Build rows, for both resident and Grace Hash execution.

use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, HashJoinRuntimeMetrics, OperatorRuntimeMetrics, QueryCursor, QueryError,
    prepare,
};
use lithograph_core::storage;
use rusqlite::Connection;

#[path = "phase22_dynamic_filter/boundaries.rs"]
mod boundaries;

const PROBE_ROWS: usize = 1000;
const TARGETS: [usize; 3] = [101, 501, 901];
const JOIN: &str = "MATCH(p:DFProbe),(b:DFBuild) WHERE p.k=b.k \
    RETURN elementId(p) AS probe,elementId(b) AS build,p.k AS probeKey,b.k AS buildKey";

struct ReadScope<'a>(&'a Connection);

impl<'a> ReadScope<'a> {
    fn new(db: &'a Connection) -> Self {
        db.execute_batch("SAVEPOINT phase22_df_read").unwrap();
        let _: String = db
            .query_row(
                "SELECT database_id FROM main._lithograph_meta WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        Self(db)
    }
}

impl Drop for ReadScope<'_> {
    fn drop(&mut self) {
        self.0.execute_batch("RELEASE phase22_df_read").unwrap();
    }
}

#[derive(Debug)]
struct Observed {
    columns: Vec<String>,
    plan: String,
    rows: Vec<Vec<Value>>,
    profile: Option<Vec<OperatorRuntimeMetrics>>,
}

fn consume(
    db: &Connection,
    text: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
    batch_size: usize,
) -> Result<Observed, QueryError> {
    let prepared = prepare(db, text, params, options)?;
    let columns = prepared.columns.clone();
    let plan = prepared.physical.explain();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let result = (|| {
        let mut rows = Vec::new();
        loop {
            let batch = cursor.next_batch(db, batch_size)?;
            let done = batch.done;
            rows.extend(batch.rows);
            if done {
                let summary = cursor.complete(db)?;
                let profile = summary
                    .metrics
                    .operator_profile()
                    .map(|operators| operators.to_vec());
                if let Some(operators) = &profile {
                    assert_eq!(
                        operators
                            .iter()
                            .map(|operator| operator.db_hits)
                            .sum::<u64>(),
                        summary.metrics.db_hits,
                        "exact storage accounting: {plan}; {operators:?}"
                    );
                }
                return Ok(Observed {
                    columns,
                    plan,
                    rows,
                    profile,
                });
            }
        }
    })();
    drop(cursor);
    assert_eq!(
        resources.used(),
        0,
        "complete/error must release all leases"
    );
    if let Some(limit) = resources.limit() {
        assert!(
            resources.peak() <= limit,
            "query peak {} exceeds {limit}",
            resources.peak()
        );
    }
    result
}

fn write(db: &Connection, text: &str) -> Observed {
    consume(db, text, BTreeMap::new(), ExecutionOptions::default(), 256)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

fn read(db: &Connection, text: &str, options: ExecutionOptions, batch_size: usize) -> Observed {
    let head = storage::branch_head(db, "main").unwrap();
    let scope = ReadScope::new(db);
    let result = consume(db, text, BTreeMap::new(), options, batch_size)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"));
    drop(scope);
    assert!(db.is_autocommit());
    assert_eq!(storage::branch_head(db, "main").unwrap(), head);
    result
}

fn empty_db() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT NOT NULL,\
         database_id TEXT NOT NULL,storage_format INTEGER NOT NULL)",
    )
    .unwrap();
    db.execute(
        "INSERT INTO main._lithograph_meta VALUES(1,'lithograph-format-v1',\
         '00000000-0000-4000-8000-000000000026',?1)",
        [storage::STORAGE_FORMAT],
    )
    .unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    db
}

#[derive(Debug)]
struct Seed {
    id: Value,
    key: Value,
}

fn seed(db: &Connection, label: &str, keys: &[Value]) -> Vec<Seed> {
    let mut observed = Vec::with_capacity(keys.len());
    for (chunk, values) in keys.chunks(256).enumerate() {
        let first = chunk * 256 + 1;
        let text = format!(
            "UNWIND range(0,size($keys)-1) AS i \
             CREATE(n:{label}:DFVisible {{seq:$first+i,k:$keys[i]}}) \
             RETURN elementId(n),n.k,n.seq"
        );
        let result = consume(
            db,
            &text,
            BTreeMap::from([
                ("keys".into(), Value::List(values.to_vec())),
                ("first".into(), Value::Integer(first as i64)),
            ]),
            ExecutionOptions::default(),
            256,
        )
        .unwrap();
        let mut rows = result.rows;
        rows.sort_by_key(|row| match row[2] {
            Value::Integer(seq) => seq,
            _ => panic!("public seed sequence"),
        });
        assert_eq!(rows.len(), values.len());
        for (offset, row) in rows.into_iter().enumerate() {
            assert_eq!(row.len(), 3);
            assert!(matches!(row[0], Value::String(_)));
            assert_eq!(row[1], values[offset]);
            assert_eq!(row[2], Value::Integer((first + offset) as i64));
            observed.push(Seed {
                id: row[0].clone(),
                key: row[1].clone(),
            });
        }
    }
    observed
}

fn rebuild_and_checkpoint(
    db: &Connection,
    build_rows: usize,
    probe_rows: usize,
    probe_index: &str,
) {
    let head = storage::branch_head(db, "main").unwrap();
    let version = format!("commit/{}", head.to_hex());
    for (name, rows) in [(probe_index, probe_rows), ("df_build_k_range", build_rows)] {
        let result = consume(
            db,
            "CALL lithograph.index.rebuild($name,$version) \
             YIELD name,commit,indexedEntities RETURN name,commit,indexedEntities",
            BTreeMap::from([
                ("name".into(), Value::String(name.into())),
                ("version".into(), Value::String(version.clone())),
            ]),
            ExecutionOptions::default(),
            3,
        )
        .unwrap();
        assert_eq!(
            result.rows,
            vec![vec![
                Value::String(name.into()),
                Value::String(version.clone()),
                Value::Integer(rows as i64),
            ]]
        );
        assert_eq!(storage::branch_head(db, "main").unwrap(), head);
    }
    storage::create_checkpoint(db, head).unwrap();
    assert_eq!(storage::branch_head(db, "main").unwrap(), head);
}

struct Fixture {
    db: Connection,
    expected: Vec<Vec<Value>>,
}

fn fixture(build_rows: usize) -> Fixture {
    let db = empty_db();
    let probe_keys = (1..=PROBE_ROWS)
        .map(|seq| {
            let key = if let Some(key) = TARGETS.iter().position(|target| *target == seq) {
                key
            } else {
                let skipped = TARGETS.iter().filter(|target| **target < seq).count();
                PROBE_ROWS - 1 - (seq - 1 - skipped)
            };
            Value::Integer(key as i64)
        })
        .collect::<Vec<_>>();
    // Probe is created first; verify the public IDs instead of inferring them.
    let probe = seed(&db, "DFProbe", &probe_keys);
    for seq in TARGETS {
        assert_eq!(probe[seq - 1].id, Value::String(format!("n:{seq}")));
    }
    let build_keys = (1..=build_rows)
        .map(|seq| Value::Integer((seq % 3) as i64))
        .collect::<Vec<_>>();
    let build = seed(&db, "DFBuild", &build_keys);
    for text in [
        "CREATE CONSTRAINT df_probe_k_type FOR(p:DFProbe) REQUIRE p.k IS :: INTEGER",
        "CREATE CONSTRAINT df_build_k_type FOR(b:DFBuild) REQUIRE b.k IS :: INTEGER",
        "CREATE CONSTRAINT df_probe_k_key FOR(p:DFProbe) REQUIRE p.k IS NODE KEY",
        "CREATE RANGE INDEX df_build_k_range FOR(b:DFBuild) ON (b.k)",
    ] {
        write(&db, text);
    }
    rebuild_and_checkpoint(&db, build_rows, PROBE_ROWS, "df_probe_k_key");
    let expected = build
        .iter()
        .enumerate()
        .map(|(offset, row)| {
            let key = (offset + 1) % 3;
            assert_eq!(row.key, Value::Integer(key as i64));
            let probe = &probe[TARGETS[key] - 1];
            assert_eq!(probe.key, row.key);
            vec![
                probe.id.clone(),
                row.id.clone(),
                Value::Integer(key as i64),
                Value::Integer(key as i64),
            ]
        })
        .collect();
    Fixture { db, expected }
}

fn bag(rows: &[Vec<Value>]) -> Vec<String> {
    let mut rows = rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

fn checked_join(fixture: &Fixture, options: ExecutionOptions, batch_size: usize) -> Observed {
    checked_join_expected(fixture, options, batch_size, &fixture.expected)
}

fn checked_join_expected(
    fixture: &Fixture,
    options: ExecutionOptions,
    batch_size: usize,
    expected: &[Vec<Value>],
) -> Observed {
    let actual = read(
        &fixture.db,
        &format!("PROFILE {JOIN}"),
        options.clone(),
        batch_size,
    );
    let original = read(
        &fixture.db,
        &format!(
            "PROFILE {}",
            JOIN.replace("WHERE p.k=b.k", "WHERE (p.k=b.k) OR false")
        ),
        options,
        batch_size,
    );
    assert!(
        !original.plan.contains("HashJoin"),
        "the OR-false oracle must retain the original MATCH path: {}",
        original.plan
    );
    assert_eq!(actual.columns, ["probe", "build", "probeKey", "buildKey"]);
    assert_eq!(actual.columns, original.columns);
    assert_eq!(bag(&actual.rows), bag(expected));
    assert_eq!(bag(&actual.rows), bag(&original.rows));
    actual
}

fn assert_dynamic_filter(actual: &Observed, build_rows: usize) -> &HashJoinRuntimeMetrics {
    assert!(actual.plan.contains("HashJoin"), "{}", actual.plan);
    assert!(!actual.plan.contains("IndexNestedLoop"), "{}", actual.plan);
    let profile = actual.profile.as_ref().expect("complete PROFILE execution");
    let hashes = profile
        .iter()
        .filter(|operator| operator.operator == "HashJoin")
        .collect::<Vec<_>>();
    assert_eq!(hashes.len(), 1, "{}; {profile:?}", actual.plan);
    assert_eq!(hashes[0].rows, build_rows as u64);
    let hash = hashes[0]
        .hash
        .as_ref()
        .expect("actual Hash runtime metrics");
    let filter = hash.dynamic_filter.as_ref().expect("actual Dynamic Filter");
    assert!(filter.applied, "{hash:?}");
    assert_eq!(hash.build_rows, build_rows as u64, "{hash:?}");
    assert_eq!(filter.build_rows, build_rows as u64, "{filter:?}");
    assert_eq!(filter.distinct_keys, 3, "{filter:?}");
    assert_eq!(filter.prefix_rows_read, 1, "{filter:?}");
    assert_eq!(filter.equality_seeks, 3, "{filter:?}");
    assert_eq!(filter.tail_owners, 3, "{filter:?}");
    assert_eq!(hash.probe_rows, 4, "{hash:?}");
    assert!(
        hash.probe_rows < PROBE_ROWS as u64,
        "actual keyed skip: {hash:?}"
    );
    assert!(filter.keyset_peak_bytes > 0, "{filter:?}");
    hash
}

#[test]
fn resident_filter_preserves_the_complete_bag_across_the_first_page_frontier() {
    let fixture = fixture(64);
    for batch_size in [1, 3, 256] {
        let actual = checked_join(&fixture, ExecutionOptions::default(), batch_size);
        let hash = assert_dynamic_filter(&actual, 64);
        assert_eq!(hash.spill_partitions, 0, "resident Hash: {hash:?}");
        assert_eq!(hash.spill_batches, 0, "resident Hash: {hash:?}");
        assert_eq!(hash.temp_allocated_bytes, 0, "resident Hash: {hash:?}");
    }
}

#[test]
fn tight_quota_selects_a_legal_index_loop_and_preserves_the_complete_skew_bag() {
    let fixture = fixture(1024);
    let mut options = ExecutionOptions::default();
    options.memory_limit_bytes = Some(512 * 1024);
    let actual = checked_join(&fixture, options, 3);
    // The full distinct upper bound and shared 512 KiB grant make this INL
    // baseline cheaper. The same quota's actual Grace DF consumer is tested
    // independently in the cfg(test)-only forced-admitted candidate tests.
    assert!(actual.plan.contains("IndexNestedLoop"), "{}", actual.plan);
    assert!(!actual.plan.contains("HashJoin"), "{}", actual.plan);
    let profile = actual.profile.as_ref().expect("complete PROFILE");
    assert!(profile.iter().all(|operator| operator.hash.is_none()));
    assert!(
        profile
            .iter()
            .any(|operator| operator.operator == "IndexNestedLoop" && operator.rows == 1024),
        "the selected loop actually consumed every Build duplicate: {profile:?}"
    );
    let counts = TARGETS.map(|seq| {
        let id = Value::String(format!("n:{seq}"));
        actual.rows.iter().filter(|row| row[0] == id).count()
    });
    assert_eq!(counts, [341, 342, 341]);
}
