use super::super::persistent::build_persistent_generation;
use std::cell::Cell;
use std::collections::BTreeMap;

use rusqlite::Connection;

use super::*;
use crate::performance;
use crate::query::{ExecutionOptions, QueryCursor, prepare};
use crate::storage::{
    LayerBuilder, OwnerKind, PropertyValue, SchemaState, branch_head, create_storage_schema,
    initialize_root,
};

fn current_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory database");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
                     id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, \
                     database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);\
                 INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format) \
                 VALUES(1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000000011', 3);",
        )
        .expect("metadata");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn execute(connection: &Connection, query: &str) {
    let prepared = prepare(
        connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare query");
    let mut cursor = QueryCursor::new(prepared);
    loop {
        let batch = cursor.next_batch(connection, 17).expect("execute batch");
        if batch.done {
            cursor.complete(connection).expect("complete query");
            return;
        }
    }
}

#[test]
fn staged_snapshot_reuses_persistent_generation_with_query_local_delta() {
    let connection = current_storage();
    execute(
        &connection,
        "CREATE (:Metric {value:1}), (:Metric {value:2}) FINISH",
    );
    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_type FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
    );
    execute(
        &connection,
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
    );
    let commit = branch_head(&connection, "main").expect("index commit");
    let base = Snapshot::resolve(&connection, commit).expect("base snapshot");
    let label = storage::find_label(&connection, "Metric")
        .expect("label lookup")
        .expect("Metric label");
    let key = storage::find_property_key(&connection, "value")
        .expect("property lookup")
        .expect("value property");
    let node = base
        .scan_label_after(label, 0, 16)
        .expect("Metric scan")
        .items
        .into_iter()
        .find(|node| {
            base.property(OwnerKind::Node, *node, key)
                .expect("property")
                == Some(PropertyValue::Integer(1))
        })
        .expect("value=1 owner");
    let mut layer = LayerBuilder::default();
    layer
        .set_property(OwnerKind::Node, node, key, PropertyValue::Integer(99))
        .expect("staged property");
    let schema = SchemaState::load(&connection, commit).expect("schema");
    let staged = Snapshot::resolve_with_layer_and_schema(&connection, commit, &layer, schema)
        .expect("staged snapshot");
    let seek = |value| StandardIndexSeek {
        index_name: "metric_value".to_owned(),
        kind: StandardIndexKind::Range,
        predicates: vec![(0, StandardIndexPredicate::Equal(Value::Integer(value)))],
    };

    performance::reset();
    performance::set_enabled(true);
    let old = scan_node_index_page(&staged, &seek(1), None, 16).expect("old value seek");
    let new = scan_node_index_page(&staged, &seek(99), None, 16).expect("new value seek");
    let counters = performance::snapshot();
    performance::set_enabled(false);
    assert!(!old.items.contains(&node));
    assert_eq!(new.items, vec![node]);
    assert_eq!(counters.standard_index_builds, 0);
}

#[test]
fn bounded_numeric_range_uses_composite_cursor_across_changed_owner_overlay() {
    let connection = current_storage();
    execute(
        &connection,
        "UNWIND range(1,129) AS value CREATE (:Metric {value:value}) FINISH",
    );
    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_type FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
    );
    execute(
        &connection,
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
    );
    let commit = branch_head(&connection, "main").expect("index commit");
    let base = Snapshot::resolve(&connection, commit).expect("base snapshot");
    let label = storage::find_label(&connection, "Metric")
        .expect("label lookup")
        .expect("Metric label");
    let key = storage::find_property_key(&connection, "value")
        .expect("property lookup")
        .expect("value property");
    let nodes = base
        .scan_label_after(label, 0, 256)
        .expect("Metric scan")
        .items;
    let node_for = |wanted| {
        nodes
            .iter()
            .copied()
            .find(|node| {
                base.property(OwnerKind::Node, *node, key).expect("value")
                    == Some(PropertyValue::Integer(wanted))
            })
            .expect("Metric value owner")
    };
    let old_50 = node_for(50);
    let old_120 = node_for(120);
    let mut layer = LayerBuilder::default();
    layer
        .set_property(OwnerKind::Node, old_50, key, PropertyValue::Integer(150))
        .expect("remove owner from Range");
    layer
        .set_property(OwnerKind::Node, old_120, key, PropertyValue::Integer(50))
        .expect("add owner to Range");
    let schema = SchemaState::load(&connection, commit).expect("schema");
    let staged = Snapshot::resolve_with_layer_and_schema(&connection, commit, &layer, schema)
        .expect("staged snapshot");
    let seek = StandardIndexSeek {
        index_name: "metric_value".to_owned(),
        kind: StandardIndexKind::Range,
        predicates: vec![(
            0,
            StandardIndexPredicate::Bounds {
                lower: Some((Value::Integer(10), true)),
                upper: Some((Value::Integer(110), false)),
            },
        )],
    };
    performance::reset();
    performance::set_enabled(true);
    let mut cursor = None;
    let mut owners = Vec::new();
    loop {
        let page =
            scan_node_index_page(&staged, &seek, cursor.as_ref(), 17).expect("bounded Range page");
        owners.extend(page.items);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
        assert!(matches!(
            cursor,
            Some(StandardIndexCursor::NumericRange { .. })
        ));
    }
    let counters = performance::snapshot();
    performance::set_enabled(false);
    assert_eq!(owners.len(), 100);
    assert_eq!(owners.iter().copied().collect::<BTreeSet<_>>().len(), 100);
    assert!(!owners.contains(&old_50));
    assert!(owners.contains(&old_120));
    assert_eq!(counters.standard_index_builds, 0);
    assert_eq!(counters.changed_owners, 2);
}

#[test]
fn numeric_range_cursor_rejects_generation_replacement_between_pages() {
    let connection = current_storage();
    execute(
        &connection,
        "UNWIND range(1,129) AS value CREATE (:Metric {value:value}) FINISH",
    );
    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_type FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
    );
    execute(
        &connection,
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
    );
    let commit = branch_head(&connection, "main").expect("index commit");
    let snapshot = Snapshot::resolve(&connection, commit).expect("snapshot");
    let schema = SchemaState::load(&connection, commit).expect("schema");
    let index = schema
        .indexes
        .get("metric_value")
        .expect("metric value index");
    let seek = StandardIndexSeek {
        index_name: "metric_value".to_owned(),
        kind: StandardIndexKind::Range,
        predicates: vec![(
            0,
            StandardIndexPredicate::Bounds {
                lower: Some((Value::Integer(1), true)),
                upper: Some((Value::Integer(130), false)),
            },
        )],
    };
    let first = scan_node_index_page(&snapshot, &seek, None, 17).expect("first Range page");
    let cursor = first.next_cursor.expect("first continuation");
    let StandardIndexCursor::NumericRange {
        generation_id: before,
        ..
    } = cursor
    else {
        panic!("bounded Range did not return numeric continuation");
    };
    let (after, _) = build_persistent_generation(&snapshot, index, &|| false)
        .expect("replace persistent generation");
    assert!(after > before, "generation identity must never be reused");
    let error = scan_node_index_page(
        &snapshot,
        &seek,
        Some(&StandardIndexCursor::NumericRange {
            generation_id: before,
            sort_number: SqlValue::Integer(17),
            owner_id: first.items.last().copied().expect("first page owner"),
        }),
        17,
    )
    .expect_err("stale Range cursor must fail closed");
    assert_eq!(error.kind, crate::query::QueryErrorKind::Internal);
}

#[test]
fn interrupted_persistent_publish_rolls_back_incomplete_generation() {
    let connection = current_storage();
    execute(
        &connection,
        "UNWIND range(1,129) AS value CREATE (:Metric {value:value}) FINISH",
    );
    execute(
        &connection,
        "CREATE CONSTRAINT metric_value_type FOR (n:Metric) REQUIRE n.value IS :: INTEGER",
    );
    execute(
        &connection,
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
    );
    let commit = branch_head(&connection, "main").expect("index commit");
    let old_generation: (i64, i64) = connection
        .query_row(
            "SELECT generation_id, entry_count FROM main._lithograph_index_generations \
                 WHERE anchor_commit = ?1 AND complete = 1",
            [commit.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("old generation");
    let snapshot = Snapshot::resolve(&connection, commit).expect("snapshot");
    let schema = SchemaState::load(&connection, commit).expect("schema");
    let index = schema.indexes.get("metric_value").expect("index");
    connection
        .execute_batch("SAVEPOINT phase11_cancelled_generation")
        .expect("savepoint");
    let calls = Cell::new(0_u32);
    let error = build_persistent_generation(&snapshot, index, &|| {
        let next = calls.get().saturating_add(1);
        calls.set(next);
        next >= 7
    })
    .expect_err("generation publish must be cancellable between pages");
    assert_eq!(error.kind, crate::query::QueryErrorKind::Interrupted);
    connection
        .execute_batch(
            "ROLLBACK TO phase11_cancelled_generation; RELEASE phase11_cancelled_generation",
        )
        .expect("rollback cancelled generation");
    let restored: (i64, i64, i64) = connection
        .query_row(
            "SELECT generation_id, complete, entry_count \
                 FROM main._lithograph_index_generations WHERE anchor_commit = ?1",
            [commit.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("restored generation");
    assert_eq!(restored, (old_generation.0, 1, old_generation.1));
    let payload: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_index_entries WHERE generation_id = ?1",
            [old_generation.0],
            |row| row.get(0),
        )
        .expect("restored payload");
    assert_eq!(payload, old_generation.1);
    let incomplete: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_index_generations WHERE complete = 0",
            [],
            |row| row.get(0),
        )
        .expect("incomplete generations");
    assert_eq!(incomplete, 0);
}
