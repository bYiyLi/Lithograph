use super::*;

#[test]
fn matched_rows_keep_their_memory_lease_after_the_match_returns() {
    use crate::query::{ExecutionOptions, QueryCursor, prepare};
    use crate::storage::{branch_head, create_storage_schema, initialize_root};

    let connection = rusqlite::Connection::open_in_memory().expect("in-memory SQLite");
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").expect("metadata");
    create_storage_schema(&connection).expect("storage");
    initialize_root(&connection).expect("root");
    let mut write = QueryCursor::new(
        prepare(
            &connection,
            "CREATE (:LeaseProbe), (:LeaseProbe), (:LeaseProbe)",
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .expect("prepare nodes"),
    );
    assert!(write.next_batch(&connection, 1).expect("create nodes").done);
    write.complete(&connection).expect("commit nodes");
    drop(write);
    let snapshot = Snapshot::resolve(
        &connection,
        branch_head(&connection, "main").expect("branch head"),
    )
    .expect("snapshot");
    let ast = crate::cypher::parse("MATCH (n:LeaseProbe) RETURN n").expect("parse match");
    let clause = ast
        .root
        .descendants()
        .find(|node| node.kind == AstKind::Clause(crate::cypher::ClauseKind::Match))
        .expect("match clause");
    let resources = crate::query::QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let (rows, memory) = execute_match(
        &snapshot,
        &ResolvedGraphView::default(),
        &BTreeMap::new(),
        clause,
        vec![BindingRow::default()],
        &mut QueryMetrics::default(),
        &control,
    )
    .expect("match rows");
    assert_eq!(rows.len(), 3);
    let expected = rows
        .iter()
        .try_fold(0, |bytes, row| {
            checked_add(bytes, expression::binding_row_bytes(row)?)
        })
        .expect("row bytes");
    assert!(resources.used() >= expected, "live rows retain their lease");
    drop(rows);
    assert_eq!(resources.used(), memory.bytes());
    drop(memory);
    assert_eq!(resources.used(), 0);
}

#[test]
fn simple_static_label_recognizes_single_pattern_label() {
    let ast = crate::cypher::parse("MATCH (:ScaleLowDegree)-[:SCALE_LINK*1..4]->(m) RETURN 1")
        .expect("parse pattern");
    let node = ast
        .root
        .descendants()
        .find(|node| node.kind == AstKind::NodePattern)
        .expect("start Node pattern");
    let pattern = parse_node(node).expect("lower Node pattern");
    assert_eq!(
        simple_static_label(&pattern),
        Some("ScaleLowDegree"),
        "labels={:#?}",
        pattern.labels
    );
}

fn accounted_state(control: &ExecutionControl<'_>, text: &str) -> PathState {
    let mut row = BindingRow::default();
    row.insert(
        "value".to_owned(),
        BindingValue::Scalar(Value::String(text.to_owned())),
    );
    let memory = control
        .reserve(binding_row_bytes(&row, control).unwrap())
        .unwrap();
    PathState {
        row,
        current: Some(1),
        nodes: Vec::new(),
        relationships: Vec::new(),
        memory,
    }
}

#[test]
fn path_state_copy_checks_memory_before_cloning_and_releases_it() {
    let resources = super::super::super::resources::QueryResources::new(Some(8_000));
    let control = ExecutionControl::new(&resources, &|| false);
    let state = accounted_state(&control, &"x".repeat(4_000));
    let used = resources.used();
    let error = state.try_clone(&control).expect_err("copy exceeds limit");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(resources.used(), used, "failed preflight retains no lease");
    drop(state);
    assert_eq!(resources.used(), 0, "state owns and releases its memory");
}

#[test]
fn path_selector_checks_cancellation_while_partitioning() {
    let resources = super::super::super::resources::QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| true);
    // Build using a different control so cancellation occurs in the selector.
    let states = {
        let building = ExecutionControl::new(&resources, &|| false);
        (0..512)
            .map(|_| accounted_state(&building, "value"))
            .collect()
    };
    let error = apply_selector(
        states,
        PathSelector {
            kind: PathSelectorKind::AnyShortest,
            count: 1,
        },
        &control,
    )
    .expect_err("partitioning must observe cancellation");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(resources.used(), 0, "cancelled paths release their leases");
}

#[test]
fn path_state_copy_observes_cancellation_inside_large_scalar() {
    let resources = super::super::super::resources::QueryResources::default();
    let building = ExecutionControl::new(&resources, &|| false);
    let mut state = accounted_state(&building, "value");
    state.row.values.insert(
        "value".to_owned(),
        BindingValue::Scalar(Value::List((0..4096).map(Value::Integer).collect())),
    );
    state
        .memory
        .resize(binding_row_bytes(&state.row, &building).unwrap())
        .unwrap();
    let original_memory = resources.used();
    let checks = std::cell::Cell::new(0);
    let interrupted = || {
        let previous = checks.get();
        checks.set(previous + 1);
        previous >= 1
    };
    let cloning = ExecutionControl::new(&resources, &interrupted);
    let error = state
        .try_clone(&cloning)
        .expect_err("copy is cancelled internally");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert!(checks.get() >= 2, "clone reaches an internal checkpoint");
    assert_eq!(
        resources.used(),
        original_memory,
        "failed clone drops its lease"
    );
    drop(state);
    assert_eq!(resources.used(), 0);
}

#[test]
fn path_sort_keeps_length_then_relationship_identity_order() {
    let resources = super::super::super::resources::QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let states = [(2, 4), (1, 9), (2, 1), (1, 3)]
        .into_iter()
        .map(|(length, first)| {
            let mut state = accounted_state(&control, "value");
            state.relationships = (0..length)
                .map(|offset| RelationshipRecord {
                    id: first + offset,
                    source: 1,
                    target: 2,
                    type_id: 1,
                })
                .collect();
            state
                .memory
                .grow(allocation_bytes::<RelationshipRecord>(length as usize).unwrap())
                .unwrap();
            state
        })
        .collect();
    let sorted = sort_paths(states, &control).expect("sort paths");
    assert_eq!(
        sorted
            .iter()
            .map(|state| (state.relationships.len(), state.relationships[0].id))
            .collect::<Vec<_>>(),
        vec![(1, 3), (1, 9), (2, 1), (2, 4)]
    );
    drop(sorted);
    assert_eq!(resources.used(), 0);
}

#[test]
fn path_sort_stops_during_merge_and_releases_all_states() {
    let resources = super::super::super::resources::QueryResources::default();
    let building = ExecutionControl::new(&resources, &|| false);
    let states = (0..512)
        .map(|_| accounted_state(&building, "value"))
        .collect();
    let checks = std::cell::Cell::new(0);
    let interrupted = || {
        let previous = checks.get();
        checks.set(previous + 1);
        previous >= 1
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = sort_paths(states, &control).expect_err("merge observes cancellation");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(resources.used(), 0);
}

fn property_fixture() -> rusqlite::Connection {
    let connection = rusqlite::Connection::open_in_memory().expect("in-memory SQLite");
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").expect("metadata");
    storage::create_storage_schema(&connection).expect("storage");
    storage::initialize_root(&connection).expect("root");
    connection
}

fn property_snapshot(connection: &rusqlite::Connection) -> Snapshot<'_> {
    let root = storage::branch_head(connection, "main").expect("branch head");
    let key = storage::intern_property_key(connection, "items").expect("property key");
    storage::intern_property_key(connection, "absent").expect("known absent property key");
    let mut snapshot = Snapshot::resolve(connection, root).expect("snapshot");
    let mut layer = storage::LayerBuilder::default();
    layer.add_node(1).expect("add node");
    layer
        .set_property(
            OwnerKind::Node,
            1,
            key,
            storage::PropertyValue::List(
                (0..10_000).map(storage::PropertyValue::Integer).collect(),
            ),
        )
        .expect("large canonical property");
    snapshot
        .apply_layer_checked(&layer, &|| Ok(()))
        .expect("apply layer");
    snapshot
}

fn match_property(
    snapshot: &Snapshot<'_>,
    name: &str,
    expected: Value,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let view = ResolvedGraphView::default();
    let params = BTreeMap::new();
    let mut metrics = QueryMetrics::default();
    let context = PathContext {
        snapshot,
        graph_view: &view,
        params: &params,
        metrics: &mut metrics,
        control,
        match_mode: MatchModeKind::DifferentRelationships,
    };
    properties_match(
        &context,
        &BindingRow::default(),
        &Expr::Literal(Value::Map(BTreeMap::from([(name.to_owned(), expected)]))),
        OwnerKind::Node,
        1,
    )
}

#[test]
fn path_property_match_reserves_actual_value_before_conversion() {
    let connection = property_fixture();
    let snapshot = property_snapshot(&connection);
    let resources = crate::query::QueryResources::new(Some(16 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let error = match_property(&snapshot, "items", Value::Integer(1), &control)
        .expect_err("actual property conversion exceeds the remaining budget");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert!(resources.peak() <= 16 * 1024);
    assert_eq!(resources.used(), 0);
}

#[test]
fn path_property_match_checks_cancellation_inside_actual_value() {
    let connection = property_fixture();
    let snapshot = property_snapshot(&connection);
    let resources = crate::query::QueryResources::default();
    let checks = std::cell::Cell::new(0);
    let interrupted = || {
        checks.set(checks.get() + 1);
        checks.get() >= 64
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = match_property(&snapshot, "items", Value::Integer(1), &control)
        .expect_err("actual property conversion checks cancellation before type comparison");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    assert_eq!(checks.get(), 64);
    assert_eq!(resources.used(), 0);
}

#[test]
fn path_property_match_preserves_numeric_and_missing_null_semantics() {
    let connection = property_fixture();
    let snapshot = property_snapshot(&connection);
    let resources = crate::query::QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let expected = Value::List(
        (0..10_000)
            .map(|value| Value::Float(value as f64))
            .collect(),
    );
    assert!(match_property(&snapshot, "items", expected, &control).expect("numeric list equality"));
    for name in ["items", "absent", "unknown"] {
        assert!(
            !match_property(&snapshot, name, Value::Null, &control).expect("null never matches")
        );
    }
    assert_eq!(resources.used(), 0);
}

#[test]
fn path_new_binding_reserves_its_map_leaf_and_order_capacity_before_insertion() {
    let sizing = crate::query::QueryResources::default();
    let building = ExecutionControl::new(&sizing, &|| false);
    let row = BindingRow::default();
    let base = binding_row_bytes(&row, &building).expect("empty state bytes");
    let binding = checked_add(
        crate::query::resources::map_entry_bytes::<BindingValue>().expect("map leaf bytes"),
        checked_add(
            2,
            allocation_bytes::<String>(4).expect("first order allocation"),
        )
        .unwrap(),
    )
    .unwrap();
    let resources = crate::query::QueryResources::new(Some(base + binding - 1));
    let mut state = PathState {
        row,
        current: None,
        nodes: Vec::new(),
        relationships: Vec::new(),
        memory: resources.reserve(base).expect("initial state"),
    };
    let error = state
        .reserve_binding(Some("n"))
        .expect_err("new leaf does not fit");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert!(state.row.values.is_empty());
    assert_eq!(state.row.order.capacity(), 0);
    assert_eq!(resources.used(), base);
    drop(state);
    assert_eq!(resources.used(), 0);
}
