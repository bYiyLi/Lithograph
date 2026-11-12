//! Public CREATE identities form the independent grouped-output oracle.

use std::collections::{BTreeMap, BTreeSet};

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, OperatorRuntimeMetrics, QueryCounters, QueryCursor, QueryError, prepare,
};
use lithograph_core::storage;
use rusqlite::Connection;

pub const STAR: &str = "MATCH (h:Hub)-[r:A]->(a:LeafA), (h)-[s:B]->(b:LeafB) \
    WHERE h.k=$key \
    RETURN elementId(h),elementId(r),elementId(a),elementId(s),elementId(b)";

pub struct ReadScope<'a>(&'a Connection);

impl<'a> ReadScope<'a> {
    pub fn new(db: &'a Connection) -> Self {
        db.execute_batch("SAVEPOINT phase22_factorized_read")
            .unwrap();
        Self(db)
    }
}

impl Drop for ReadScope<'_> {
    fn drop(&mut self) {
        self.0
            .execute_batch("RELEASE phase22_factorized_read")
            .unwrap();
    }
}

#[derive(Debug)]
pub struct Observed {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub plan: String,
    pub profile: Vec<OperatorRuntimeMetrics>,
    pub db_hits: u64,
    pub counters: QueryCounters,
    pub commit: Option<String>,
    pub memory_peak_bytes: u64,
}

pub fn execute(
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
            let done = {
                let batch = cursor.next_batch(db, batch_size)?;
                let done = batch.done;
                rows.extend(batch.rows);
                done
            };
            if done {
                let summary = cursor.complete(db)?;
                let profile = summary
                    .metrics
                    .operator_profile()
                    .map_or_else(Vec::new, |operators| operators.to_vec());
                if !profile.is_empty() {
                    assert_eq!(
                        profile.iter().map(|operator| operator.db_hits).sum::<u64>(),
                        summary.metrics.db_hits,
                        "every factor/flatten storage read belongs to PROFILE: {plan}; {profile:?}"
                    );
                }
                return Ok(Observed {
                    columns,
                    rows,
                    plan,
                    profile,
                    db_hits: summary.metrics.db_hits,
                    counters: summary.counters.clone(),
                    commit: summary.commit.clone(),
                    memory_peak_bytes: resources.peak(),
                });
            }
        }
    })();
    drop(cursor);
    assert_eq!(
        resources.used(),
        0,
        "success/error released every group lease"
    );
    result
}

pub fn read(
    db: &Connection,
    text: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
    batch_size: usize,
) -> Observed {
    let _scope = ReadScope::new(db);
    execute(db, text, params, options, batch_size)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

pub fn write(db: &Connection, text: &str) -> Observed {
    execute(db, text, BTreeMap::new(), ExecutionOptions::default(), 256)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

pub fn params() -> BTreeMap<String, Value> {
    BTreeMap::from([("key".to_owned(), Value::Integer(7))])
}

pub fn empty_db() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT NOT NULL,\
         database_id TEXT NOT NULL,storage_format INTEGER NOT NULL)",
    )
    .unwrap();
    db.execute(
        "INSERT INTO main._lithograph_meta VALUES(1,'lithograph-format-v1',\
         '00000000-0000-4000-8000-000000000027',?1)",
        [storage::STORAGE_FORMAT],
    )
    .unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    db
}

pub fn checkpoint(db: &Connection) {
    storage::create_checkpoint(db, storage::branch_head(db, "main").unwrap()).unwrap();
}

#[derive(Clone, Debug)]
pub struct ArmRow {
    pub relationship: Value,
    pub endpoint: Value,
}

pub struct StarFixture {
    pub db: Connection,
    pub hub: Value,
    pub left: Vec<ArmRow>,
    pub right: Vec<ArmRow>,
}

impl StarFixture {
    pub fn new(left_endpoints: usize, right_endpoints: usize) -> Self {
        let db = empty_db();
        let created = write(&db, "CREATE(h:Hub {k:7}) RETURN elementId(h),h.k");
        assert_eq!(created.rows.len(), 1);
        assert_eq!(created.rows[0][1], Value::Integer(7));
        let hub = created.rows[0][0].clone();
        for ddl in [
            "CREATE CONSTRAINT s7_hub_k_type FOR(h:Hub) REQUIRE h.k IS :: INTEGER",
            "CREATE CONSTRAINT s7_hub_k_unique FOR(h:Hub) REQUIRE h.k IS UNIQUE",
        ] {
            write(&db, ddl);
        }
        let left = create_arm(&db, "A", "LeafA", left_endpoints, &hub);
        let right = create_arm(&db, "B", "LeafB", right_endpoints, &hub);
        checkpoint(&db);
        Self {
            db,
            hub,
            left,
            right,
        }
    }

    pub fn expected_rows(&self) -> Vec<Vec<Value>> {
        self.left
            .iter()
            .flat_map(|left| {
                self.right.iter().map(move |right| {
                    vec![
                        self.hub.clone(),
                        left.relationship.clone(),
                        left.endpoint.clone(),
                        right.relationship.clone(),
                        right.endpoint.clone(),
                    ]
                })
            })
            .collect()
    }
}

fn create_arm(
    db: &Connection,
    kind: &str,
    label: &str,
    endpoints: usize,
    hub: &Value,
) -> Vec<ArmRow> {
    if endpoints == 0 {
        return Vec::new();
    }
    let text = format!(
        "MATCH(h:Hub) WHERE h.k=7 UNWIND range(1,{endpoints}) AS seq \
         CREATE(n:{label} {{seq:seq}}) CREATE(h)-[r:{kind}]->(n),(h)-[s:{kind}]->(n) \
         RETURN elementId(h),elementId(n),elementId(r),elementId(startNode(r)),\
         elementId(endNode(r)),elementId(s),elementId(startNode(s)),elementId(endNode(s)),seq"
    );
    let created = write(db, &text);
    assert_eq!(created.rows.len(), endpoints);
    let mut arm = Vec::new();
    let mut ids = BTreeSet::new();
    let mut nodes = BTreeSet::new();
    let mut sequences = BTreeSet::new();
    for row in created.rows {
        assert_eq!(row.len(), 9);
        assert_eq!(&row[0], hub);
        assert!(matches!(&row[1], Value::String(id) if id.starts_with("n:")));
        assert!(nodes.insert(format!("{:?}", row[1])));
        let Value::Integer(seq) = row[8] else {
            panic!("public CREATE sequence must remain INTEGER")
        };
        assert!((1..=i64::try_from(endpoints).unwrap()).contains(&seq));
        assert!(sequences.insert(seq));
        for (relationship, start, end) in [(2, 3, 4), (5, 6, 7)] {
            assert_eq!(&row[start], hub);
            assert_eq!(row[end], row[1]);
            assert!(matches!(&row[relationship], Value::String(id) if id.starts_with("r:")));
            assert!(ids.insert(format!("{:?}", row[relationship])));
            arm.push(ArmRow {
                relationship: row[relationship].clone(),
                endpoint: row[1].clone(),
            });
        }
    }
    arm
}

pub fn bag(rows: &[Vec<Value>]) -> Vec<String> {
    let mut rows = rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

pub fn assert_bag(actual: &[Vec<Value>], expected: &[Vec<Value>]) {
    assert_eq!(bag(actual), bag(expected));
}

pub fn actual_operator<'a>(result: &'a Observed, name: &str) -> &'a OperatorRuntimeMetrics {
    assert!(result.plan.contains(name), "{}", result.plan);
    let operators = result
        .profile
        .iter()
        .filter(|operator| operator.operator == name)
        .collect::<Vec<_>>();
    assert_eq!(operators.len(), 1, "{name}: {:?}", result.profile);
    operators[0]
}
