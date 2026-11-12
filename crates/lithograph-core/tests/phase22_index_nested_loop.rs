//! Parameterized Standard Index probes share the existing MATCH cursor.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, PhysicalOperator, PhysicalPlan, QueryCursor, QueryError, QueryErrorKind,
    QuerySummary, SnapshotSelector, prepare,
};
use lithograph_core::storage::{
    self, CommitMetadata, LayerBuilder, OwnerKind, PropertyValue, RelationshipRecord,
};
use rusqlite::Connection;

#[cfg(feature = "test-support")]
#[path = "phase22_index_nested_loop/budget.rs"]
mod budget;

const ITEMS: i64 = 256;
const NODE_JOIN: &str = "MATCH (i:Item),(s:Source) WHERE i.k=s.k \
    RETURN s.seq AS source,i.seq AS item ORDER BY source,item";
const NODE_ORACLE: &str = "MATCH (i:Item),(s:Source) WHERE (i.k=s.k) OR false \
    RETURN s.seq AS source,i.seq AS item ORDER BY source,item";

struct ReadScope<'a>(&'a Connection);

impl<'a> ReadScope<'a> {
    fn new(db: &'a Connection) -> Self {
        // Standalone Core callers own a main read context across prepare and
        // consume. This savepoint is owned by this helper, not by the engine.
        db.execute_batch("SAVEPOINT phase22_inl_read").unwrap();
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
        self.0.execute_batch("RELEASE phase22_inl_read").unwrap();
    }
}

#[derive(Debug)]
struct ResultSet {
    columns: Vec<String>,
    physical: PhysicalPlan,
    rows: Vec<Vec<Value>>,
    summary: QuerySummary,
}

fn consume(
    db: &Connection,
    text: &str,
    options: ExecutionOptions,
) -> Result<ResultSet, QueryError> {
    let prepared = prepare(db, text, BTreeMap::new(), options)?;
    let columns = prepared.columns.clone();
    let physical = prepared.physical.clone();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(db, 3)?;
        rows.extend(batch.rows);
        if batch.done {
            let result = ResultSet {
                columns,
                physical,
                rows,
                summary: cursor.complete(db)?,
            };
            assert_profile_accounting(&result);
            return Ok(result);
        }
    }
}

fn assert_profile_accounting(result: &ResultSet) {
    if let Some(operators) = result.summary.metrics.operator_profile() {
        assert_eq!(
            operators
                .iter()
                .map(|operator| operator.db_hits)
                .sum::<u64>(),
            result.summary.metrics.db_hits,
            "all work has a real boundary: {:?}; {operators:?}",
            result.physical
        );
    }
}

fn read(db: &Connection, text: &str, options: ExecutionOptions) -> ResultSet {
    let _scope = ReadScope::new(db);
    consume(db, text, options).unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

fn query(db: &Connection, text: &str) -> Vec<Vec<Value>> {
    read(db, text, ExecutionOptions::default()).rows
}

fn write(db: &Connection, text: &str) -> ResultSet {
    // Do not add a test-owned rollback around mutation: that would hide an
    // engine failure to roll back its own provisional writes.
    consume(db, text, ExecutionOptions::default())
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

fn explain(db: &Connection, text: &str, options: ExecutionOptions) -> String {
    let result = read(db, &format!("EXPLAIN {text}"), options);
    let Value::String(plan) = &result.rows[0][0] else {
        panic!("EXPLAIN must return its physical plan")
    };
    plan.clone()
}

fn assert_bag(actual: &[Vec<Value>], expected: &[Vec<Value>]) {
    let sorted = |rows: &[Vec<Value>]| {
        let mut rows = rows
            .iter()
            .map(|row| format!("{row:?}"))
            .collect::<Vec<_>>();
        rows.sort();
        rows
    };
    assert_eq!(sorted(actual), sorted(expected));
}

fn assert_index_loop(result: &ResultSet) {
    let operators = result
        .summary
        .metrics
        .operator_profile()
        .expect("PROFILE metrics");
    assert!(
        operators
            .iter()
            .any(|operator| operator.operator == "IndexNestedLoop" && operator.rows > 0),
        "real IndexNestedLoop rows: {operators:?}"
    );
    assert!(
        operators
            .iter()
            .any(|operator| operator.operator == "IndexSeek" && operator.db_hits > 0),
        "real parameterized IndexSeek: {operators:?}"
    );
}

fn assert_two_directional_adjacencies(result: &ResultSet) {
    eprintln!(
        "Rare adjacency physical = {}; profile = {:?}",
        result.physical.explain(),
        result.summary.metrics.operator_profile()
    );
    let adjacency = result
        .physical
        .operators
        .iter()
        .enumerate()
        .filter_map(|(slot, operator)| match operator {
            PhysicalOperator::AdjacencySeek {
                from,
                relationship_type,
                direction,
            } => Some((slot, from, relationship_type, format!("{direction:?}"))),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(adjacency.len(), 2, "{:?}", result.physical);
    let mut directions = adjacency
        .iter()
        .map(|entry| entry.3.as_str())
        .collect::<Vec<_>>();
    directions.sort_unstable();
    assert_eq!(
        directions,
        ["Incoming", "Outgoing"],
        "{:?}; {:?}",
        result.physical,
        result.summary.metrics.operator_profile()
    );
    let profile = result.summary.metrics.operator_profile().unwrap();
    let (seed_slot, seed) = result
        .physical
        .operators
        .iter()
        .enumerate()
        .find(|(_, operator)| {
            matches!(
                operator,
                PhysicalOperator::NodeScan { .. } | PhysicalOperator::LabelIndexScan { .. }
            )
        })
        .expect("Rare node producer");
    match seed {
        PhysicalOperator::LabelIndexScan { variable, label } => {
            assert_eq!(variable, "b");
            assert_eq!(label, "Rare");
        }
        PhysicalOperator::NodeScan { variable } => {
            assert_eq!(variable, "b");
            assert_eq!(
                profile[seed_slot].rows, 1,
                "Scan must prove a single Rare seed: {profile:?}"
            );
        }
        _ => unreachable!(),
    }
    for (slot, from, relationship_type, _) in &adjacency {
        assert_eq!(from.as_str(), "b");
        assert_eq!(relationship_type.as_deref(), Some("EDGE"));
        let operator = &profile[*slot];
        assert_eq!(operator.operator, "AdjacencySeek");
        assert!(operator.rows > 0 && operator.db_hits > 0, "{profile:?}");
    }
    assert_ne!(profile[adjacency[0].0].id, profile[adjacency[1].0].id);
    assert!(
        !result
            .physical
            .operators
            .iter()
            .any(|operator| matches!(operator, PhysicalOperator::IndexSeek { .. }))
    );
}

fn counted(db: &Connection, text: &str) -> (ResultSet, u64) {
    let steps = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&steps);
    db.progress_handler(
        1,
        Some(move || {
            observed.fetch_add(1, Ordering::Relaxed);
            false
        }),
    )
    .unwrap();
    let result = read(db, text, ExecutionOptions::default());
    db.progress_handler(0, None::<fn() -> bool>).unwrap();
    (result, steps.load(Ordering::Relaxed))
}

fn fixture() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT NOT NULL,\
         database_id TEXT NOT NULL,storage_format INTEGER NOT NULL)",
    )
    .unwrap();
    db.execute(
        "INSERT INTO main._lithograph_meta VALUES(1,'lithograph-format-v1',\
         '00000000-0000-4000-8000-000000000024',?1)",
        [storage::STORAGE_FORMAT],
    )
    .unwrap();
    storage::create_storage_schema(&db).unwrap();
    let root = storage::initialize_root(&db).unwrap().root;
    let source = storage::intern_label(&db, "Source").unwrap();
    let item = storage::intern_label(&db, "Item").unwrap();
    let visible = storage::intern_label(&db, "Visible").unwrap();
    let edge = storage::intern_relationship_type(&db, "EDGE").unwrap();
    let pick = storage::intern_relationship_type(&db, "PICK").unwrap();
    let k = storage::intern_property_key(&db, "k").unwrap();
    let seq = storage::intern_property_key(&db, "seq").unwrap();
    let tag = storage::intern_property_key(&db, "tag").unwrap();
    let mut layer = LayerBuilder::default();
    seed_sources(&db, &mut layer, source, visible, k, seq);
    let sink = storage::allocate_node_id(&db).unwrap();
    layer.add_node(sink).unwrap();
    layer.add_label(sink, visible).unwrap();
    layer
        .set_property(OwnerKind::Node, sink, tag, PropertyValue::Integer(-1))
        .unwrap();
    let items = seed_items(&db, &mut layer, item, visible, k, seq, tag);
    seed_edges(&db, &mut layer, &items, sink, edge, k, seq);
    for (number, key) in [
        PropertyValue::Integer(5),
        PropertyValue::Float(5.0),
        PropertyValue::Integer(13),
    ]
    .into_iter()
    .enumerate()
    {
        let id = storage::allocate_relationship_id(&db).unwrap();
        layer
            .add_relationship(RelationshipRecord {
                id,
                source: sink,
                target: sink,
                type_id: pick,
            })
            .unwrap();
        layer
            .set_property(OwnerKind::Relationship, id, k, key)
            .unwrap();
        layer
            .set_property(
                OwnerKind::Relationship,
                id,
                seq,
                PropertyValue::Integer(number as i64),
            )
            .unwrap();
    }
    storage::commit_layer(
        &db,
        "main",
        root,
        None,
        &layer,
        &CommitMetadata {
            author: None,
            message: None,
            committed_at: 1,
        },
    )
    .unwrap();
    install_schema(&db);
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    db
}

fn seed_sources(
    db: &Connection,
    layer: &mut LayerBuilder,
    source: i64,
    visible: i64,
    k: i64,
    seq: i64,
) {
    let keys = [
        Some(PropertyValue::Integer(5)),
        Some(PropertyValue::Float(5.0)),
        Some(PropertyValue::Integer(13)),
        None,
        Some(PropertyValue::Integer(99_999)),
    ];
    for (number, key) in keys.into_iter().enumerate() {
        let id = storage::allocate_node_id(db).unwrap();
        layer.add_node(id).unwrap();
        layer.add_label(id, source).unwrap();
        if number != 1 {
            layer.add_label(id, visible).unwrap();
        }
        layer
            .set_property(
                OwnerKind::Node,
                id,
                seq,
                PropertyValue::Integer(number as i64),
            )
            .unwrap();
        if let Some(key) = key {
            layer.set_property(OwnerKind::Node, id, k, key).unwrap();
        }
    }
}

fn seed_items(
    db: &Connection,
    layer: &mut LayerBuilder,
    item: i64,
    visible: i64,
    k: i64,
    seq: i64,
    tag: i64,
) -> Vec<i64> {
    (0..ITEMS)
        .map(|number| {
            let id = storage::allocate_node_id(db).unwrap();
            layer.add_node(id).unwrap();
            layer.add_label(id, item).unwrap();
            if number % 64 < 32 {
                layer.add_label(id, visible).unwrap();
            }
            for (property, value) in [(k, number % 32), (seq, number), (tag, number)] {
                layer
                    .set_property(OwnerKind::Node, id, property, PropertyValue::Integer(value))
                    .unwrap();
            }
            id
        })
        .collect()
}

fn seed_edges(
    db: &Connection,
    layer: &mut LayerBuilder,
    items: &[i64],
    sink: i64,
    edge: i64,
    k: i64,
    seq: i64,
) {
    let endpoints = items
        .iter()
        .enumerate()
        .map(|(number, id)| (*id, sink, number as i64, number as i64 % 32))
        .chain([
            (items[5], sink, ITEMS, 5),
            (items[13], items[13], ITEMS + 1, 13),
        ]);
    for (source, target, number, key) in endpoints {
        let id = storage::allocate_relationship_id(db).unwrap();
        layer
            .add_relationship(RelationshipRecord {
                id,
                source,
                target,
                type_id: edge,
            })
            .unwrap();
        for (property, value) in [(k, key), (seq, number)] {
            layer
                .set_property(
                    OwnerKind::Relationship,
                    id,
                    property,
                    PropertyValue::Integer(value),
                )
                .unwrap();
        }
    }
}

fn install_schema(db: &Connection) {
    for (name, pattern, variable) in [
        ("source", "(s:Source)", "s"),
        ("item", "(i:Item)", "i"),
        ("edge", "()-[r:EDGE]-()", "r"),
        ("pick", "()-[p:PICK]-()", "p"),
        ("empty", "(e:Empty)", "e"),
    ] {
        write(
            db,
            &format!(
                "CREATE CONSTRAINT {name}_k FOR {pattern} REQUIRE {variable}.k IS :: INTEGER | FLOAT"
            ),
        );
        write(
            db,
            &format!(
                "CREATE CONSTRAINT {name}_seq FOR {pattern} REQUIRE {variable}.seq IS :: INTEGER"
            ),
        );
    }
    write(db, "CREATE RANGE INDEX item_key FOR (i:Item) ON (i.k)");
    write(
        db,
        "CREATE RANGE INDEX edge_key FOR ()-[r:EDGE]-() ON (r.k)",
    );
}

fn expected_nodes() -> Vec<Vec<Value>> {
    [5, 5, 13]
        .into_iter()
        .enumerate()
        .flat_map(|(source, key)| {
            (0..ITEMS)
                .filter(move |item| item % 32 == key)
                .map(move |item| vec![Value::Integer(source as i64), Value::Integer(item)])
        })
        .collect()
}

#[test]
fn typed_node_probe_reduces_real_vm_and_preserves_numeric_duplicates_and_nulls() {
    let db = fixture();
    let before = storage::branch_head(&db, "main").unwrap();
    let plan = explain(&db, NODE_JOIN, ExecutionOptions::default());
    assert!(
        plan.contains("IndexNestedLoop") && plan.contains("item_key"),
        "{plan}"
    );
    assert!(!explain(&db, NODE_ORACLE, ExecutionOptions::default()).contains("IndexNestedLoop"));
    let (actual, fast_vm) = counted(&db, &format!("PROFILE {NODE_JOIN}"));
    let (original, old_vm) = counted(&db, &format!("PROFILE {NODE_ORACLE}"));
    assert_eq!(actual.columns, ["source", "item"]);
    assert_eq!(actual.rows, expected_nodes());
    assert_eq!(actual.rows, original.rows);
    assert_index_loop(&actual);
    assert!(
        actual.summary.metrics.db_hits * 2 < original.summary.metrics.db_hits,
        "dbHits {} / {}",
        actual.summary.metrics.db_hits,
        original.summary.metrics.db_hits
    );
    assert!(
        fast_vm * 2 < old_vm,
        "actual SQLite VM work {fast_vm} / {old_vm}"
    );
    for _ in 0..3 {
        assert_eq!(explain(&db, NODE_JOIN, ExecutionOptions::default()), plan);
    }
    assert_eq!(storage::branch_head(&db, "main").unwrap(), before);
}

#[test]
fn repeated_variable_names_across_clauses_use_distinct_index_profile_slots() {
    let db = fixture();
    let text = "MATCH (s:Source),(i:Item) WHERE s.k=i.k WITH s AS seed \
        MATCH (s:Source),(i:Item) WHERE s.k=i.k \
        RETURN seed.seq AS previous,s.seq AS source,i.seq AS item";
    let oracle = "MATCH (s:Source),(i:Item) WHERE (s.k=i.k) OR false WITH s AS seed \
        MATCH (s:Source),(i:Item) WHERE (s.k=i.k) OR false \
        RETURN seed.seq AS previous,s.seq AS source,i.seq AS item";
    #[cfg(feature = "test-support")]
    {
        lithograph_core::performance::set_enabled(true);
        lithograph_core::performance::reset();
    }
    let actual = read(&db, &format!("PROFILE {text}"), ExecutionOptions::default());
    #[cfg(feature = "test-support")]
    {
        let counters = lithograph_core::performance::snapshot();
        lithograph_core::performance::set_enabled(false);
        eprintln!("repeated-clause prepare/PROFILE counters = {counters:?}");
        assert!(counters.planner_states <= 4_096, "{counters:?}");
        assert!(counters.planner_candidates <= 1_024, "{counters:?}");
        assert!(counters.planner_bytes <= 256 * 1_024, "{counters:?}");
    }
    let single = expected_nodes();
    let expected = single
        .iter()
        .flat_map(|first| {
            single
                .iter()
                .map(move |second| vec![first[0].clone(), second[0].clone(), second[1].clone()])
        })
        .collect::<Vec<_>>();
    assert_eq!(actual.columns, ["previous", "source", "item"]);
    assert_eq!(actual.rows.len(), 576);
    assert_bag(&actual.rows, &expected);
    assert_bag(&actual.rows, &query(&db, oracle));
    let slots = actual
        .physical
        .operators
        .iter()
        .enumerate()
        .filter_map(|(slot, operator)| {
            matches!(operator, PhysicalOperator::IndexSeek { .. }).then_some(slot)
        })
        .collect::<Vec<_>>();
    assert_eq!(slots.len(), 2, "{:?}", actual.physical);
    let profile = actual.summary.metrics.operator_profile().unwrap();
    let first = &profile[slots[0]];
    let second = &profile[slots[1]];
    assert_ne!(first.id, second.id);
    for operator in [first, second] {
        assert_eq!(operator.operator, "IndexSeek");
        assert!(operator.rows > 0 && operator.db_hits > 0, "{profile:?}");
    }
    assert!(second.rows > first.rows, "{profile:?}");
    assert!(second.db_hits > first.db_hits, "{profile:?}");
}

#[test]
fn mixed_exact_join_and_fallback_keep_separate_source_counts() {
    let db = fixture();
    let text = "MATCH (s:Source),(i:Item) WHERE s.k=i.k WITH s AS seed \
        MATCH (s:Source),(i:Item) WHERE (s.k=i.k) OR false \
        RETURN seed.seq AS previous,s.seq AS source,i.seq AS item";
    let oracle = "MATCH (s:Source),(i:Item) WHERE (s.k=i.k) OR false WITH s AS seed \
        MATCH (s:Source),(i:Item) WHERE (s.k=i.k) OR false \
        RETURN seed.seq AS previous,s.seq AS source,i.seq AS item";
    let actual = read(&db, &format!("PROFILE {text}"), ExecutionOptions::default());
    assert_eq!(actual.rows.len(), 576);
    assert_bag(&actual.rows, &query(&db, oracle));
    let source_slots = actual
        .physical
        .operators
        .iter()
        .enumerate()
        .filter_map(|(slot, operator)| match operator {
            PhysicalOperator::LabelIndexScan { variable, label }
                if variable == "s" && label == "Source" =>
            {
                Some(slot)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(source_slots.len(), 2, "{:?}", actual.physical);
    assert_eq!(
        actual
            .physical
            .operators
            .iter()
            .filter(|operator| { matches!(operator, PhysicalOperator::IndexNestedLoop) })
            .count(),
        1,
        "only the first clause has a proven parameterized access: {:?}",
        actual.physical
    );
    let profile = actual.summary.metrics.operator_profile().unwrap();
    let first = &profile[source_slots[0]];
    let second = &profile[source_slots[1]];
    eprintln!(
        "mixed exact/fallback physical = {}; profile = {profile:?}",
        actual.physical.explain()
    );
    assert_eq!(
        first.rows, 5,
        "Source producer must not count complete join output: {profile:?}"
    );
    assert!(first.db_hits > 0 && second.db_hits > 0, "{profile:?}");
    assert!(second.rows > first.rows, "{profile:?}");
    assert!(second.db_hits > first.db_hits, "{profile:?}");
    assert_index_loop(&actual);
}

fn singleton_fixture() -> Connection {
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
    write(&db, "CREATE (:Rare:Small:Target:Typed {k:1}) FINISH");
    write(
        &db,
        "UNWIND range(2,101) AS key CREATE (:Typed {k:key}) FINISH",
    );
    for label in ["Typed", "Target"] {
        write(
            &db,
            &format!(
                "CREATE CONSTRAINT singleton_{label}_k FOR (n:{label}) REQUIRE n.k IS :: INTEGER"
            ),
        );
    }
    write(
        &db,
        "CREATE RANGE INDEX singleton_key FOR (n:Typed) ON (n.k)",
    );
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    db
}

#[test]
fn standalone_bound_match_never_steals_the_exact_rare_profile_slot() {
    let db = singleton_fixture();
    for (with_projection, points, first, text, oracle) in [
        (
            true,
            1,
            "MATCH (a:Rare:Typed),(b:Target) WHERE a.k=b.k RETURN a.k",
            "MATCH (a:Rare:Typed),(b:Target) WHERE a.k=b.k WITH a MATCH (a) RETURN a.k",
            "MATCH (a:Rare:Typed),(b:Target) WHERE (a.k=b.k) OR false WITH a MATCH (a) RETURN a.k",
        ),
        (
            false,
            1,
            "MATCH (a:Rare),(b:Small) WHERE a IS NOT NULL RETURN a.k",
            "MATCH (a:Rare),(b:Small) WHERE a IS NOT NULL MATCH (a) RETURN a.k",
            "MATCH (a:Rare),(b:Small) WHERE (a IS NOT NULL) OR false MATCH (a) RETURN a.k",
        ),
        (
            false,
            2,
            "MATCH (a:Rare),(b:Small) WHERE a IS NOT NULL RETURN a.k",
            "MATCH (a:Rare),(b:Small) WHERE a IS NOT NULL MATCH (a) MATCH (a) RETURN a.k",
            "MATCH (a:Rare),(b:Small) WHERE (a IS NOT NULL) OR false MATCH (a) MATCH (a) RETURN a.k",
        ),
    ] {
        assert_bound_point_case(&db, with_projection, points, first, text, oracle);
    }
    assert_bound_scalar_inputs(&db);
    assert_hidden_bound_inputs();
}

fn assert_bound_point_case(
    db: &Connection,
    with_projection: bool,
    points: usize,
    first: &str,
    text: &str,
    oracle: &str,
) {
    let baseline = read(db, &format!("PROFILE {first}"), ExecutionOptions::default());
    let actual = read(db, &format!("PROFILE {text}"), ExecutionOptions::default());
    assert_eq!(actual.rows, vec![vec![Value::Integer(1)]]);
    assert_eq!(actual.rows, baseline.rows);
    assert_bag(&actual.rows, &query(db, oracle));
    let ordinary = read(db, text, ExecutionOptions::default());
    assert_eq!(ordinary.rows, actual.rows);
    assert_eq!(ordinary.physical, actual.physical);
    assert_eq!(
        explain(db, text, ExecutionOptions::default()),
        actual.physical.explain()
    );
    if with_projection {
        for result in [&baseline, &actual] {
            assert!(
                result.physical.operators.iter().any(|operator| {
                    matches!(
                        operator,
                        PhysicalOperator::NestedLoop | PhysicalOperator::IndexNestedLoop
                    )
                }),
                "the first typed join must own exact sites: {:?}",
                result.physical
            );
        }
    }
    let rare_slot = |result: &ResultSet| {
        result
            .physical
            .operators
            .iter()
            .position(|operator| {
                matches!(operator, PhysicalOperator::LabelIndexScan { variable, label }
                if variable == "a" && label == "Rare")
            })
            .expect("the exact safe match starts from its Rare singleton")
    };
    let profile = actual.summary.metrics.operator_profile().unwrap();
    let previous = &baseline.summary.metrics.operator_profile().unwrap()[rare_slot(&baseline)];
    let seed = rare_slot(&actual);
    eprintln!(
        "standalone bound WITH={with_projection} points={points} physical = {}; profile = {profile:?}; total dbHits = {}; baseline dbHits = {}",
        actual.physical.explain(),
        actual.summary.metrics.db_hits,
        baseline.summary.metrics.db_hits
    );
    assert_eq!(
        profile[seed].rows, 1,
        "the bound clause cannot count a second Rare scan: {profile:?}"
    );
    assert_eq!(
        profile[seed].db_hits, previous.db_hits,
        "bound validation cannot steal the exact slot: {profile:?}"
    );
    assert!(
        actual.summary.metrics.db_hits > baseline.summary.metrics.db_hits,
        "bound point validation remains in total dbHits: {} / {}",
        actual.summary.metrics.db_hits,
        baseline.summary.metrics.db_hits
    );
    let point_slots = actual
        .physical
        .operators
        .iter()
        .enumerate()
        .filter_map(|(slot, operator)| {
            matches!(operator, PhysicalOperator::NodeIdSeek { variable } if variable == "a")
                .then_some(slot)
        })
        .collect::<Vec<_>>();
    assert_eq!(point_slots.len(), points, "{:?}", actual.physical);
    for slot in point_slots {
        assert_eq!(profile[slot].id, slot as u64);
        assert_eq!(profile[slot].operator, "NodeIdSeek");
        assert_eq!(profile[slot].rows, 1, "{profile:?}");
        assert_eq!(profile[slot].db_hits, 1, "{profile:?}");
    }
}

fn assert_bound_scalar_inputs(db: &Connection) {
    for value in ["null", "7", "'n:1'"] {
        let actual = format!(
            "PROFILE MATCH (a:Rare:Typed),(b:Target) WHERE a.k=b.k \
            WITH {value} AS a MATCH (a) RETURN a.k"
        );
        let oracle = format!(
            "PROFILE MATCH (a:Rare:Typed),(b:Target) WHERE (a.k=b.k) OR false \
            WITH {value} AS a MATCH (a) RETURN a.k"
        );
        let run = |text: &str| {
            let _scope = ReadScope::new(db);
            consume(db, text, ExecutionOptions::default())
        };
        match (run(&actual), run(&oracle)) {
            (Ok(actual), Ok(original)) => assert_bag(&actual.rows, &original.rows),
            (Err(actual), Err(original)) => assert_eq!(actual.kind, original.kind, "{value}"),
            (actual, original) => panic!("bound input {value}: {actual:?} / {original:?}"),
        }
    }
}

fn assert_hidden_bound_inputs() {
    let mut errors = Vec::new();
    for predicate in ["a.k=b.k", "(a.k=b.k) OR false"] {
        let current = singleton_fixture();
        let before = storage::branch_head(&current, "main").unwrap();
        let options =
            ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Rare"]}}"#).unwrap();
        let text = format!(
            "PROFILE MATCH (a:Rare:Typed),(b:Target) WHERE {predicate} \
            REMOVE a:Rare WITH a MATCH (a) RETURN a.k"
        );
        // A clause cannot leave a surviving Node outside its active view.
        let error = consume(&current, &text, options).unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::GraphViewViolation);
        assert_eq!(storage::branch_head(&current, "main").unwrap(), before);
        assert_eq!(
            query(&current, "MATCH (n:Rare) RETURN n.k"),
            vec![vec![Value::Integer(1)]]
        );
        errors.push(error.kind);
        let hidden =
            ExecutionOptions::parse_text(r#"{"graphView":{"excludeAnyLabels":["Rare"]}}"#).unwrap();
        let read_only = format!(
            "PROFILE MATCH (a:Rare:Typed),(b:Target) WHERE {predicate} \
            WITH a MATCH (a) RETURN a.k"
        );
        let result = read(&current, &read_only, hidden);
        assert!(result.rows.is_empty());
        let profile = result.summary.metrics.operator_profile().unwrap();
        for operator in profile
            .iter()
            .filter(|operator| operator.operator == "NodeIdSeek")
        {
            assert_eq!(operator.rows, 0);
            assert_eq!(
                operator.db_hits, 0,
                "invisible outer never runs a point: {profile:?}"
            );
        }
    }
    assert_eq!(errors[0], errors[1]);
}

#[test]
fn an_already_bound_target_cannot_be_reported_as_an_index_probe() {
    let db = fixture();
    write(&db, "CREATE (:Rare:Target:Typed {k:1}) FINISH");
    write(
        &db,
        "UNWIND range(2,101) AS key CREATE (:Typed {k:key}) FINISH",
    );
    for label in ["Typed", "Target"] {
        write(
            &db,
            &format!("CREATE CONSTRAINT bound_{label}_k FOR (n:{label}) REQUIRE n.k IS :: INTEGER"),
        );
    }
    write(&db, "CREATE RANGE INDEX typed_key FOR (n:Typed) ON (n.k)");
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    let text = "MATCH (n:Rare),(m:Target),(n:Typed) WHERE n=m AND n.k=m.k RETURN n.k,m.k";
    let oracle = "MATCH (n:Rare),(m:Target),(n:Typed) WHERE (n=m AND n.k=m.k) OR false \
        RETURN n.k,m.k";
    let actual = read(&db, &format!("PROFILE {text}"), ExecutionOptions::default());
    assert_eq!(
        actual.rows,
        vec![vec![Value::Integer(1), Value::Integer(1)]]
    );
    assert_eq!(actual.rows, query(&db, oracle));
    let profile = actual.summary.metrics.operator_profile().unwrap();
    eprintln!(
        "bound target physical = {}; profile = {profile:?}",
        actual.physical.explain()
    );
    assert!(
        !actual
            .physical
            .operators
            .iter()
            .any(|operator| matches!(operator, PhysicalOperator::IndexNestedLoop)),
        "already-bound n bypasses Index access: {:?}; {profile:?}",
        actual.physical
    );
    assert!(
        !profile
            .iter()
            .any(|operator| operator.operator == "IndexSeek"),
        "the bound target uses membership validation: {profile:?}"
    );
}

#[test]
fn reversed_same_type_adjacency_parts_keep_distinct_direction_and_profile_slots() {
    let db = fixture();
    // Existing fixture edges point to a Sink. Add one incoming/outgoing Item
    // connector at the Rare seed so both adjacency operators produce rows.
    write(
        &db,
        "MATCH (n:Item) WHERE n.seq=5 SET n:Rare \
         CREATE (a:Item {seq:1000,k:5,tag:1000})-[:EDGE {seq:258,k:5}]->(n),\
         (n)-[:EDGE {seq:259,k:5}]->(c:Item {seq:1001,k:5,tag:1001}) FINISH",
    );
    let text = "MATCH (a:Item)-[r:EDGE]->(b:Rare),(b)-[t:EDGE]->(c:Item) \
        RETURN a.seq,b.seq,c.seq,r.seq,t.seq";
    let oracle = "MATCH (a:Item)-[r:EDGE]->(b:Rare),(b)-[t:EDGE]->(c:Item) \
        WHERE true OR false RETURN a.seq,b.seq,c.seq,r.seq,t.seq";
    let (actual, actual_vm) = counted(&db, &format!("PROFILE {text}"));
    assert_eq!(
        actual.rows,
        vec![vec![
            Value::Integer(1000),
            Value::Integer(5),
            Value::Integer(1001),
            Value::Integer(258),
            Value::Integer(259),
        ]]
    );
    let (original, original_vm) = counted(&db, &format!("PROFILE {oracle}"));
    assert_bag(&actual.rows, &original.rows);
    eprintln!(
        "Rare named VM {actual_vm}/{original_vm}; dbHits {}/{}",
        actual.summary.metrics.db_hits, original.summary.metrics.db_hits
    );
    assert_two_directional_adjacencies(&actual);
    assert!(
        actual_vm < original_vm,
        "Rare seed VM {actual_vm}/{original_vm}"
    );
    assert!(
        actual.summary.metrics.db_hits < original.summary.metrics.db_hits,
        "Rare seed dbHits {}/{}",
        actual.summary.metrics.db_hits,
        original.summary.metrics.db_hits
    );

    let anonymous = "MATCH (:Item)-[:EDGE]->(b:Rare),(b)-[:EDGE]->(:Item) RETURN b.seq";
    let anonymous_oracle = "MATCH (:Item)-[:EDGE]->(b:Rare),(b)-[:EDGE]->(:Item) \
        WHERE true OR false RETURN b.seq";
    let (actual, actual_vm) = counted(&db, &format!("PROFILE {anonymous}"));
    assert_eq!(actual.rows, vec![vec![Value::Integer(5)]]);
    let (original, original_vm) = counted(&db, &format!("PROFILE {anonymous_oracle}"));
    assert_bag(&actual.rows, &original.rows);
    eprintln!(
        "Rare anonymous VM {actual_vm}/{original_vm}; dbHits {}/{}",
        actual.summary.metrics.db_hits, original.summary.metrics.db_hits
    );
    assert_two_directional_adjacencies(&actual);
    assert!(
        actual_vm < original_vm,
        "Rare seed VM {actual_vm}/{original_vm}"
    );
    assert!(
        actual.summary.metrics.db_hits < original.summary.metrics.db_hits,
        "Rare seed dbHits {}/{}",
        actual.summary.metrics.db_hits,
        original.summary.metrics.db_hits
    );
}

#[test]
fn relationship_probe_keeps_direction_parallel_identity_and_self_loop_rows() {
    let db = fixture();
    for pattern in [
        "(a)-[r:EDGE]->(b),(s:Source)",
        "(b)<-[r:EDGE]-(a),(s:Source)",
        "(a)-[r:EDGE]-(b),(s:Source)",
    ] {
        let undirected = pattern == "(a)-[r:EDGE]-(b),(s:Source)";
        let projection = "s.seq,r.seq,a.tag,b.tag,elementId(r) ORDER BY s.seq,r.seq,a.tag,b.tag";
        let text = format!("MATCH {pattern} WHERE r.k=s.k RETURN {projection}");
        let oracle = format!("MATCH {pattern} WHERE (r.k=s.k) OR false RETURN {projection}");
        let plan = explain(&db, &text, ExecutionOptions::default());
        assert!(
            plan.contains("IndexNestedLoop") && plan.contains("edge_key"),
            "{plan}"
        );
        let actual = read(&db, &format!("PROFILE {text}"), ExecutionOptions::default());
        let expected = query(&db, &oracle);
        assert_eq!(actual.rows, expected);
        assert_eq!(actual.rows.len(), if undirected { 53 } else { 27 });
        assert_index_loop(&actual);
        assert_eq!(
            actual
                .rows
                .iter()
                .filter(|row| row[1] == Value::Integer(ITEMS))
                .count(),
            if undirected { 4 } else { 2 }
        );
        assert_eq!(
            actual
                .rows
                .iter()
                .filter(|row| row[1] == Value::Integer(ITEMS + 1))
                .count(),
            1
        );
    }
}

#[test]
fn relationship_outer_and_original_end_node_use_real_node_index_probes() {
    let db = fixture();
    let text = "MATCH (i:Item),()-[p:PICK]->() WHERE i.k=p.k \
        RETURN p.seq AS source,i.seq AS item ORDER BY source,item";
    let actual = read(&db, &format!("PROFILE {text}"), ExecutionOptions::default());
    assert_eq!(actual.rows, expected_nodes());
    assert_index_loop(&actual);
    assert!(explain(&db, text, ExecutionOptions::default()).contains("item_key"));

    let text = "MATCH (b)<-[r:EDGE]-(i:Item),(s:Source) WHERE i.k=s.k \
        RETURN s.seq,r.seq,i.seq,b.tag ORDER BY s.seq,r.seq";
    let oracle = "MATCH (b)<-[r:EDGE]-(i:Item),(s:Source) WHERE (i.k=s.k) OR false \
        RETURN s.seq,r.seq,i.seq,b.tag ORDER BY s.seq,r.seq";
    let actual = read(&db, &format!("PROFILE {text}"), ExecutionOptions::default());
    assert_eq!(actual.rows, query(&db, oracle));
    assert_eq!(actual.rows.len(), 27);
    assert_index_loop(&actual);
    assert!(explain(&db, text, ExecutionOptions::default()).contains("item_key"));
}

#[test]
fn restarting_inner_relationship_probes_preserves_the_match_used_set() {
    let db = fixture();
    let text = "MATCH ()-[r:EDGE]->(),()-[q:EDGE]->(),(s:Source) \
        WHERE r.k=s.k AND q.k=s.k AND s.seq=0 \
        RETURN r.seq,q.seq ORDER BY r.seq,q.seq";
    let result = read(&db, &format!("PROFILE {text}"), ExecutionOptions::default());
    let identities = [5, 37, 69, 101, 133, 165, 197, 229, ITEMS];
    let expected = identities
        .into_iter()
        .flat_map(|first| {
            identities
                .into_iter()
                .filter(move |second| *second != first)
                .map(move |second| vec![Value::Integer(first), Value::Integer(second)])
        })
        .collect::<Vec<_>>();
    assert_eq!(result.rows, expected);
    assert_eq!(result.rows.len(), 72);
    assert_index_loop(&result);
}

#[test]
fn string_range_probes_and_text_fallback_keep_duplicate_and_missing_source_keys() {
    let db = fixture();
    write(
        &db,
        "UNWIND range(0,63) AS number CREATE (:Word {seq:number,\
         k:CASE WHEN number IN [5,9] THEN 'alpha' ELSE toString(number) END}) FINISH",
    );
    write(
        &db,
        "CREATE (:WordSource {seq:0,k:'alpha'}),(:WordSource {seq:1,k:'alpha'}),\
         (:WordSource {seq:2}) FINISH",
    );
    for label in ["Word", "WordSource"] {
        for (property, kind) in [("k", "STRING"), ("seq", "INTEGER")] {
            write(
                &db,
                &format!(
                    "CREATE CONSTRAINT {label}_{property} FOR (n:{label}) REQUIRE n.{property} IS :: {kind}"
                ),
            );
        }
    }
    let text = "MATCH (w:Word),(s:WordSource) WHERE w.k=s.k \
        RETURN s.seq,w.seq ORDER BY s.seq,w.seq";
    let oracle = "MATCH (w:Word),(s:WordSource) WHERE (w.k=s.k) OR false \
        RETURN s.seq,w.seq ORDER BY s.seq,w.seq";
    let expected = [[0, 5], [0, 9], [1, 5], [1, 9]]
        .into_iter()
        .map(|row| row.into_iter().map(Value::Integer).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    for kind in ["RANGE", "TEXT"] {
        write(
            &db,
            &format!("CREATE {kind} INDEX words FOR (w:Word) ON (w.k)"),
        );
        storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
        let actual = read(&db, &format!("PROFILE {text}"), ExecutionOptions::default());
        assert_eq!(actual.rows, expected, "{kind}");
        assert_eq!(actual.rows, query(&db, oracle), "{kind}");
        if kind == "RANGE" {
            assert_index_loop(&actual);
        } else {
            // Text has no measured key distribution on this path. Unknown
            // fanout keeps a real fallback, not an invented Index NL cost.
            assert!(
                !actual
                    .physical
                    .operators
                    .iter()
                    .any(|operator| { matches!(operator, PhysicalOperator::IndexNestedLoop) })
            );
        }
        write(&db, "DROP INDEX words");
    }
}

#[test]
fn graph_view_and_query_access_remain_final_probe_guards() {
    let db = fixture();
    let visible =
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Visible"]}}"#).unwrap();
    let actual = read(&db, NODE_JOIN, visible.clone());
    let oracle = read(&db, NODE_ORACLE, visible.clone());
    assert_eq!(actual.rows, oracle.rows);
    assert_eq!(actual.rows.len(), 8);
    let pattern = "MATCH (a)-[r:EDGE]->(b),(s:Source) WHERE r.k=s.k RETURN s.seq,r.seq";
    let oracle = "MATCH (a)-[r:EDGE]->(b),(s:Source) WHERE (r.k=s.k) OR false RETURN s.seq,r.seq";
    let actual = read(&db, pattern, visible.clone());
    assert_bag(&actual.rows, &read(&db, oracle, visible).rows);
    assert_eq!(actual.rows.len(), 10);

    let hidden =
        ExecutionOptions::parse_text(r#"{"readAccess":{"query":{"denyProperties":["k"]}}}"#)
            .unwrap();
    assert!(read(&db, NODE_JOIN, hidden.clone()).rows.is_empty());
    assert!(!explain(&db, NODE_JOIN, hidden).contains("IndexNestedLoop"));
    let result_mask =
        ExecutionOptions::parse_text(r#"{"readAccess":{"result":{"denyProperties":["k"]}}}"#)
            .unwrap();
    let actual = read(
        &db,
        "PROFILE MATCH (i:Item),(s:Source) WHERE i.k=s.k RETURN s.k,i.k,s,i",
        result_mask,
    );
    assert_eq!(actual.rows.len(), 24);
    assert_bag(
        &actual.rows,
        &query(
            &db,
            "MATCH (i:Item),(s:Source) WHERE i.k=s.k RETURN s.k,i.k,s,i",
        ),
    );
    for row in &actual.rows {
        // Result-only filtering belongs to the public JSON encoder. Core
        // runtime/spill Values and explicit scalars retain the complete data.
        assert_ne!(row[0], Value::Null);
        assert_ne!(row[1], Value::Null);
        for (position, value) in row[2..].iter().enumerate() {
            let Value::Node(node) = value else {
                panic!("projected Node")
            };
            assert_eq!(node.properties.get("k"), Some(&row[position]));
            assert!(node.properties.contains_key("seq"));
        }
    }
    assert_index_loop(&actual);
}

#[test]
fn historical_definition_identity_and_new_commits_do_not_reuse_old_probe_bindings() {
    let db = fixture();
    let old = storage::branch_head(&db, "main").unwrap();
    assert_eq!(query(&db, NODE_JOIN), expected_nodes());
    write(&db, "MATCH (i:Item) WHERE i.seq=5 SET i.k=42 FINISH");
    let changed = query(&db, NODE_JOIN);
    assert_eq!(changed.len(), 22);
    assert!(!changed.iter().any(|row| row[1] == Value::Integer(5)));
    let mut historical = ExecutionOptions::default();
    historical.snapshot = SnapshotSelector::Commit(old.to_hex());
    let result = read(&db, &format!("PROFILE {NODE_JOIN}"), historical.clone());
    assert_eq!(result.rows, expected_nodes());
    assert_index_loop(&result);
    write(&db, "DROP INDEX item_key");
    write(&db, "CREATE RANGE INDEX item_key FOR (i:Item) ON (i.seq)");
    let current = explain(&db, NODE_JOIN, ExecutionOptions::default());
    assert!(
        !current.contains("IndexNestedLoop"),
        "different same-name definition: {current}"
    );
    assert_eq!(query(&db, NODE_JOIN), changed);
    assert!(explain(&db, NODE_JOIN, historical.clone()).contains("IndexNestedLoop"));
    assert_eq!(read(&db, NODE_JOIN, historical).rows, expected_nodes());
}

#[test]
fn join_write_prefix_preserves_rollback_and_reads_new_staged_values_after_barrier() {
    let db = fixture();
    let before = storage::branch_head(&db, "main").unwrap();
    let bad = "MATCH (i:Item),(s:Source) WHERE i.k=s.k AND s.seq=0 \
        SET i.k=42 RETURN 1/0";
    let error = consume(&db, bad, ExecutionOptions::default()).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Type);
    assert_eq!(storage::branch_head(&db, "main").unwrap(), before);
    assert_eq!(query(&db, NODE_JOIN), expected_nodes());
    assert!(db.is_autocommit());

    let prefix = "MATCH (i:Item),(s:Source) WHERE i.k=s.k AND s.seq=0 \
        SET i.touched=1 RETURN i.seq ORDER BY i.seq";
    let result = write(&db, &format!("PROFILE {prefix}"));
    assert_eq!(
        result.rows,
        (0..ITEMS)
            .filter(|item| item % 32 == 5)
            .map(|item| vec![Value::Integer(item)])
            .collect::<Vec<_>>()
    );
    assert_eq!(result.summary.counters.properties_set, 8);
    assert_index_loop(&result);
    let before_aggregate = storage::branch_head(&db, "main").unwrap();

    let text = "MATCH (i:Item),(s:Source) WHERE i.k=s.k AND s.seq=0 SET i.k=42 \
        WITH count(i) AS changed MATCH (j:Item),(u:Source) WHERE j.k=u.k \
        RETURN changed,u.seq,j.seq ORDER BY u.seq,j.seq";
    let result = write(&db, &format!("PROFILE {text}"));
    let expected = (0..ITEMS)
        .filter(|item| item % 32 == 13)
        .map(|item| vec![Value::Integer(8), Value::Integer(2), Value::Integer(item)])
        .collect::<Vec<_>>();
    assert_eq!(result.rows, expected);
    assert_eq!(result.summary.counters.properties_set, 8);
    // The aggregate program has no StepCursor consumer before its write;
    // preserve its real eager fallback instead of advertising a Join plan.
    assert!(
        !result
            .physical
            .operators
            .iter()
            .any(|operator| matches!(operator, PhysicalOperator::IndexNestedLoop))
    );
    let profile = result.summary.metrics.operator_profile().unwrap();
    assert!(
        profile
            .iter()
            .any(|operator| { operator.operator == "LabelIndexScan" && operator.db_hits > 0 }),
        "{profile:?}"
    );
    assert_ne!(storage::branch_head(&db, "main").unwrap(), before_aggregate);
    assert_eq!(query(&db, NODE_JOIN).len(), 8);
    let mut historical = ExecutionOptions::default();
    historical.snapshot = SnapshotSelector::Commit(before.to_hex());
    assert_eq!(read(&db, NODE_JOIN, historical).rows, expected_nodes());
}

#[test]
fn program_star_columns_and_repeated_outer_rows_preserve_binding_order_and_bag() {
    let db = fixture();
    let text = "UNWIND [10,10,20] AS seed MATCH (i:Item),(s:Source) WHERE i.k=s.k \
        WITH *,i.seq AS item,s.seq AS source RETURN seed,source,item ORDER BY seed,source,item";
    let result = read(&db, &format!("PROFILE {text}"), ExecutionOptions::default());
    assert_eq!(result.columns, ["seed", "source", "item"]);
    let expected = [10, 10, 20]
        .into_iter()
        .flat_map(|seed| {
            expected_nodes()
                .into_iter()
                .map(move |row| vec![Value::Integer(seed), row[0].clone(), row[1].clone()])
        })
        .collect::<Vec<_>>();
    assert_eq!(result.rows.len(), 72);
    assert_bag(&result.rows, &expected);
    assert_index_loop(&result);
    let star = "MATCH (i:Item),(s:Source) WHERE i.k=s.k RETURN *";
    let conservative = "MATCH (i:Item),(s:Source) WHERE (i.k=s.k) OR false RETURN *";
    let actual = read(&db, star, ExecutionOptions::default());
    let expected = read(&db, conservative, ExecutionOptions::default());
    assert_eq!(actual.columns, expected.columns);
    assert_bag(&actual.rows, &expected.rows);
}

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

#[test]
fn zero_outer_does_not_bind_an_index_or_evaluate_throwing_inner_predicates() {
    let db = fixture();
    let before = temp_catalog(&db);
    let head = storage::branch_head(&db, "main").unwrap();
    for text in [
        "MATCH (e:Empty),(i:Item) WHERE i.k=e.k RETURN i.seq",
        "MATCH (e:Empty),(i:Item) WHERE i.k=e.k AND 1/0=0 RETURN i.seq",
        "UNWIND [] AS seed MATCH (i:Item),(s:Source) WHERE i.k=s.k AND 1/0=0 RETURN i.seq",
    ] {
        assert!(query(&db, text).is_empty(), "{text}");
        assert_eq!(
            temp_catalog(&db),
            before,
            "zero outer must not initialize cache: {text}"
        );
    }
    assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
    let with_inner = "MATCH (s:Source),(i:Item) WHERE i.k=s.k AND 1/0=0 RETURN i.seq";
    let _scope = ReadScope::new(&db);
    let error = consume(&db, with_inner, ExecutionOptions::default()).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Type);
}

#[test]
fn early_close_and_interrupt_release_bound_state_without_changing_graph() {
    let db = fixture();
    let head = storage::branch_head(&db, "main").unwrap();
    let text = "PROFILE MATCH (i:Item),(s:Source) WHERE i.k=s.k RETURN s.seq,i.seq";
    for interrupt in [false, true] {
        let scope = ReadScope::new(&db);
        let mut options = ExecutionOptions::default();
        options.memory_limit_bytes = Some(4 * 1024 * 1024);
        let prepared = prepare(&db, text, BTreeMap::new(), options).unwrap();
        assert!(prepared.physical.explain().contains("IndexNestedLoop"));
        let resources = prepared.resources.clone();
        let mut cursor = QueryCursor::new(prepared);
        let batch = cursor.next_batch(&db, 1).unwrap();
        assert_eq!(batch.rows.len(), 1);
        assert!(!batch.done);
        drop(batch);
        if interrupt {
            let error = cursor
                .next_batch_with_interrupt(&db, 1, &|| true)
                .unwrap_err();
            assert_eq!(error.kind, QueryErrorKind::Interrupted);
        } else {
            cursor.cancel(&db).unwrap();
        }
        drop(cursor);
        assert_eq!(resources.used(), 0);
        assert!(resources.peak() <= 4 * 1024 * 1024);
        drop(scope);
        assert!(db.is_autocommit());
        assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
        assert_eq!(query(&db, NODE_JOIN), expected_nodes());
    }
}

#[test]
fn missing_index_and_host_query_only_keep_truthful_fallbacks() {
    let db = fixture();
    assert_eq!(query(&db, NODE_JOIN), expected_nodes());
    db.pragma_update(None, "query_only", true).unwrap();
    let catalog = temp_catalog(&db);
    assert!(!explain(&db, NODE_JOIN, ExecutionOptions::default()).contains("IndexNestedLoop"));
    assert_eq!(query(&db, NODE_JOIN), expected_nodes());
    assert_eq!(temp_catalog(&db), catalog);
    db.pragma_update(None, "query_only", false).unwrap();
    write(&db, "DROP INDEX item_key");
    assert!(!explain(&db, NODE_JOIN, ExecutionOptions::default()).contains("IndexNestedLoop"));
    assert_eq!(query(&db, NODE_JOIN), expected_nodes());
    assert_eq!(query(&db, NODE_JOIN), query(&db, NODE_ORACLE));
}

#[cfg(feature = "test-support")]
#[test]
fn program_outer_rows_do_not_repeat_snapshot_resolution_or_index_build() {
    let db = fixture();
    lithograph_core::performance::set_enabled(true);
    let measured = |repeats: i64| {
        lithograph_core::performance::reset();
        let text = format!(
            "UNWIND range(1,{repeats}) AS repetition \
            MATCH (i:Item),(s:Source) WHERE i.k=s.k RETURN repetition,s.seq,i.seq"
        );
        let result = read(&db, &format!("PROFILE {text}"), ExecutionOptions::default());
        (result, lithograph_core::performance::snapshot())
    };
    let (one, first) = measured(1);
    let (many, repeated) = measured(12);
    lithograph_core::performance::set_enabled(false);
    assert_eq!(one.rows.len(), 24);
    assert_eq!(many.rows.len(), 288);
    assert_index_loop(&one);
    assert_index_loop(&many);
    assert_eq!(first.resolved_state_builds, 1, "{first:?}");
    assert_eq!(repeated.resolved_state_builds, 1, "{repeated:?}");
    assert_eq!(repeated.lineage_layers_loaded, first.lineage_layers_loaded);
    assert_eq!(
        first.standard_index_builds, 0,
        "ready generation: {first:?}"
    );
    assert_eq!(repeated.standard_index_builds, 0, "{repeated:?}");
}
