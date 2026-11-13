use lithograph_core::query::{QueryErrorKind, SnapshotSelector};

use super::*;

#[test]
fn historical_checkpoint_and_current_overlay_keep_their_own_relationship_identity() {
    let fixture = CycleFixture::new(Shape::Triangle, 16, 1);
    let old = storage::branch_head(&fixture.db, "main").unwrap();
    let expected = fixture.expected();
    let removed = fixture.relationships[0][&(0, 0)][0].clone();
    let deleted = support::execute(
        &fixture.db,
        "MATCH()-[r:S8A]->() WHERE elementId(r)=$removed DELETE r",
        BTreeMap::from([("removed".into(), removed.clone())]),
        ExecutionOptions::default(),
        1,
    )
    .unwrap();
    assert_eq!(deleted.counters.relationships_deleted, 1);
    let created = support::execute(
        &fixture.db,
        "MATCH(a:S8D0),(b:S8D1) WHERE elementId(a)=$a AND elementId(b)=$b \
         CREATE(a)-[r:S8A]->(b) RETURN elementId(r),elementId(startNode(r)),elementId(endNode(r))",
        BTreeMap::from([
            ("a".into(), fixture.nodes[0][0].clone()),
            ("b".into(), fixture.nodes[1][0].clone()),
        ]),
        ExecutionOptions::default(),
        1,
    )
    .unwrap();
    assert_eq!(created.rows.len(), 1);
    assert_eq!(created.rows[0][1], fixture.nodes[0][0]);
    assert_eq!(created.rows[0][2], fixture.nodes[1][0]);
    let fresh = created.rows[0][0].clone();
    assert_ne!(fresh, removed);
    let current_expected = expected
        .iter()
        .cloned()
        .map(|mut row| {
            if row[3] == removed {
                row[3] = fresh.clone();
            }
            row
        })
        .collect::<Vec<_>>();
    let current = storage::branch_head(&fixture.db, "main").unwrap();
    assert_ne!(current, old);
    // First inspect the real checkpoint + tombstone/addition overlay, then
    // repeat after a derived checkpoint rebuild. Neither changes the oracle.
    for checkpoint in [false, true] {
        if checkpoint {
            support::checkpoint(&fixture.db);
        }
        let latest = read(
            &fixture.db,
            &format!("PROFILE {}", fixture.query()),
            BTreeMap::new(),
            ExecutionOptions::default(),
            7,
        );
        assert_bag(&latest.rows, &current_expected);
        let mut options = ExecutionOptions::default();
        options.snapshot = SnapshotSelector::Commit(old.to_hex());
        let historical = read(
            &fixture.db,
            &format!("PROFILE {}", fixture.query()),
            BTreeMap::new(),
            options,
            5,
        );
        assert_bag(&historical.rows, &expected);
        assert_multiway(&historical, expected.len());
        assert_eq!(historical.commit, Some(format!("commit/{}", old.to_hex())));
    }
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), current);
    assert!(fixture.db.is_autocommit());
}

#[test]
fn graph_view_and_query_permissions_preserve_visibility_and_function_denial() {
    let fixture = CycleFixture::new(Shape::Triangle, 16, 1);
    let expected = fixture.expected();
    let before = storage::branch_head(&fixture.db, "main").unwrap();
    for (options, visible) in [
        (
            r#"{"graphView":{"requireAnyLabels":["S8D0","S8D1","S8D2"]}}"#,
            true,
        ),
        (r#"{"graphView":{"excludeAnyLabels":["S8D2"]}}"#, false),
        (
            r#"{"readAccess":{"query":{"denyProperties":["seq"]}}}"#,
            true,
        ),
    ] {
        let options = ExecutionOptions::parse_text(options).unwrap();
        let result = read(
            &fixture.db,
            &format!("PROFILE {}", fixture.query()),
            BTreeMap::new(),
            options,
            11,
        );
        assert_bag(&result.rows, if visible { &expected } else { &[] });
    }
    let options =
        ExecutionOptions::parse_text(r#"{"readAccess":{"denyFunctions":["elementId"]}}"#).unwrap();
    let _scope = support::ReadScope::new(&fixture.db);
    let error =
        support::execute(&fixture.db, &fixture.query(), BTreeMap::new(), options, 1).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::AccessDenied);
    drop(_scope);
    let recovered = read(
        &fixture.db,
        &format!("PROFILE {}", fixture.query()),
        BTreeMap::new(),
        ExecutionOptions::default(),
        13,
    );
    assert_bag(&recovered.rows, &expected);
    assert_multiway(&recovered, expected.len());
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
}

#[test]
fn a_write_barrier_observes_new_staged_edges_and_failed_output_rolls_back() {
    let fixture = CycleFixture::new(Shape::Triangle, 16, 1);
    let removed = fixture.relationships[0][&(0, 0)][0].clone();
    let original = storage::branch_head(&fixture.db, "main").unwrap();
    let prefix = "MATCH(a:S8D0)-[old:S8A]->(b:S8D1) WHERE elementId(old)=$removed \
                  DELETE old WITH a,b CREATE(a)-[fresh:S8A]->(b) WITH elementId(fresh) AS fresh ";
    let text = format!(
        "PROFILE {prefix}{}",
        fixture.query().replace(" RETURN ", " RETURN fresh,")
    );
    let result = support::execute(
        &fixture.db,
        &text,
        BTreeMap::from([("removed".into(), removed.clone())]),
        ExecutionOptions::default(),
        3,
    )
    .unwrap();
    assert_eq!(result.counters.relationships_deleted, 1);
    assert_eq!(result.counters.relationships_created, 1);
    let fresh = result.rows[0][0].clone();
    assert!(matches!(&fresh, Value::String(id) if id.starts_with("r:")));
    let expected = fixture
        .expected()
        .into_iter()
        .map(|mut row| {
            if row[3] == removed {
                row[3] = fresh.clone();
            }
            row.insert(0, fresh.clone());
            row
        })
        .collect::<Vec<_>>();
    assert_bag(&result.rows, &expected);
    assert!(
        !result.plan.contains("MultiwayIntersect"),
        "the incoming write barrier retains the proved fallback: {}",
        result.plan
    );
    let current = storage::branch_head(&fixture.db, "main").unwrap();
    assert_ne!(current, original);
    let bad = format!(
        "{prefix}{}",
        fixture.query().split(" RETURN ").next().unwrap().to_owned() + " RETURN 1/0"
    );
    let error = support::execute(
        &fixture.db,
        &bad,
        BTreeMap::from([("removed".into(), fresh)]),
        ExecutionOptions::default(),
        1,
    )
    .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Type);
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), current);
    assert!(fixture.db.is_autocommit());
    let recovered = read(
        &fixture.db,
        &fixture.query(),
        BTreeMap::new(),
        ExecutionOptions::default(),
        7,
    );
    assert_bag(
        &recovered.rows,
        &expected
            .into_iter()
            .map(|row| row.into_iter().skip(1).collect())
            .collect::<Vec<_>>(),
    );
}
