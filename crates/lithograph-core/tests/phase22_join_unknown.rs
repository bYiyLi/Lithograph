//! Unknown estimates must not move a connected fanout ahead of an empty Part.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryError, QueryResources, QuerySummary,
    prepare_with_resources_and_interrupt,
};
use lithograph_core::storage::{self, CommitMetadata, LayerBuilder, RelationshipRecord};
use rusqlite::Connection;

const FANOUT: i64 = 1100;
const EMPTY_JOIN: &str = "MATCH(a)-[r0:R]->(b),(x:Missing),(b)-[r2:R]->(c) RETURN 0";
const ONE_JOIN: &str = "MATCH(a:InOne)-[r0:R]->(b),(x:Anchor),(b)-[r2:R]->(c:OutOne) \
    RETURN elementId(a),elementId(r0),elementId(b),elementId(x),elementId(r2),elementId(c)";

#[derive(Clone, Copy)]
struct WorkLimit {
    vm: u64,
    checks: u64,
}

impl WorkLimit {
    fn baseline() -> Self {
        Self {
            vm: 2_000_000,
            checks: 200_000,
        }
    }

    fn relative_to(oracle: &Evidence) -> Self {
        Self {
            vm: oracle
                .vm
                .saturating_mul(4)
                .saturating_add(100_000)
                .min(Self::baseline().vm),
            checks: oracle
                .checks
                .saturating_mul(4)
                .saturating_add(10_000)
                .min(Self::baseline().checks),
        }
    }
}

#[derive(Debug)]
struct Consumed {
    rows: Vec<Vec<Value>>,
    plan: String,
    summary: QuerySummary,
}

#[derive(Debug)]
struct Evidence {
    result: Consumed,
    vm: u64,
    checks: u64,
}

fn consume(
    db: &Connection,
    text: &str,
    interrupted: &dyn Fn() -> bool,
) -> Result<Consumed, QueryError> {
    let options = ExecutionOptions::default();
    let resources = QueryResources::new(options.memory_limit_bytes);
    let prepared = prepare_with_resources_and_interrupt(
        db,
        &format!("PROFILE {text}"),
        BTreeMap::new(),
        options,
        resources,
        interrupted,
    )?;
    // No CP plus 4401 graph changes exceeds the global 4096 collection grant.
    assert_eq!(prepared.statistics, Default::default(), "{text}");
    let plan = prepared.physical.explain();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch_with_interrupt(db, 7, interrupted)?;
        rows.extend(batch.rows);
        if batch.done {
            return Ok(Consumed {
                rows,
                plan,
                summary: cursor.complete(db)?,
            });
        }
    }
}

fn measured(db: &Connection, text: &str, limit: WorkLimit) -> Evidence {
    // A standalone Core caller owns a stable main read context for the drain.
    db.execute_batch("SAVEPOINT phase22_unknown_read").unwrap();
    let _: String = db
        .query_row(
            "SELECT database_id FROM _lithograph_meta WHERE id=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let vm = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&vm);
    db.progress_handler(
        1,
        Some(move || observed.fetch_add(1, Ordering::Relaxed) >= limit.vm),
    )
    .unwrap();
    let checks = Cell::new(0_u64);
    let result = consume(db, text, &|| {
        checks.set(checks.get() + 1);
        checks.get() > limit.checks
    });
    // Clear the host interrupt source before releasing the caller's savepoint.
    db.progress_handler(0, None::<fn() -> bool>).unwrap();
    db.execute_batch("RELEASE phase22_unknown_read").unwrap();
    let vm = vm.load(Ordering::Relaxed);
    let checks = checks.get();
    let result = result.unwrap_or_else(|error| {
        panic!("bounded Join work exceeded or execution failed: vm={vm}, checks={checks}, {text}: {error:?}")
    });
    assert!(vm > 0 && vm <= limit.vm && checks <= limit.checks);
    Evidence { result, vm, checks }
}

fn canonical(text: &str) -> String {
    let (pattern, output) = text.rsplit_once(" RETURN ").unwrap();
    format!("{pattern} WHERE true OR false RETURN {output}")
}

fn bag(rows: &[Vec<Value>]) -> Vec<String> {
    let mut rows = rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

fn assert_linear(actual: &Evidence, oracle: &Evidence) {
    let metrics = &actual.result.summary.metrics;
    let operators = metrics.operator_profile().expect("real PROFILE operators");
    assert!(metrics.db_hits <= (FANOUT as u64) * 16 + 1024, "{actual:?}");
    assert!(
        operators
            .iter()
            .all(|operator| operator.rows <= (FANOUT as u64) * 8 + 1024),
        "quadratic intermediate rows: {actual:?}"
    );
    assert!(
        actual.vm <= oracle.vm.saturating_mul(2) + 20_000,
        "{actual:?}\n{oracle:?}"
    );
    assert_eq!(bag(&actual.result.rows), bag(&oracle.result.rows));
    eprintln!("unknown Join actual={actual:?}\nwritten-order oracle={oracle:?}");
}

fn fixture() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)",
    )
    .unwrap();
    db.execute(
        "INSERT INTO _lithograph_meta VALUES(1,'lithograph-format-v1','00000000-0000-4000-8000-000000000025',?1)",
        [storage::STORAGE_FORMAT],
    )
    .unwrap();
    storage::create_storage_schema(&db).unwrap();
    let root = storage::initialize_root(&db).unwrap().root;
    let edge = storage::intern_relationship_type(&db, "R").unwrap();
    let anchor = storage::intern_label(&db, "Anchor").unwrap();
    let incoming = storage::intern_label(&db, "InOne").unwrap();
    let outgoing = storage::intern_label(&db, "OutOne").unwrap();
    let mut layer = LayerBuilder::default();
    let hub = storage::allocate_node_id(&db).unwrap();
    layer.add_node(hub).unwrap();
    layer.add_label(hub, anchor).unwrap();
    seed_fan(&db, &mut layer, hub, edge, incoming, true);
    seed_fan(&db, &mut layer, hub, edge, outgoing, false);
    storage::commit_layer(
        &db,
        "main",
        root,
        None,
        &layer,
        &CommitMetadata {
            author: None,
            message: Some("Phase22 unknown Join synthetic hub".into()),
            committed_at: 22_400,
        },
    )
    .unwrap();
    let checkpoints: i64 = db
        .query_row("SELECT count(*) FROM _lithograph_checkpoints", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(checkpoints, 0);
    let (nodes, relationships): (i64, i64) = db
        .query_row(
            "SELECT (SELECT count(*) FROM _lithograph_node_delta),(SELECT count(*) FROM _lithograph_rel_delta)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(nodes, FANOUT * 2 + 1);
    assert_eq!(relationships, FANOUT * 2);
    assert!(nodes + relationships + 4 > 4096);
    db
}

fn seed_fan(
    db: &Connection,
    layer: &mut LayerBuilder,
    hub: i64,
    edge: storage::RelationshipTypeId,
    selected_label: storage::LabelId,
    incoming: bool,
) {
    for ordinal in 0..FANOUT {
        let leaf = storage::allocate_node_id(db).unwrap();
        layer.add_node(leaf).unwrap();
        if ordinal == 0 {
            layer.add_label(leaf, selected_label).unwrap();
        }
        let id = storage::allocate_relationship_id(db).unwrap();
        let (source, target) = if incoming { (leaf, hub) } else { (hub, leaf) };
        layer
            .add_relationship(RelationshipRecord {
                id,
                source,
                target,
                type_id: edge,
            })
            .unwrap();
    }
}

#[test]
fn known_empty_part_precedes_unknown_connected_hub_fanout() {
    let db = fixture();
    let head = storage::branch_head(&db, "main").unwrap();
    let oracle = measured(&db, &canonical(EMPTY_JOIN), WorkLimit::baseline());
    assert!(oracle.result.rows.is_empty());
    assert!(
        oracle.result.summary.metrics.db_hits > 0,
        "the original first Part ran"
    );
    assert!(!oracle.result.plan.contains("NestedLoop"), "{oracle:?}");
    let actual = measured(&db, EMPTY_JOIN, WorkLimit::relative_to(&oracle));
    assert_linear(&actual, &oracle);
    assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
}

#[test]
fn unknown_mixed_parts_preserve_complete_entity_values_and_linear_work() {
    let db = fixture();
    let head = storage::branch_head(&db, "main").unwrap();
    let oracle = measured(&db, &canonical(ONE_JOIN), WorkLimit::baseline());
    assert!(!oracle.result.plan.contains("NestedLoop"), "{oracle:?}");
    let expected =
        ["n:2", "r:1", "n:1", "n:1", "r:1101", "n:1102"].map(|id| Value::String(id.into()));
    assert_eq!(oracle.result.rows, vec![expected.to_vec()]);
    let actual = measured(&db, ONE_JOIN, WorkLimit::relative_to(&oracle));
    assert_linear(&actual, &oracle);
    assert_eq!(actual.result.rows, vec![expected.to_vec()]);
    assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
}
