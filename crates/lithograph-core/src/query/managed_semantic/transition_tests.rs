use std::cell::Cell;

use super::*;

#[test]
fn schema_namespace_checks_cancel_even_when_indices_use_other_families() {
    let connection = Connection::open_in_memory().unwrap();
    let previous = SchemaState::default();
    let mut next = SchemaState::default();
    for i in 0..512 {
        let name = format!("range_{i:04}");
        next.indexes.insert(
            name.clone(),
            IndexDefinition {
                name,
                kind: StandardIndexKind::Range,
                target: IndexTarget::NodeProperties {
                    label: "Person".to_owned(),
                    properties: vec!["id".to_owned()],
                },
                owning_constraint: None,
                labels_or_types: Vec::new(),
                additional_properties: Vec::new(),
                configuration: None,
            },
        );
    }
    let checks = Cell::new(0);
    let interrupt = || {
        checks.set(checks.get() + 1);
        checks.get() >= 3
    };
    let error = validate_transition_controlled(&connection, &previous, &next, &interrupt)
        .expect_err("cancel while visiting non-semantic schema entries");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(checks.get(), 3);
}
