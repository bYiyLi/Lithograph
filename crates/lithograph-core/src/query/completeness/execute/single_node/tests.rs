use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

use crate::cypher::{AstKind, AstNode, ClauseKind};
use crate::query::plan::MatchStep;
use crate::query::{QueryErrorKind, QueryResult};

type LowerMatch = fn(&Connection, &AstNode, &[String]) -> QueryResult<Option<MatchStep>>;

fn assert_dictionary_auth_is_not_an_optional_fallback(
    lower: LowerMatch,
    query: &str,
    incoming: &[String],
) -> MatchStep {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    crate::storage::create_storage_schema(&connection).expect("public storage schema");
    let parsed = crate::cypher::parse(query)
        .expect("parse the exact helper input without resolving a Snapshot");
    let clause = parsed
        .root
        .descendants()
        .find(|node| node.kind == AstKind::Clause(ClauseKind::Match))
        .expect("MATCH clause");
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
        .expect("install one-shot canonical dictionary refusal");

    let error = lower(&connection, clause, incoming)
        .expect_err("the helper must propagate the first dictionary error without fallback/retry");

    assert_eq!(error.kind, QueryErrorKind::AccessDenied);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_AUTH));
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .expect("remove the authorizer before retrying the same helper");
    let recovered = lower(&connection, clause, incoming)
        .expect("dictionary access recovers")
        .expect("the unchanged fresh single-node shape is supported");
    assert_eq!(recovered.parts.len(), 1);
    assert_eq!(recovered.parts[0].start.variable.as_deref(), Some("a"));
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    recovered
}

#[test]
fn single_node_lowering_preserves_one_shot_dictionary_access_denial() {
    assert_dictionary_auth_is_not_an_optional_fallback(
        super::lower_match,
        "MATCH(a:Probe) WHERE a IN members RETURN a",
        &["members".to_owned()],
    );
}

#[test]
fn sip_lowering_preserves_one_shot_dictionary_access_denial() {
    assert_dictionary_auth_is_not_an_optional_fallback(
        super::super::sip::lower_match,
        "MATCH(a:Probe) WHERE a IN members RETURN a",
        &["members".to_owned()],
    );
}

fn lower_identity_match(
    connection: &Connection,
    clause: &AstNode,
    incoming: &[String],
) -> QueryResult<Option<MatchStep>> {
    super::super::identity::lower_identity_match(connection, clause, &BTreeMap::new(), incoming)
}

#[test]
fn identity_lowering_preserves_one_shot_dictionary_access_denial() {
    let recovered = assert_dictionary_auth_is_not_an_optional_fallback(
        lower_identity_match,
        "MATCH(a:Probe) WHERE elementId(a)='n:1' RETURN a",
        &[],
    );
    assert_eq!(recovered.parts[0].start.id_seek, Some(1));
}
