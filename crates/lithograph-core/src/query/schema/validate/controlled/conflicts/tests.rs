use super::*;
use crate::query::QueryResources;
use std::collections::BTreeSet;

fn schema() -> SchemaState {
    let required = PropertyRule {
        property_type: storage::PropertyType::Integer,
        required: true,
    };
    let mut schema = SchemaState::default();
    schema.graph_nodes.insert(
        "Thing".to_owned(),
        GraphNodeType {
            label: "Thing".to_owned(),
            implied_labels: BTreeSet::new(),
            properties: BTreeMap::from([("missing".to_owned(), required)]),
        },
    );
    for (name, property, kind) in [
        ("requires_q", "q", ConstraintDefinitionKind::NotNull),
        ("unique_p", "p", ConstraintDefinitionKind::Unique),
    ] {
        schema.constraints.insert(
            name.to_owned(),
            ConstraintDefinition {
                name: name.to_owned(),
                target: SchemaTarget::Node {
                    label: "Thing".to_owned(),
                },
                properties: vec![property.to_owned()],
                kind,
                origin: None,
            },
        );
    }
    schema
}

#[test]
fn collector_preserves_all_conflicts_order_categories_and_retained_memory() {
    let connection = super::super::tests::fixture("CREATE (:Thing {p:1}), (:Thing {p:1})");
    let resources = QueryResources::new(Some(16 * 1024 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let commit = storage::branch_head(&connection, "main").unwrap();
    let snapshot =
        crate::query::graph::resolve_snapshot_with_control(&connection, commit, &control).unwrap();
    let retained = resources.used();
    let (conflicts, memory) =
        validation_conflicts_controlled(&connection, &schema(), &snapshot, &control).unwrap();
    assert_eq!(
        conflicts
            .iter()
            .map(|(slot, _)| slot.as_str())
            .collect::<Vec<_>>(),
        [
            "graph/node/Thing",
            "constraint/requires_q",
            "constraint/unique_p"
        ]
    );
    assert!(
        conflicts
            .iter()
            .all(|(_, error)| error.kind == QueryErrorKind::Constraint)
    );
    assert_eq!(resources.used(), retained + memory.bytes());
    let scratch: i64 = connection
        .query_row(
            "SELECT count(*) FROM temp._lithograph_constraint_validation_seen",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(scratch, 0);
    drop(conflicts);
    drop(memory);
    assert_eq!(resources.used(), retained);
    drop(snapshot);
    assert_eq!(resources.used(), 0);
}

#[test]
fn collector_budget_failure_releases_errors_and_work_buffers() {
    let connection = super::super::tests::fixture("CREATE (:Thing {p:1}), (:Thing {p:1})");
    let limit = 512 * 1024;
    let resources = QueryResources::new(Some(limit));
    let control = ExecutionControl::new(&resources, &|| false);
    let commit = storage::branch_head(&connection, "main").unwrap();
    let snapshot =
        crate::query::graph::resolve_snapshot_with_control(&connection, commit, &control).unwrap();
    let retained = resources.used();
    let hold = resources.reserve(limit - retained - 4096).unwrap();
    let error =
        validation_conflicts_controlled(&connection, &schema(), &snapshot, &control).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    drop(hold);
    assert_eq!(resources.used(), retained);
    drop(snapshot);
    assert_eq!(resources.used(), 0);
}
