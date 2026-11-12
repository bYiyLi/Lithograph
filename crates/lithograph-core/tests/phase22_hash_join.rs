//! Typed Hash Join semantics use complete identity bags and the original MATCH oracle.

use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, OperatorRuntimeMetrics, QueryCounters, QueryCursor, QueryError,
    QueryErrorKind, SnapshotSelector, prepare,
};
use lithograph_core::storage;
use rusqlite::Connection;

#[path = "phase22_hash_join/batch.rs"]
mod batch;
#[path = "phase22_hash_join/boundaries.rs"]
mod boundaries;
#[path = "phase22_hash_join/grace.rs"]
mod grace;
#[path = "phase22_hash_join/large_key.rs"]
mod large_key;
#[path = "phase22_hash_join/mixed.rs"]
mod mixed;
#[path = "phase22_hash_join/reuse.rs"]
mod reuse;
#[path = "phase22_hash_join/visibility.rs"]
mod visibility;

const SIZE: usize = 256;
const JOIN: &str = "MATCH(a:Build),(b:Probe) WHERE a.k=b.k \
    RETURN elementId(a) AS build,elementId(b) AS probe,a.k AS buildKey,b.k AS probeKey";

struct ReadScope<'a>(&'a Connection);

impl<'a> ReadScope<'a> {
    fn new(db: &'a Connection) -> Self {
        db.execute_batch("SAVEPOINT phase22_hash_read").unwrap();
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
        self.0.execute_batch("RELEASE phase22_hash_read").unwrap();
    }
}

#[derive(Debug)]
struct Observed {
    columns: Vec<String>,
    plan: String,
    rows: Vec<Vec<Value>>,
    profile: Option<Vec<OperatorRuntimeMetrics>>,
    counters: QueryCounters,
    commit: Option<String>,
}

fn consume(
    db: &Connection,
    text: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Result<Observed, QueryError> {
    let prepared = prepare(db, text, params, options)?;
    let columns = prepared.columns.clone();
    let plan = prepared.physical.explain();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let result = (|| {
        let mut rows = Vec::new();
        loop {
            let done = {
                let batch = cursor.next_batch(db, 3)?;
                let done = batch.done;
                rows.extend(batch.rows);
                done
            };
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
                        "every storage hit has an actual PROFILE boundary: {plan}; {operators:?}"
                    );
                }
                return Ok(Observed {
                    columns,
                    plan,
                    rows,
                    profile,
                    counters: summary.counters.clone(),
                    commit: summary.commit.clone(),
                });
            }
        }
    })();
    drop(cursor);
    assert_eq!(
        resources.used(),
        0,
        "success/error must release all query leases"
    );
    result
}

fn read(db: &Connection, text: &str, options: ExecutionOptions) -> Observed {
    let _scope = ReadScope::new(db);
    consume(db, text, BTreeMap::new(), options).unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

fn write(db: &Connection, text: &str) -> Observed {
    // An engine-owned mutation rollback must not be hidden by a test savepoint.
    consume(db, text, BTreeMap::new(), ExecutionOptions::default())
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

fn query(db: &Connection, text: &str) -> Vec<Vec<Value>> {
    read(db, text, ExecutionOptions::default()).rows
}

fn canonical(text: &str) -> String {
    let (pattern, rest) = text.split_once(" WHERE ").expect("WHERE fixture");
    let (predicate, output) = rest.split_once(" RETURN ").expect("RETURN fixture");
    format!("{pattern} WHERE ({predicate}) OR false RETURN {output}")
}

fn bag(rows: &[Vec<Value>]) -> Vec<String> {
    let mut rows = rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

fn assert_bag(actual: &[Vec<Value>], expected: &[Vec<Value>]) {
    assert_eq!(bag(actual), bag(expected));
}

fn assert_hash(result: &Observed) {
    assert!(
        result.plan.contains("HashJoin"),
        "actual physical plan: {}",
        result.plan
    );
    assert!(!result.plan.contains("IndexNestedLoop"), "{}", result.plan);
    let profile = result.profile.as_ref().expect("complete PROFILE execution");
    assert!(
        profile
            .iter()
            .any(|operator| operator.operator == "HashJoin" && operator.rows > 0),
        "Hash must actually produce rows: {profile:?}"
    );
    for operator in profile
        .iter()
        .filter(|operator| operator.operator == "HashJoin")
    {
        let hash = operator.hash.as_ref().expect("actual Hash runtime metrics");
        assert!(hash.build_rows > 0 && hash.probe_rows > 0, "{operator:?}");
    }
}

fn assert_fallback(result: &Observed) {
    assert!(
        !result.plan.contains("HashJoin"),
        "unsupported proof must fall back: {}",
        result.plan
    );
    assert!(
        result
            .profile
            .as_ref()
            .is_none_or(|profile| profile.iter().all(|op| op.operator != "HashJoin"))
    );
}

fn checked_join(
    db: &Connection,
    text: &str,
    options: ExecutionOptions,
    expected: &[Vec<Value>],
) -> Observed {
    let actual = read(db, &format!("PROFILE {text}"), options.clone());
    let original = read(db, &format!("PROFILE {}", canonical(text)), options);
    assert_fallback(&original);
    assert_eq!(actual.columns, original.columns);
    assert_bag(&actual.rows, expected);
    assert_bag(&actual.rows, &original.rows);
    actual
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
         '00000000-0000-4000-8000-000000000025',?1)",
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

fn seed(db: &Connection, label: &str, keys: Vec<Value>) -> Vec<Seed> {
    assert!(!keys.is_empty());
    let count = keys.len();
    let text = format!(
        "UNWIND range(0,size($keys)-1) AS seq CREATE(n:{label}:Visible {{seq:seq,k:$keys[seq]}}) \
         RETURN elementId(n),n.k,n.seq"
    );
    let result = consume(
        db,
        &text,
        BTreeMap::from([("keys".into(), Value::List(keys))]),
        ExecutionOptions::default(),
    )
    .unwrap();
    let mut rows = result.rows;
    assert_eq!(rows.len(), count);
    rows.sort_by_key(|row| match row[2] {
        Value::Integer(seq) => seq,
        _ => panic!("seed sequence"),
    });
    rows.into_iter()
        .enumerate()
        .map(|(seq, row)| {
            assert_eq!(row.len(), 3);
            assert_eq!(row[2], Value::Integer(seq as i64));
            assert!(matches!(row[0], Value::String(_)));
            Seed {
                id: row[0].clone(),
                key: row[1].clone(),
            }
        })
        .collect()
}

fn checkpoint(db: &Connection) {
    storage::create_checkpoint(db, storage::branch_head(db, "main").unwrap()).unwrap();
}

fn constraints(db: &Connection, build_type: &str, probe_type: &str) {
    for (label, kind) in [("Build", build_type), ("Probe", probe_type)] {
        write(
            db,
            &format!("CREATE CONSTRAINT {label}_k FOR(n:{label}) REQUIRE n.k IS :: {kind}"),
        );
        write(
            db,
            &format!("CREATE CONSTRAINT {label}_seq FOR(n:{label}) REQUIRE n.seq IS :: INTEGER"),
        );
    }
    checkpoint(db);
}

fn expected_pairs(build: &[Seed], probe: &[Seed], pairs: &[(usize, usize)]) -> Vec<Vec<Value>> {
    pairs
        .iter()
        .map(|&(a, b)| {
            vec![
                build[a].id.clone(),
                probe[b].id.clone(),
                build[a].key.clone(),
                probe[b].key.clone(),
            ]
        })
        .collect()
}

fn integer_fixture() -> (Connection, Vec<Seed>, Vec<Seed>) {
    let db = empty_db();
    let build = seed(
        &db,
        "Build",
        (0..SIZE)
            .map(|seq| Value::Integer(seq as i64 % 32))
            .collect(),
    );
    let mut keys = vec![
        Value::Integer(5),
        Value::Integer(5),
        Value::Integer(13),
        Value::Null,
        Value::Integer(99_999),
    ];
    keys.extend((5..SIZE).map(|seq| Value::Integer(100_000 + seq as i64)));
    let probe = seed(&db, "Probe", keys);
    constraints(&db, "INTEGER", "INTEGER");
    (db, build, probe)
}

fn integer_pairs() -> Vec<(usize, usize)> {
    (0..SIZE)
        .flat_map(|a| {
            (0..3).filter_map(move |b| (a % 32 == if b == 2 { 13 } else { 5 }).then_some((a, b)))
        })
        .collect()
}

#[test]
fn hash_integer_bag_preserves_ids_duplicate_keys_nulls_and_safe_residuals() {
    let (db, build, probe) = integer_fixture();
    let expected = expected_pairs(&build, &probe, &integer_pairs());
    assert_eq!(expected.len(), 24);
    let actual = checked_join(&db, JOIN, ExecutionOptions::default(), &expected);
    assert_eq!(actual.columns, ["build", "probe", "buildKey", "probeKey"]);
    assert_hash(&actual);
    let reversed = JOIN.replace("a.k=b.k", "b.k=a.k");
    assert_hash(&checked_join(
        &db,
        &reversed,
        ExecutionOptions::default(),
        &expected,
    ));
    let text = JOIN.replace("a.k=b.k", "a.k=b.k AND a.seq>=0 AND b.seq>=0");
    assert_hash(&checked_join(
        &db,
        &text,
        ExecutionOptions::default(),
        &expected,
    ));
    let star = "MATCH(a:Build),(b:Probe) WHERE a.k=b.k RETURN *";
    let result = read(&db, &format!("PROFILE {star}"), ExecutionOptions::default());
    assert_hash(&result);
    assert_eq!(result.columns, ["a", "b"]);
    assert_bag(&result.rows, &query(&db, &canonical(star)));
}

#[test]
fn hash_boolean_and_string_keys_keep_exact_family_and_multiplicity() {
    for (kind, mut left, mut right, pairs) in [
        (
            "BOOLEAN",
            vec![
                Value::Boolean(true),
                Value::Boolean(true),
                Value::Boolean(false),
                Value::Null,
            ],
            vec![
                Value::Boolean(true),
                Value::Boolean(true),
                Value::Boolean(true),
                Value::Boolean(false),
                Value::Null,
            ],
            vec![(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (2, 3)],
        ),
        (
            "STRING",
            ["", "a", "a", "A", "a "]
                .into_iter()
                .map(|s| Value::String(s.into()))
                .chain([Value::Null])
                .collect(),
            ["", "a", "a", "absent"]
                .into_iter()
                .map(|s| Value::String(s.into()))
                .chain([Value::Null])
                .collect(),
            vec![(0, 0), (1, 1), (1, 2), (2, 1), (2, 2)],
        ),
    ] {
        let db = empty_db();
        left.resize(SIZE, Value::Null);
        right.resize(SIZE, Value::Null);
        let build = seed(&db, "Build", left);
        let probe = seed(&db, "Probe", right);
        constraints(&db, kind, kind);
        assert_hash(&checked_join(
            &db,
            JOIN,
            ExecutionOptions::default(),
            &expected_pairs(&build, &probe, &pairs),
        ));
    }
}

#[test]
fn hash_numeric_keys_preserve_signed_zero_nan_and_integer_float_precision() {
    let db = empty_db();
    let mut left = vec![
        Value::Integer(0),
        Value::Integer(1),
        Value::Integer(9_007_199_254_740_992),
        Value::Integer(9_007_199_254_740_993),
        Value::Float(f64::NAN),
    ];
    let mut right = vec![
        Value::Float(0.0),
        Value::Float(-0.0),
        Value::Float(1.0),
        Value::Float(9_007_199_254_740_992.0),
        Value::Float(f64::NAN),
    ];
    left.extend((5..SIZE).map(|seq| Value::Integer(100_000 + seq as i64)));
    right.extend((5..SIZE).map(|seq| Value::Float(200_000.0 + seq as f64)));
    let build = seed(&db, "Build", left);
    let probe = seed(&db, "Probe", right);
    constraints(&db, "INTEGER | FLOAT", "FLOAT");
    let expected = expected_pairs(&build, &probe, &[(0, 0), (0, 1), (1, 2), (2, 3)]);
    assert_hash(&checked_join(
        &db,
        JOIN,
        ExecutionOptions::default(),
        &expected,
    ));
    let float_text = "MATCH(a:Probe),(b:Probe) WHERE a.k=b.k RETURN elementId(a),elementId(b)";
    let pairs = (0..SIZE)
        .filter(|&a| a != 4)
        .flat_map(|a| {
            (0..SIZE).filter_map(move |b| ((a == b || a < 2 && b < 2) && b != 4).then_some((a, b)))
        })
        .map(|(a, b)| vec![probe[a].id.clone(), probe[b].id.clone()])
        .collect::<Vec<_>>();
    assert_eq!(pairs.len(), SIZE + 1);
    assert_hash(&checked_join(
        &db,
        float_text,
        ExecutionOptions::default(),
        &pairs,
    ));
}

#[test]
fn node_identity_preserves_ids_outer_duplicates_and_cheapest_access() {
    let (db, build, _) = integer_fixture();
    let extra = write(&db, "CREATE(n:Build:Visible {seq:256}) RETURN elementId(n)");
    checkpoint(&db);
    let text = "MATCH(a:Build),(b:Build) WHERE a=b RETURN elementId(a),elementId(b)";
    let mut expected = build
        .iter()
        .map(|node| vec![node.id.clone(), node.id.clone()])
        .collect::<Vec<_>>();
    expected.push(vec![extra.rows[0][0].clone(), extra.rows[0][0].clone()]);
    assert_eq!(expected.len(), 257, "cross the internal 256-row boundary");
    let actual = checked_join(&db, text, ExecutionOptions::default(), &expected);
    // Dedicated key and typed-spill unit tests cover HashFamily::NodeIdentity.
    // This simple equality has a cheap identity Seek, which must remain a real
    // cost alternative to building a Hash table over every distinct Node.
    if actual.plan.contains("HashJoin") {
        assert_hash(&actual);
    } else {
        let profile = actual.profile.as_ref().expect("complete PROFILE execution");
        for producer in ["IndexNestedLoop", "NodeIdSeek"] {
            assert!(
                profile.iter().any(|operator| {
                    operator.operator == producer
                        && operator.rows > 0
                        && (producer != "NodeIdSeek" || operator.db_hits > 0)
                }),
                "actual cheap {producer}: {}; {profile:?}",
                actual.plan
            );
        }
    }
    let ordered = "UNWIND [10,10,20] AS outer MATCH(a:Build),(b:Probe) WHERE a.k=b.k \
        RETURN outer,a.seq,b.seq ORDER BY outer,a.seq,b.seq SKIP 2 LIMIT 7";
    let mut expected = [10, 10, 20]
        .into_iter()
        .flat_map(|outer| {
            integer_pairs().into_iter().map(move |(a, b)| {
                vec![
                    Value::Integer(outer),
                    Value::Integer(a as i64),
                    Value::Integer(b as i64),
                ]
            })
        })
        .collect::<Vec<_>>();
    expected.sort_by_key(|row| {
        row.iter()
            .map(|value| {
                if let Value::Integer(n) = value {
                    *n
                } else {
                    unreachable!()
                }
            })
            .collect::<Vec<_>>()
    });
    let expected = expected.into_iter().skip(2).take(7).collect::<Vec<_>>();
    let result = checked_join(&db, ordered, ExecutionOptions::default(), &expected);
    assert_hash(&result);
    assert_eq!(result.rows, expected);
}

#[test]
fn two_hash_stages_preserve_the_full_three_label_identity_bag() {
    let (db, build, probe) = integer_fixture();
    // Only 5 and 13 reach the original 24-row Build/Probe bag. The other two
    // repeated keys and all padding must not manufacture a three-way match.
    let mut keys = [1, 1, 2, 2, 5, 5, 13, 13]
        .into_iter()
        .map(Value::Integer)
        .collect::<Vec<_>>();
    keys.extend((8..SIZE).map(|seq| Value::Integer(300_000 + seq as i64)));
    let third = seed(&db, "Third", keys);
    write(
        &db,
        "CREATE CONSTRAINT Third_k FOR(n:Third) REQUIRE n.k IS :: INTEGER",
    );
    write(
        &db,
        "CREATE CONSTRAINT Third_seq FOR(n:Third) REQUIRE n.seq IS :: INTEGER",
    );
    checkpoint(&db);
    let text = "MATCH(a:Build),(b:Probe),(c:Third) WHERE a.k=b.k AND b.k=c.k \
        RETURN elementId(a) AS build,elementId(b) AS probe,elementId(c) AS third,\
        a.k AS buildKey,b.k AS probeKey,c.k AS thirdKey";
    // An independent seed recipe avoids running a 256^3 canonical Cartesian
    // oracle. The single-edge tests above retain the original OR-false oracle.
    let expected = integer_pairs()
        .into_iter()
        .flat_map(|(a, b)| {
            let c = if b == 2 { [6, 7] } else { [4, 5] };
            let build = &build;
            let probe = &probe;
            let third = &third;
            c.into_iter().map(move |c| {
                vec![
                    build[a].id.clone(),
                    probe[b].id.clone(),
                    third[c].id.clone(),
                    build[a].key.clone(),
                    probe[b].key.clone(),
                    third[c].key.clone(),
                ]
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), 48);
    let actual = read(&db, &format!("PROFILE {text}"), ExecutionOptions::default());
    assert_eq!(
        actual.columns,
        [
            "build", "probe", "third", "buildKey", "probeKey", "thirdKey"
        ]
    );
    assert_bag(&actual.rows, &expected);
    let producers = actual
        .profile
        .as_ref()
        .expect("complete PROFILE execution")
        .iter()
        .filter(|operator| operator.operator == "HashJoin" && operator.rows > 0)
        .collect::<Vec<_>>();
    assert!(
        producers.len() >= 2,
        "two distinct Hash stages must actually produce rows: {}; {:?}",
        actual.plan,
        actual.profile
    );
    assert_ne!(producers[0].id, producers[1].id);
    for operator in producers {
        let hash = operator
            .hash
            .as_ref()
            .expect("each Hash has its own runtime metrics");
        assert!(hash.build_rows > 0 && hash.probe_rows > 0, "{operator:?}");
    }
}
