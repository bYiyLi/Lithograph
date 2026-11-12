use super::*;
use crate::query::{ExecutionOptions, QueryCursor, QueryMetrics, QueryResources, prepare};
use crate::storage::{branch_head, create_storage_schema, initialize_root};
use std::collections::BTreeMap;

#[test]
fn relationship_outputs_transfer_exact_claims_with_live_undirected_sibling() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
    create_storage_schema(&db).unwrap();
    initialize_root(&db).unwrap();
    let seed = prepare(
        &db,
        "CREATE (:Lease)-[:EDGE]->(:Lease)",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap();
    let mut seed = QueryCursor::new(seed);
    assert!(seed.next_batch(&db, 1).unwrap().done);
    seed.complete(&db).unwrap();
    let names = ["a".repeat(4096), "r".repeat(4096), "b".repeat(4096)];
    let text = format!(
        "MATCH ({})-[{}]-({}) RETURN 1",
        names[0], names[1], names[2]
    );
    let prepared = prepare(&db, &text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let mut part = prepared.matches[0].parts[0].clone();
    part.relationship.as_mut().unwrap().id_seek = Some(1);
    let snapshot = Snapshot::resolve(&db, branch_head(&db, "main").unwrap()).unwrap();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut base = BindingRow::default();
    for n in 0..8 {
        base.insert(format!("seed{n}"), BindingValue::Scalar(Value::Integer(n)));
    }
    let mut cursor = PartCursor::new(&part, &base, &control).unwrap();
    let mut metrics = QueryMetrics::default();
    let bindings = MatchReadBindings::default();
    let first = cursor
        .next_row(
            &snapshot,
            &prepared.graph_view,
            &mut metrics,
            &bindings,
            &control,
        )
        .unwrap()
        .unwrap();
    let exact = expression::binding_row_bytes_controlled(&first, Some(&control)).unwrap();
    let legacy = names.iter().fold(
        expression::binding_row_bytes_controlled(&base, Some(&control)).unwrap() + 256,
        |sum, name| sum + map_entry_bytes::<BindingValue>().unwrap() + name.len() as u64,
    );
    assert!(
        exact > legacy,
        "the prior one-name/256 estimate omitted real storage"
    );
    let first_memory = cursor.split_output_memory(&first, &control).unwrap();
    assert_eq!(first_memory.bytes(), exact);
    let second = cursor
        .next_row(
            &snapshot,
            &prepared.graph_view,
            &mut metrics,
            &bindings,
            &control,
        )
        .unwrap()
        .unwrap();
    let second_memory = cursor.split_output_memory(&second, &control).unwrap();
    assert!(resources.used() >= first_memory.bytes() + second_memory.bytes());
    assert_ne!(first.values.get(&names[0]), second.values.get(&names[0]));
    drop((first, second, first_memory, second_memory, cursor));
    assert_eq!(resources.used(), 0);
}
