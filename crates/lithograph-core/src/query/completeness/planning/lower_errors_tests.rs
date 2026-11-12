use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

use super::*;
use crate::query::resources::QueryResources;

#[test]
fn program_planning_preserves_the_first_dictionary_access_denial() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
             id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, \
             database_id TEXT NOT NULL, storage_format INTEGER NOT NULL)",
        )
        .unwrap();
    crate::storage::create_storage_schema(&connection).unwrap();
    let root = crate::storage::initialize_root(&connection).unwrap();
    let snapshot = Snapshot::resolve(&connection, root.root).unwrap();
    let options = ExecutionOptions::default();
    let view = ResolvedGraphView::resolve(&connection, &options.graph_view).unwrap();
    let source = "WITH 1 AS seed MATCH(a:Probe) RETURN elementId(a)";
    let ast = crate::cypher::parse(source).unwrap();
    let mut program = prepare_program(&connection, &ast, source, &options).unwrap();
    let params = BTreeMap::new();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = attempts.clone();
    connection
        .authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Read {
                    table_name: "_lithograph_labels",
                    ..
                }
            ) && observed.fetch_add(1, Ordering::Relaxed) == 0
            {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();

    let error = program
        .plan_fixed_matches(&snapshot, &view, &params, &control)
        .expect_err("planning must not turn the first real dictionary failure into fallback");
    assert_eq!(error.kind, crate::query::QueryErrorKind::AccessDenied);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_AUTH));
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    assert!(program.match_plans.is_empty());
    assert_eq!(resources.used(), 0);
    connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    let memory = program
        .plan_fixed_matches(&snapshot, &view, &params, &control)
        .expect("the same planning context succeeds when dictionary access is restored");
    assert_eq!(program.match_plans.len(), 1);
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    drop(memory);
    drop(program);
    assert_eq!(resources.used(), 0);
}
