use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

use super::{ClauseStreamBuilder, SeedSource};
use crate::cypher::{AstKind, ClauseKind};
use crate::query::completeness::prepare_program;
use crate::query::{ExecutionControl, ExecutionOptions, QueryErrorKind, QueryResources};
use crate::storage::{Snapshot, create_storage_schema, initialize_root};

fn fixture() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
             id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, \
             database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);",
        )
        .expect("metadata schema");
    create_storage_schema(&connection).expect("public storage schema");
    initialize_root(&connection).expect("public root initialization");
    connection
}

#[test]
fn match_source_preserves_one_shot_dictionary_access_denial() {
    let connection = fixture();
    let text = "MATCH(a:Probe) RETURN a";
    let ast = crate::cypher::parse(text).expect("parse the plain MATCH");
    let program = prepare_program(&connection, &ast, text, &ExecutionOptions::default())
        .expect("prepare the actual builder context before installing the authorizer");
    let root = initialize_root(&connection).expect("existing public root");
    let snapshot = Snapshot::resolve(&connection, root.root).expect("root Snapshot");
    let clause = ast
        .root
        .descendants()
        .find(|node| node.kind == AstKind::Clause(ClauseKind::Match))
        .expect("MATCH clause");
    let params = BTreeMap::new();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let builder = ClauseStreamBuilder {
        connection: &connection,
        program: &program,
        _snapshot: &snapshot,
        params: &params,
        control: &control,
    };
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
        .expect("install the first dictionary READ refusal after all context preparation");

    let error = builder
        .match_source(
            Box::new(SeedSource { emitted: false }),
            &[],
            &["a".to_owned()],
            clause,
        )
        .expect_err("the actual builder must propagate the first canonical dictionary failure");
    assert_eq!(error.kind, QueryErrorKind::AccessDenied);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_AUTH));
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    assert_eq!(
        resources.used(),
        0,
        "failed source construction releases its leases"
    );
    connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .expect("remove the authorizer");
    let recovered = builder
        .match_source(
            Box::new(SeedSource { emitted: false }),
            &[],
            &["a".to_owned()],
            clause,
        )
        .expect("the unchanged builder context recovers")
        .expect("the plain fresh Node MATCH is streamable");
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    drop(recovered);
    assert_eq!(
        resources.used(),
        0,
        "dropping the real source releases its leases"
    );
}
