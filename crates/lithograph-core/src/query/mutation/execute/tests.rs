use super::*;
use std::cell::Cell;

fn connection() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").unwrap();
    storage::create_storage_schema(&connection).unwrap();
    storage::initialize_root(&connection).unwrap();
    connection
}

#[test]
fn resolved_dynamic_property_key_keeps_its_lease_while_rhs_is_evaluated() {
    let connection = connection();
    let root = storage::branch_head(&connection, "main").unwrap();
    let snapshot = Snapshot::resolve(&connection, root).unwrap();
    let resources = QueryResources::new(Some(64 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let key = WritePropertyKey::Dynamic(Expr::Parameter("key".into()));
    let params = BTreeMap::from([("key".into(), Value::String("x".repeat(16 * 1024)))]);
    let (resolved, memory) = resolve_property_key(
        &key,
        &snapshot,
        &BindingRow::default(),
        &params,
        &Default::default(),
        &control,
    )
    .expect("dynamic property key");
    assert_eq!(resolved.len(), 16 * 1024);
    assert!(memory.bytes() >= resolved.len() as u64);
    assert_eq!(resources.used(), memory.bytes());
    let error = resources
        .reserve(resources.limit().unwrap() - memory.bytes() + 1)
        .expect_err("RHS workspace cannot reuse the still-live key's memory");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    drop(resolved);
    drop(memory);
    assert_eq!(resources.used(), 0);
}

#[test]
fn static_property_key_copy_checks_its_budget_before_allocation() {
    let connection = connection();
    let root = storage::branch_head(&connection, "main").unwrap();
    let snapshot = Snapshot::resolve(&connection, root).unwrap();
    let resources = QueryResources::new(Some(1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let error = resolve_property_key(
        &WritePropertyKey::Static("x".repeat(2048)),
        &snapshot,
        &BindingRow::default(),
        &BTreeMap::new(),
        &Default::default(),
        &control,
    )
    .expect_err("static property name is an execution-owned copy");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(resources.peak(), 0);
    assert_eq!(resources.used(), 0);
}

#[test]
fn controlled_property_list_conversion_and_validation_are_cancellable() {
    for cancel_at in [8, 90] {
        let resources = QueryResources::default();
        let checks = Cell::new(0);
        let interrupted = || {
            checks.set(checks.get() + 1);
            checks.get() >= cancel_at
        };
        let control = ExecutionControl::new(&resources, &interrupted);
        let value = Value::List((0..10_000).map(Value::Integer).collect());
        let error = property_from_value_controlled(value, &control)
            .expect_err("cancel in property List conversion or its final validation");
        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert_eq!(checks.get(), cancel_at);
        assert_eq!(resources.used(), 0);
    }
}

#[test]
fn controlled_property_conversion_preserves_its_callers_input_and_output_workspace() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let original = Value::List((0..1000).map(Value::Integer).collect());
    let bytes = crate::query::resources::value_bytes(&original).unwrap();
    let input_memory = control.reserve(bytes).unwrap();
    let workspace = control.reserve(bytes * 2).unwrap();
    let owned = expression::clone_value_unreserved_controlled(&original, Some(&control)).unwrap();
    let property = property_from_value_controlled(owned, &control)
        .unwrap()
        .unwrap();
    assert_eq!(resources.used(), bytes * 3);
    assert!(matches!(&property, PropertyValue::List(values) if values.len() == 1000));
    drop(property);
    drop(workspace);
    assert_eq!(resources.used(), input_memory.bytes());
    drop(original);
    drop(input_memory);
    assert_eq!(resources.used(), 0);
}

#[test]
fn controlled_property_vector_packing_checks_cancellation_and_preserves_bytes() {
    let resources = QueryResources::default();
    let checks = Cell::new(0);
    let interrupted = || {
        checks.set(checks.get() + 1);
        checks.get() >= 8
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    let vector = crate::cypher::VectorValue::new(
        crate::cypher::VectorCoordinateType::I64,
        VectorValues::I64(vec![42; 4096]),
    )
    .unwrap();
    let error = property_from_value_controlled(Value::Vector(vector), &control)
        .expect_err("cancel during Vector packing");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(checks.get(), 8);
    let control = ExecutionControl::new(&resources, &|| false);
    let vector = crate::cypher::VectorValue::new(
        crate::cypher::VectorCoordinateType::I16,
        VectorValues::I16(vec![1, -2, 256]),
    )
    .unwrap();
    let property = property_from_value_controlled(Value::Vector(vector), &control)
        .unwrap()
        .unwrap();
    let PropertyValue::Vector(vector) = property else {
        panic!("typed vector property")
    };
    assert_eq!(vector.dimension, 3);
    assert_eq!(vector.coordinate_type, StorageVectorCoordinateType::I16);
    assert_eq!(vector.packed, vec![1, 0, 254, 255, 0, 1]);
    assert_eq!(resources.used(), 0);
}
