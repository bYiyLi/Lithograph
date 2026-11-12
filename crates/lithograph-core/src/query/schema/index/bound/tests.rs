use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;

use super::*;
use crate::query::{ExecutionOptions, QueryCursor, QueryErrorKind, QueryResources, prepare};
use crate::storage::{
    LayerBuilder, PropertyValue, SchemaState, branch_head, create_storage_schema, initialize_root,
};

fn current_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory database");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
         id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, \
         database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);\
         INSERT INTO main._lithograph_meta VALUES(1, 'lithograph-format-v1', \
         '00000000-0000-4000-8000-000000000022', 3);",
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
    .expect("prepare fixture");
    let mut cursor = QueryCursor::new(prepared);
    loop {
        if cursor
            .next_batch(connection, 17)
            .expect("execute fixture")
            .done
        {
            cursor.complete(connection).expect("complete fixture");
            return;
        }
    }
}

fn node_fixture(values: &str) -> Connection {
    let connection = current_storage();
    execute(
        &connection,
        &format!("UNWIND {values} AS value CREATE (:Metric {{value:value}}) FINISH"),
    );
    execute(
        &connection,
        "CREATE RANGE INDEX metric_value FOR (n:Metric) ON (n.value)",
    );
    connection
}

fn active_snapshot(connection: &Connection) -> Snapshot<'_> {
    Snapshot::resolve(connection, branch_head(connection, "main").expect("head")).expect("snapshot")
}

fn pinned_schema_snapshot(connection: &Connection) -> Snapshot<'_> {
    let base = active_snapshot(connection);
    let schema = base.schema_state().unwrap();
    Snapshot::resolve_with_layer_and_schema(
        connection,
        base.commit(),
        &LayerBuilder::default(),
        schema,
    )
    .unwrap()
}

#[test]
fn dynamic_filter_binding_rejects_long_scope_before_any_optional_allocation_or_sql() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    let connection = current_storage();
    let label = "L".repeat(4096);
    execute(&connection, &format!("CREATE (n:{label} {{k:7}}) FINISH"));
    execute(
        &connection,
        &format!("CREATE RANGE INDEX short FOR(n:{label}) ON(n.k)"),
    );
    let snapshot = pinned_schema_snapshot(&connection);
    let index = &snapshot.query_schema_state().unwrap().indexes["short"];
    let (_, hash) = index_definition_identity(index).unwrap();
    let ready: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_index_generations \
         WHERE definition_hash=?1 AND anchor_commit=?2 AND complete=1",
            params![
                hash.as_bytes().as_slice(),
                snapshot.commit().as_bytes().as_slice()
            ],
            |row| row.get(0),
        )
        .unwrap();
    assert!(ready > 0, "the real current generation exists");
    let resources = QueryResources::new(Some(90 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    assert!(64 * 1024 + index.name.len() as u64 * 64 < 90 * 1024);
    assert!(definition_workspace(index, &control).unwrap() > 90 * 1024);
    connection
        .authorizer(Some(|context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Read {
                    table_name: "_lithograph_index_generations",
                    ..
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
    assert!(
        try_bind_dynamic_filter_index(&snapshot, "short", hash, &control)
            .unwrap()
            .is_none()
    );
    assert_eq!(resources.used(), 0);
    assert_eq!(
        resources.peak(),
        0,
        "preflight rejects before cloning or reserving the definition"
    );
    connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
}

#[test]
fn dynamic_filter_binding_missing_ready_source_is_fallback_but_main_auth_remains_error() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    let connection = node_fixture("[7,8,9]");
    let snapshot = pinned_schema_snapshot(&connection);
    let index = &snapshot.query_schema_state().unwrap().indexes["metric_value"];
    let (_, hash) = index_definition_identity(index).unwrap();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let bound = try_bind_dynamic_filter_index(&snapshot, "metric_value", hash, &control)
        .unwrap()
        .unwrap();
    assert_eq!(
        owners(&snapshot, &bound, &Value::Integer(7), &control).len(),
        1
    );
    drop(bound);
    assert_eq!(resources.used(), 0);
    connection
        .authorizer(Some(|context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Read {
                    table_name: "_lithograph_index_generations",
                    ..
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
    let error =
        try_bind_dynamic_filter_index(&snapshot, "metric_value", hash, &control).unwrap_err();
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_AUTH));
    connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    // Derived-state eviction is synthetic and disposable. The optional path
    // must not use generic binding to rebuild the complete Probe domain.
    connection
        .execute_batch(
            "DELETE FROM main._lithograph_index_entries; \
        DELETE FROM main._lithograph_index_generations; \
        DELETE FROM temp._lithograph_standard_index_cache_meta",
        )
        .unwrap();
    assert!(
        try_bind_dynamic_filter_index(&snapshot, "metric_value", hash, &control)
            .unwrap()
            .is_none()
    );
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_index_generations",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "no hidden generic rebuild");
    assert_eq!(resources.used(), 0);
}

fn bind(
    snapshot: &Snapshot<'_>,
    name: &str,
    control: &ExecutionControl<'_>,
) -> BoundStandardIndexRead {
    let schema = snapshot.schema_state().expect("schema");
    let (_, hash) = index_definition_identity(&schema.indexes[name]).expect("definition hash");
    bind_standard_index_read(snapshot, name, hash, control).expect("bind Index")
}

fn owners(
    snapshot: &Snapshot<'_>,
    bound: &BoundStandardIndexRead,
    value: &Value,
    control: &ExecutionControl<'_>,
) -> Vec<i64> {
    let mut result = Vec::new();
    let mut cursor = None;
    loop {
        let page = scan_bound_index_page(snapshot, bound, value, cursor.as_ref(), 2, control)
            .expect("equality page");
        assert!(page.items.len() <= 2);
        result.extend(page.items);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return result;
        }
    }
}

#[test]
fn scalar_keys_keep_numeric_equality_null_nan_and_repeated_probe_bags() {
    let connection = node_fixture("[0, -0.0, 1, 1.0, 1.5, '1', true, null]");
    let snapshot = active_snapshot(&connection);
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let bound = bind(&snapshot, "metric_value", &control);
    assert_eq!(bound.owner_kind(), OwnerKind::Node);
    let cases = [
        (Value::Integer(0), 2),
        (Value::Float(-0.0), 2),
        (Value::Integer(1), 2),
        (Value::Float(1.0), 2),
        (Value::Float(1.5), 1),
        (Value::String("1".to_owned()), 1),
        (Value::Boolean(true), 1),
        (Value::Boolean(false), 0),
        (Value::Null, 0),
        (Value::Float(f64::NAN), 0),
    ];
    for (value, expected) in cases {
        let first = owners(&snapshot, &bound, &value, &control);
        assert_eq!(first.len(), expected, "{value:?}");
        assert!(first.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(owners(&snapshot, &bound, &value, &control), first);
    }
}

#[test]
fn relationship_and_text_definitions_bind_the_actual_owner_and_key_encoding() {
    let connection = current_storage();
    execute(
        &connection,
        "UNWIND ['same','same','other'] AS value \
         CREATE (:Metric)-[:LINK {value:value}]->(:Metric) FINISH",
    );
    execute(
        &connection,
        "CREATE TEXT INDEX link_value FOR ()-[r:LINK]-() ON (r.value)",
    );
    let snapshot = active_snapshot(&connection);
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let bound = bind(&snapshot, "link_value", &control);
    assert_eq!(bound.owner_kind(), OwnerKind::Relationship);
    let matched = owners(
        &snapshot,
        &bound,
        &Value::String("same".to_owned()),
        &control,
    );
    assert_eq!(matched.len(), 2);
    for relationship in matched {
        assert!(
            snapshot
                .relationship(relationship)
                .expect("relationship")
                .is_some()
        );
    }
    assert!(owners(&snapshot, &bound, &Value::Integer(1), &control).is_empty());
}

fn page_vm_work(size: usize) -> (u64, Vec<i64>) {
    let connection = node_fixture(&format!("[value IN range(1,{size}) | 7]"));
    let snapshot = active_snapshot(&connection);
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let bound = bind(&snapshot, "metric_value", &control);
    assert!(matches!(bound.source, BoundSource::Persistent { .. }));
    let work = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&work);
    connection
        .progress_handler(
            1,
            Some(move || {
                observed.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .expect("install VM counter");
    let page = scan_bound_index_page(&snapshot, &bound, &Value::Integer(7), None, 7, &control)
        .expect("bounded high-fanout equality page");
    connection
        .progress_handler(0, None::<fn() -> bool>)
        .expect("remove VM counter");
    assert!(page.next_cursor.is_some());
    (work.load(Ordering::Relaxed), page.items)
}

#[test]
fn equality_limit_bounds_real_sqlite_vm_work_at_high_fanout() {
    let (small_work, small) = page_vm_work(256);
    let (large_work, large) = page_vm_work(4096);
    assert_eq!(small.len(), 7);
    assert_eq!(large.len(), 7);
    assert!(small.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(large.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(
        large_work <= small_work.saturating_mul(2),
        "VM {small_work} -> {large_work}"
    );
    assert!(
        large_work < 2_000,
        "first page must not scan all matching owners: {large_work}"
    );
}

#[test]
fn staged_overlay_removes_old_keys_and_merges_new_keys_in_owner_order() {
    let connection = node_fixture("[1, 2, 3, 4]");
    let base = active_snapshot(&connection);
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let base_bound = bind(&base, "metric_value", &control);
    let first = owners(&base, &base_bound, &Value::Integer(1), &control)[0];
    let second = owners(&base, &base_bound, &Value::Integer(2), &control)[0];
    let key = storage::find_property_key(&connection, "value")
        .unwrap()
        .unwrap();
    let mut layer = LayerBuilder::default();
    layer
        .set_property(OwnerKind::Node, first, key, PropertyValue::Integer(9))
        .unwrap();
    layer
        .set_property(OwnerKind::Node, second, key, PropertyValue::Integer(1))
        .unwrap();
    let schema = SchemaState::load(&connection, base.commit()).unwrap();
    let staged =
        Snapshot::resolve_with_layer_and_schema(&connection, base.commit(), &layer, schema)
            .expect("staged snapshot");
    let bound = bind(&staged, "metric_value", &control);
    assert!(matches!(bound.source, BoundSource::Persistent { .. }));
    assert_eq!(
        owners(&staged, &bound, &Value::Integer(1), &control),
        vec![second]
    );
    assert_eq!(
        owners(&staged, &bound, &Value::Integer(9), &control),
        vec![first]
    );
    assert!(
        scan_bound_index_page(&staged, &base_bound, &Value::Integer(1), None, 2, &control).is_err()
    );
    assert_eq!(
        owners(&base, &base_bound, &Value::Integer(1), &control),
        vec![first]
    );
}

fn assert_source_changes_fail(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    bound: &BoundStandardIndexRead,
    mutations: &[&str],
    control: &ExecutionControl<'_>,
) {
    let first = scan_bound_index_page(snapshot, bound, &Value::Integer(7), None, 1, control)
        .expect("first page");
    assert_eq!(first.items.len(), 1);
    for mutation in mutations {
        connection
            .execute_batch("SAVEPOINT change_bound_source")
            .unwrap();
        connection
            .execute_batch(mutation)
            .expect("mutate disposable derived source");
        let error = scan_bound_index_page(
            snapshot,
            bound,
            &Value::Integer(7),
            first.next_cursor.as_ref(),
            1,
            control,
        )
        .expect_err("invalid continuation must fail");
        assert_eq!(error.kind, QueryErrorKind::Internal, "{mutation}: {error}");
        connection
            .execute_batch("ROLLBACK TO change_bound_source; RELEASE change_bound_source")
            .unwrap();
        assert_eq!(
            owners(snapshot, bound, &Value::Integer(7), control).len(),
            3
        );
    }
}

#[test]
fn persistent_manifest_and_temp_binding_headers_are_checked_on_every_page() {
    let connection = node_fixture("[7,7,7]");
    let snapshot = active_snapshot(&connection);
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let bound = bind(&snapshot, "metric_value", &control);
    assert!(matches!(bound.source, BoundSource::Persistent { .. }));
    assert_source_changes_fail(
        &connection,
        &snapshot,
        &bound,
        &[
            "UPDATE main._lithograph_index_generations SET complete=0",
            "UPDATE main._lithograph_index_generations SET encoding_version=encoding_version+1",
            "UPDATE main._lithograph_index_generations SET definition_blob='broken'",
            "UPDATE main._lithograph_index_generations SET definition_hash=zeroblob(32)",
            "UPDATE main._lithograph_index_generations SET anchor_commit=zeroblob(32)",
            "UPDATE temp._lithograph_standard_index_cache_meta SET complete=0",
            "UPDATE temp._lithograph_standard_index_cache_meta SET generation_anchor=zeroblob(32)",
            "UPDATE temp._lithograph_standard_index_cache_meta SET generation_id=generation_id+100",
            "UPDATE temp._lithograph_standard_index_cache_meta SET generation_id=NULL,generation_anchor=NULL",
            "DELETE FROM temp._lithograph_standard_index_cache_meta",
            "DELETE FROM main._lithograph_index_entries; DELETE FROM main._lithograph_index_generations",
        ],
        &control,
    );
}

#[test]
fn missing_persistent_generation_can_rebind_only_as_a_new_read() {
    let connection = node_fixture("[7,7,7]");
    let snapshot = active_snapshot(&connection);
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let bound = bind(&snapshot, "metric_value", &control);
    let first =
        scan_bound_index_page(&snapshot, &bound, &Value::Integer(7), None, 1, &control).unwrap();
    connection.execute_batch(
        "DELETE FROM main._lithograph_index_entries; DELETE FROM main._lithograph_index_generations"
    ).unwrap();
    assert!(
        scan_bound_index_page(
            &snapshot,
            &bound,
            &Value::Integer(7),
            first.next_cursor.as_ref(),
            1,
            &control
        )
        .is_err()
    );
    let rebound = bind(&snapshot, "metric_value", &control);
    assert_eq!(rebound.source, BoundSource::Local);
    assert_eq!(
        owners(&snapshot, &rebound, &Value::Integer(7), &control).len(),
        3
    );
    assert!(
        scan_bound_index_page(
            &snapshot,
            &bound,
            &Value::Integer(7),
            first.next_cursor.as_ref(),
            1,
            &control
        )
        .is_err()
    );
}

#[test]
fn local_cache_binding_requires_the_same_complete_null_generation_source() {
    let connection = node_fixture("[7,7,7]");
    connection.execute_batch(
        "DELETE FROM main._lithograph_index_entries; DELETE FROM main._lithograph_index_generations"
    ).unwrap();
    let snapshot = active_snapshot(&connection);
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let bound = bind(&snapshot, "metric_value", &control);
    assert_eq!(bound.source, BoundSource::Local);
    assert_source_changes_fail(
        &connection,
        &snapshot,
        &bound,
        &[
            "UPDATE temp._lithograph_standard_index_cache_meta SET complete=0",
            "UPDATE temp._lithograph_standard_index_cache_meta SET snapshot_hash=zeroblob(32)",
            "UPDATE temp._lithograph_standard_index_cache_meta SET index_name='different'",
            "UPDATE temp._lithograph_standard_index_cache_meta SET generation_id=999,generation_anchor=zeroblob(32)",
            "DELETE FROM temp._lithograph_standard_index_cache_meta",
        ],
        &control,
    );
}

#[test]
fn bind_rejects_definition_drift_and_unsupported_capabilities() {
    let connection = node_fixture("[1]");
    let snapshot = active_snapshot(&connection);
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    assert!(
        bind_standard_index_read(
            &snapshot,
            "metric_value",
            HashId::from_bytes([0; 32]),
            &control
        )
        .is_err()
    );
    assert!(
        bind_standard_index_read(&snapshot, "missing", HashId::from_bytes([0; 32]), &control)
            .is_err()
    );
    let mut definition = snapshot
        .schema_state()
        .unwrap()
        .indexes
        .remove("metric_value")
        .unwrap();
    for kind in [
        StandardIndexKind::Lookup,
        StandardIndexKind::Point,
        StandardIndexKind::FullText,
        StandardIndexKind::Vector,
        StandardIndexKind::Semantic,
    ] {
        definition.kind = kind;
        assert!(equality_owner_kind(&definition).is_err());
    }
    definition.kind = StandardIndexKind::Range;
    if let IndexTarget::NodeProperties { properties, .. } = &mut definition.target {
        properties.push("other".to_owned());
    }
    assert!(equality_owner_kind(&definition).is_err());
    assert_eq!(resources.used(), 0);
}

#[test]
fn zero_limit_foreign_cursor_and_unproved_key_family_are_errors() {
    let connection = node_fixture("[1]");
    let snapshot = active_snapshot(&connection);
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let bound = bind(&snapshot, "metric_value", &control);
    assert!(
        scan_bound_index_page(&snapshot, &bound, &Value::Integer(1), None, 0, &control).is_err()
    );
    let cursor = StandardIndexCursor::NumericRange {
        generation_id: 1,
        sort_number: rusqlite::types::Value::Integer(1),
        owner_id: 1,
    };
    assert!(
        scan_bound_index_page(
            &snapshot,
            &bound,
            &Value::Integer(1),
            Some(&cursor),
            1,
            &control
        )
        .is_err()
    );
    assert!(
        scan_bound_index_page(&snapshot, &bound, &Value::List(vec![]), None, 1, &control).is_err()
    );
}

#[test]
fn binding_and_key_workspace_obey_quota_and_cancellation_before_copying() {
    let connection = node_fixture("[1]");
    let snapshot = active_snapshot(&connection);
    let schema = snapshot.schema_state().unwrap();
    let (_, hash) = index_definition_identity(&schema.indexes["metric_value"]).unwrap();
    let tiny = QueryResources::new(Some(1));
    let tiny_control = ExecutionControl::new(&tiny, &|| false);
    let error =
        bind_standard_index_read(&snapshot, "metric_value", hash, &tiny_control).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(tiny.peak(), 0);
    let resources = QueryResources::new(Some(1024 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let bound = bind(&snapshot, "metric_value", &control);
    let retained = resources.used();
    assert!(retained > 0);
    let value = Value::String("x".repeat(128 * 1024));
    let error = scan_bound_index_page(&snapshot, &bound, &value, None, 1, &control).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(resources.used(), retained);
    let interrupted = Cell::new(true);
    let cancelled = || interrupted.get();
    let cancelled_control = ExecutionControl::new(&resources, &cancelled);
    assert_eq!(
        bind_standard_index_read(&snapshot, "metric_value", hash, &cancelled_control)
            .unwrap_err()
            .kind,
        QueryErrorKind::Interrupted
    );
    assert_eq!(
        scan_bound_index_page(
            &snapshot,
            &bound,
            &Value::Integer(1),
            None,
            1,
            &cancelled_control
        )
        .unwrap_err()
        .kind,
        QueryErrorKind::Interrupted
    );
    drop(bound);
    assert_eq!(resources.used(), 0);
}

fn standard_seven_owners(
    snapshot: &Snapshot<'_>,
    control: &ExecutionControl<'_>,
) -> QueryResult<Vec<i64>> {
    let seek = super::super::StandardIndexSeek {
        index_name: "metric_value".to_owned(),
        kind: StandardIndexKind::Range,
        predicates: vec![(
            0,
            super::super::StandardIndexPredicate::Equal(Value::Integer(7)),
        )],
    };
    Ok(super::super::cache::scan_node_index_page(snapshot, &seek, None, 8, control)?.items)
}

fn malformed_manifest_updates() -> &'static [&'static str] {
    &[
        "UPDATE main._lithograph_index_generations SET definition_blob='broken'",
        "UPDATE main._lithograph_index_generations SET definition_blob=42",
        "UPDATE main._lithograph_index_generations SET definition_blob=zeroblob(1048576)",
        "UPDATE main._lithograph_index_generations SET anchor_commit='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
        "UPDATE main._lithograph_index_generations SET anchor_commit=zeroblob(1)",
        "UPDATE main._lithograph_index_generations SET anchor_commit=42",
        "UPDATE main._lithograph_index_generations SET definition_hash='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
        "UPDATE main._lithograph_index_generations SET definition_hash=zeroblob(1)",
        "UPDATE main._lithograph_index_generations SET definition_hash=42",
        "UPDATE main._lithograph_index_generations SET encoding_version='broken'",
        "UPDATE main._lithograph_index_generations SET encoding_version=X'31'",
        "UPDATE main._lithograph_index_generations SET complete='broken'",
        "UPDATE main._lithograph_index_generations SET complete=X'31'",
    ]
}

fn assert_initial_probe_rebuilds_bad_metadata(updates: &[&str], warm: bool, bound_probe: bool) {
    let connection = node_fixture("[7,7,7]");
    // Only this disposable fixture permits malformed derived CHECK values.
    connection
        .execute_batch("PRAGMA ignore_check_constraints=ON")
        .unwrap();
    let snapshot = active_snapshot(&connection);
    let resources = QueryResources::new(Some(256 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    for update in updates {
        connection
            .execute_batch("SAVEPOINT malformed_initial_probe")
            .unwrap();
        connection
            .execute_batch(
                "DELETE FROM temp._lithograph_standard_index_cache_meta; \
             DELETE FROM temp._lithograph_standard_index_cache_local; \
             DELETE FROM temp._lithograph_standard_index_changed_owners",
            )
            .unwrap();
        if warm {
            assert_eq!(standard_seven_owners(&snapshot, &control).unwrap().len(), 3);
        }
        connection
            .execute_batch(update)
            .expect("mutate disposable derived metadata");
        let head = branch_head(&connection, "main").unwrap();
        if bound_probe {
            let bound = bind(&snapshot, "metric_value", &control);
            assert_eq!(
                owners(&snapshot, &bound, &Value::Integer(7), &control).len(),
                3,
                "{update}"
            );
        } else {
            assert_eq!(
                standard_seven_owners(&snapshot, &control).unwrap().len(),
                3,
                "{update}"
            );
        }
        assert_eq!(branch_head(&connection, "main").unwrap(), head);
        assert_eq!(
            resources.used(),
            0,
            "temporary claims must be released: {update}"
        );
        connection
            .execute_batch("ROLLBACK TO malformed_initial_probe; RELEASE malformed_initial_probe")
            .unwrap();
    }
}

#[test]
fn cold_and_warm_standard_scans_rebuild_type_invalid_persistent_manifests() {
    for warm in [false, true] {
        assert_initial_probe_rebuilds_bad_metadata(malformed_manifest_updates(), warm, false);
    }
}

#[test]
fn first_bound_bind_rebuilds_type_invalid_persistent_manifests() {
    for warm in [false, true] {
        assert_initial_probe_rebuilds_bad_metadata(malformed_manifest_updates(), warm, true);
    }
}

#[test]
fn initial_probe_rejects_bad_temp_source_types_before_cloning_or_binding() {
    let updates = [
        "UPDATE temp._lithograph_standard_index_cache_meta SET generation_id='broken'",
        "UPDATE temp._lithograph_standard_index_cache_meta SET generation_anchor='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
        "UPDATE temp._lithograph_standard_index_cache_meta SET generation_anchor=zeroblob(1048576)",
        "UPDATE temp._lithograph_standard_index_cache_meta SET generation_id=NULL,generation_anchor=42",
        "UPDATE temp._lithograph_standard_index_cache_meta SET generation_id=0",
    ];
    for bound_probe in [false, true] {
        assert_initial_probe_rebuilds_bad_metadata(&updates, true, bound_probe);
    }
}

#[test]
fn metadata_fallback_never_writes_main_and_preserves_read_authorization_errors() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    let connection = node_fixture("[7,7,7]");
    let snapshot = active_snapshot(&connection);
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    assert_eq!(standard_seven_owners(&snapshot, &control).unwrap().len(), 3);
    connection
        .execute_batch("UPDATE main._lithograph_index_generations SET definition_blob='broken'")
        .unwrap();
    connection
        .authorizer(Some(|context: AuthContext<'_>| {
            if context.database_name == Some("main")
                && matches!(
                    context.action,
                    AuthAction::Insert { .. }
                        | AuthAction::Update { .. }
                        | AuthAction::Delete { .. }
                )
            {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .expect("deny main writes");
    let bound = bind(&snapshot, "metric_value", &control);
    assert_eq!(bound.source, BoundSource::Local);
    assert_eq!(
        owners(&snapshot, &bound, &Value::Integer(7), &control).len(),
        3
    );
    drop(bound);
    connection
        .authorizer(Some(|context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Read {
                    table_name: "_lithograph_index_generations",
                    ..
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .expect("deny manifest reads");
    connection
        .execute_batch("DELETE FROM temp._lithograph_standard_index_cache_meta")
        .unwrap();
    let error =
        standard_seven_owners(&snapshot, &control).expect_err("READ AUTH is not corrupt advice");
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_AUTH));
    connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .expect("remove authorizer");
}
