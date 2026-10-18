use super::*;
use crate::query::resources::QueryResources;

#[test]
fn frozen_function_inventory_fits_metadata_preflight() {
    let mut overloads = BTreeMap::<&str, usize>::new();
    for definition in crate::query::registry::functions() {
        *overloads.entry(definition.name).or_default() += 1;
        let arguments = definition.arguments().expect("frozen arguments");
        assert!(arguments.len() <= MAX_ARGUMENTS, "{}", definition.name);
        for argument in arguments {
            assert!(
                argument.name.len() <= MAX_ARGUMENT_NAME,
                "{} argument {:?} uses {} bytes, preflight allows {}",
                definition.name,
                argument.name,
                argument.name.len(),
                MAX_ARGUMENT_NAME
            );
            assert!(
                argument.value_type.len() <= MAX_ARGUMENT_TYPE,
                "{}",
                definition.name
            );
            assert!(
                argument.description.len() <= MAX_ARGUMENT_DESCRIPTION,
                "{}",
                definition.name
            );
        }
        let signature = definition.signature().expect("frozen signature");
        assert!(
            signature.len() as u64 <= signature_bound(definition).unwrap(),
            "{}",
            definition.name
        );
        let row = function_registry_row(definition).unwrap();
        assert!(
            expression::binding_row_bytes(&row).unwrap()
                <= function_metadata_bytes(definition).unwrap(),
            "{}",
            definition.name
        );
    }
    assert!(overloads.values().all(|count| *count <= MAX_OVERLOADS));
}

#[test]
fn frozen_procedure_inventory_fits_metadata_preflight() {
    for definition in crate::query::registry::procedures() {
        let row = procedure_registry_row(definition);
        assert!(
            expression::binding_row_bytes(&row).unwrap()
                <= procedure_metadata_bytes(definition).unwrap(),
            "{}",
            definition.name
        );
    }
}

#[test]
fn registry_construction_rejects_tiny_budget_without_retaining_memory() {
    let resources = QueryResources::new(Some(1));
    let control = ExecutionControl::new(&resources, &|| false);
    assert!(function_registry_rows(&control).is_err());
    assert_eq!(resources.used(), 0);
    assert_eq!(
        resources.peak(),
        0,
        "enumeration preflight fails before construction"
    );
    assert!(procedure_registry_rows(&control).is_err());
    assert_eq!(resources.used(), 0);
    assert_eq!(resources.peak(), 0);
}

#[test]
fn registry_rows_keep_memory_until_their_returned_lease_is_released() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    for make_rows in [function_registry_rows, procedure_registry_rows] {
        let (rows, memory) = make_rows(&control).expect("build frozen registry");
        assert!(!rows.is_empty());
        let retained = rows
            .iter()
            .map(|row| expression::binding_row_bytes(row).unwrap())
            .sum::<u64>();
        assert!(memory.bytes() >= retained);
        assert_eq!(resources.used(), memory.bytes());
        drop(rows);
        assert!(resources.used() > 0);
        drop(memory);
        assert_eq!(resources.used(), 0);
    }
}
