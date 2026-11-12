//! Query-local groups never outlive their pinned graph state or access rules.

use std::collections::BTreeMap;

use lithograph_core::query::{QueryErrorKind, SnapshotSelector};

use super::*;
use support::{ArmRow, Observed};

const REPLACE: &str = "PROFILE MATCH(h:Hub)-[old:A]->(a:LeafA) \
    WHERE elementId(old)=$removed DELETE old WITH h,a \
    CREATE(h)-[fresh:A]->(a) WITH elementId(fresh) AS created \
    MATCH(h:Hub)-[r:A]->(a:LeafA),(h)-[s:B]->(b:LeafB) WHERE h.k=$key \
    RETURN created,elementId(h),elementId(r),elementId(a),elementId(s),elementId(b)";

fn no_group(result: &Observed) {
    assert!(
        !result.plan.contains("FactorizedAccumulate"),
        "{}",
        result.plan
    );
    assert!(result.profile.iter().all(|operator| {
        !matches!(
            operator.operator.as_str(),
            "FactorizedAccumulate" | "ASPJoin"
        )
    }));
}

fn star(fixture: &StarFixture, options: ExecutionOptions) -> Observed {
    read(
        &fixture.db,
        &format!("PROFILE {STAR}"),
        params(),
        options,
        3,
    )
}

fn oracle(fixture: &StarFixture, options: ExecutionOptions, expected: &[Vec<Value>]) {
    let result = read(
        &fixture.db,
        &format!(
            "PROFILE {}",
            STAR.replace("h.k=$key", "(h.k=$key) OR false")
        ),
        params(),
        options,
        256,
    );
    no_group(&result);
    assert_bag(&result.rows, expected);
}

fn replacement_params(removed: &Value) -> BTreeMap<String, Value> {
    let mut bindings = params();
    bindings.insert("removed".to_owned(), removed.clone());
    bindings
}

fn replaced_bag(fixture: &StarFixture, removed: &Value, created: &Value) -> Vec<Vec<Value>> {
    fixture
        .expected_rows()
        .into_iter()
        .map(|mut row| {
            if &row[1] == removed {
                row[1] = created.clone();
            }
            row
        })
        .collect()
}

#[test]
fn historical_checkpoint_keeps_the_original_group_after_relationship_replacement() {
    let fixture = StarFixture::new(64, 128);
    let old = storage::branch_head(&fixture.db, "main").unwrap();
    let removed = fixture.left[0].relationship.clone();
    let deleted = support::execute(
        &fixture.db,
        "MATCH()-[r:A]->() WHERE elementId(r)=$removed DELETE r",
        replacement_params(&removed),
        ExecutionOptions::default(),
        1,
    )
    .unwrap();
    assert_eq!(deleted.counters.relationships_deleted, 1);
    let created = support::execute(
        &fixture.db,
        "MATCH(h:Hub),(a:LeafA) WHERE h.k=7 AND elementId(a)=$endpoint \
         CREATE(h)-[r:A]->(a) \
         RETURN elementId(r),elementId(startNode(r)),elementId(endNode(r))",
        BTreeMap::from([("endpoint".to_owned(), fixture.left[0].endpoint.clone())]),
        ExecutionOptions::default(),
        1,
    )
    .unwrap();
    assert_eq!(created.counters.relationships_created, 1);
    assert_eq!(created.rows.len(), 1);
    assert_eq!(created.rows[0][1], fixture.hub);
    assert_eq!(created.rows[0][2], fixture.left[0].endpoint);
    let fresh = &created.rows[0][0];
    assert_ne!(fresh, &removed);
    assert!(matches!(fresh, Value::String(id) if id.starts_with("r:")));
    support::checkpoint(&fixture.db);
    let current = storage::branch_head(&fixture.db, "main").unwrap();
    assert_ne!(current, old);
    let latest = star(&fixture, ExecutionOptions::default());
    assert_bag(&latest.rows, &replaced_bag(&fixture, &removed, fresh));
    assert_factorized(&latest, 32768);
    let mut historical = ExecutionOptions::default();
    historical.snapshot = SnapshotSelector::Commit(old.to_hex());
    let pinned = star(&fixture, historical.clone());
    assert_eq!(pinned.commit, Some(format!("commit/{}", old.to_hex())));
    assert_bag(&pinned.rows, &fixture.expected_rows());
    assert_factorized(&pinned, 32768);
    oracle(&fixture, historical, &fixture.expected_rows());
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), current);
    assert!(fixture.db.is_autocommit());
}

#[test]
fn graph_view_and_query_access_use_their_own_flat_bag_and_reject_denied_functions() {
    let fixture = StarFixture::new(64, 128);
    let head = storage::branch_head(&fixture.db, "main").unwrap();
    let expected = fixture.expected_rows();
    for (text, visible) in [
        (
            r#"{"graphView":{"requireAnyLabels":["Hub","LeafA","LeafB"]}}"#,
            true,
        ),
        (r#"{"graphView":{"excludeAnyLabels":["LeafB"]}}"#, false),
        (
            r#"{"readAccess":{"query":{"denyProperties":["seq"]}}}"#,
            true,
        ),
        (
            r#"{"readAccess":{"query":{"denyProperties":["k"]}}}"#,
            false,
        ),
    ] {
        let options = ExecutionOptions::parse_text(text).unwrap();
        let result = star(&fixture, options.clone());
        no_group(&result);
        let bag = if visible { expected.as_slice() } else { &[] };
        assert_bag(&result.rows, bag);
        oracle(&fixture, options, bag);
    }
    let denied =
        ExecutionOptions::parse_text(r#"{"readAccess":{"denyFunctions":["elementId"]}}"#).unwrap();
    let error = support::execute(&fixture.db, STAR, params(), denied, 1).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::AccessDenied);
    let recovered = star(&fixture, ExecutionOptions::default());
    assert_bag(&recovered.rows, &expected);
    assert_factorized(&recovered, 32768);
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), head);
    assert!(fixture.db.is_autocommit());
}

fn replace_and_check(fixture: &mut StarFixture) -> Value {
    let removed = fixture.left[0].relationship.clone();
    let old = storage::branch_head(&fixture.db, "main").unwrap();
    let result = support::execute(
        &fixture.db,
        REPLACE,
        replacement_params(&removed),
        ExecutionOptions::default(),
        3,
    )
    .unwrap();
    no_group(&result);
    assert_eq!(result.counters.relationships_deleted, 1);
    assert_eq!(result.counters.relationships_created, 1);
    assert_eq!(result.rows.len(), 32768);
    let created = result.rows[0][0].clone();
    assert!(matches!(&created, Value::String(id) if id.starts_with("r:")));
    assert_ne!(created, removed);
    let expected = replaced_bag(fixture, &removed, &created)
        .into_iter()
        .map(|mut row| {
            row.insert(0, created.clone());
            row
        })
        .collect::<Vec<_>>();
    assert_bag(&result.rows, &expected);
    fixture.left[0] = ArmRow {
        relationship: created.clone(),
        endpoint: fixture.left[0].endpoint.clone(),
    };
    assert_ne!(storage::branch_head(&fixture.db, "main").unwrap(), old);
    assert!(fixture.db.is_autocommit());
    created
}

#[test]
fn repeated_write_barriers_read_new_staged_relationships_and_roll_back_failed_output() {
    let mut fixture = StarFixture::new(64, 128);
    let initial = star(&fixture, ExecutionOptions::default());
    assert_bag(&initial.rows, &fixture.expected_rows());
    assert_factorized(&initial, 32768);
    let first = replace_and_check(&mut fixture);
    let second = replace_and_check(&mut fixture);
    assert_ne!(first, second);
    let before = storage::branch_head(&fixture.db, "main").unwrap();
    let bad = REPLACE.replace(
        "RETURN created,elementId(h),elementId(r),elementId(a),elementId(s),elementId(b)",
        "RETURN created,1/0",
    );
    let error = support::execute(
        &fixture.db,
        &bad,
        replacement_params(&second),
        ExecutionOptions::default(),
        1,
    )
    .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Type);
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
    assert!(fixture.db.is_autocommit());
    support::checkpoint(&fixture.db);
    let recovered = star(&fixture, ExecutionOptions::default());
    assert_bag(&recovered.rows, &fixture.expected_rows());
    assert_factorized(&recovered, 32768);
    oracle(
        &fixture,
        ExecutionOptions::default(),
        &fixture.expected_rows(),
    );
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
}
