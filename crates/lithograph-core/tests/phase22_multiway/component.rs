//! One MATCH preserves the cyclic prefix's used RIDs in the original tail.

use lithograph_core::query::{QueryCursor, QueryErrorKind, prepare};

use super::*;

fn query(fixture: &CycleFixture, named: bool) -> String {
    let cycle = fixture.query();
    let (pattern, projection) = cycle.split_once(" RETURN ").unwrap();
    let relationship = if named { "[tail]" } else { "[]" };
    let identity = if named { ",elementId(tail)" } else { "" };
    format!(
        "{pattern},(n2)-{relationship}->(next:S8D0) \
         RETURN {projection}{identity},elementId(next)"
    )
}

fn expected(fixture: &CycleFixture, named: bool) -> Vec<Vec<Value>> {
    let mut result = Vec::new();
    for cycle in fixture.expected() {
        let from = fixture.nodes[2]
            .iter()
            .position(|node| node == &cycle[2])
            .unwrap();
        // The public CREATE map, rather than another MATCH, supplies every
        // outgoing C edge. Exactly the C RID already used by the prefix is
        // excluded; other endpoints and parallel identities remain distinct.
        for (&(source, target), relationships) in &fixture.relationships[2] {
            if source != from {
                continue;
            }
            for relationship in relationships {
                if cycle[3..6].contains(relationship) {
                    continue;
                }
                let mut row = cycle.clone();
                if named {
                    row.push(relationship.clone());
                }
                row.push(fixture.nodes[0][target].clone());
                result.push(row);
            }
        }
    }
    result
}

fn assert_component(actual: &support::Observed, expected: &[Vec<Value>]) {
    assert_bag(&actual.rows, expected);
    // This operator produces component rows. The original binary tail has
    // seven available RIDs per row and produces the final 2464-row bag.
    assert_multiway(actual, 352);
    let adjacency = actual_operator(actual, "AdjacencySeek");
    assert_eq!(adjacency.rows, 2464);
    assert!(adjacency.db_hits > 0);
    let joined = actual_operator(actual, "NestedLoop");
    assert_eq!(joined.rows, 2464);
}

#[test]
fn named_and_anonymous_untyped_tails_keep_the_same_match_relationship_exclusion() {
    let fixture = CycleFixture::new(Shape::Triangle, 16, 1);
    let before = storage::branch_head(&fixture.db, "main").unwrap();
    for named in [true, false] {
        let expected = expected(&fixture, named);
        assert_eq!(expected.len(), 352 * 7);
        if named {
            assert!(expected.iter().all(|row| !row[3..6].contains(&row[6])));
        }
        let actual = read(
            &fixture.db,
            &format!("PROFILE {}", query(&fixture, named)),
            BTreeMap::new(),
            ExecutionOptions::default(),
            11,
        );
        assert_component(&actual, &expected);
        assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
        assert!(fixture.db.is_autocommit());
    }
}

#[test]
fn complete_where_parameters_null_and_errors_keep_the_component_tail_boundary() {
    let fixture = CycleFixture::new(Shape::Triangle, 16, 1);
    let text = query(&fixture, true).replace(" RETURN ", " WHERE $gate RETURN ");
    let expected = expected(&fixture, true);
    let before = storage::branch_head(&fixture.db, "main").unwrap();
    for gate in [Value::Boolean(true), Value::Boolean(false), Value::Null] {
        let actual = read(
            &fixture.db,
            &format!("PROFILE {text}"),
            BTreeMap::from([("gate".into(), gate.clone())]),
            ExecutionOptions::default(),
            13,
        );
        if gate == Value::Boolean(true) {
            assert_component(&actual, &expected);
        } else {
            assert!(actual.rows.is_empty());
        }
    }
    let scope = support::ReadScope::new(&fixture.db);
    let error = support::execute(
        &fixture.db,
        &text,
        BTreeMap::from([("gate".into(), Value::Integer(1))]),
        ExecutionOptions::default(),
        1,
    )
    .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Type);
    drop(scope);
    let failed_projection = query(&fixture, true)
        .split(" RETURN ")
        .next()
        .unwrap()
        .to_owned()
        + " RETURN 1/0";
    let scope = support::ReadScope::new(&fixture.db);
    let error = support::execute(
        &fixture.db,
        &failed_projection,
        BTreeMap::new(),
        ExecutionOptions::default(),
        1,
    )
    .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Type);
    drop(scope);
    let recovered = read(
        &fixture.db,
        &format!("PROFILE {}", query(&fixture, true)),
        BTreeMap::new(),
        ExecutionOptions::default(),
        17,
    );
    assert_component(&recovered, &expected);
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
}

#[test]
fn partial_component_tail_output_cancels_and_recovers_under_a_small_budget() {
    let fixture = CycleFixture::new(Shape::Triangle, 16, 1);
    let text = format!("PROFILE {}", query(&fixture, true));
    let expected = expected(&fixture, true);
    let before = storage::branch_head(&fixture.db, "main").unwrap();
    for interrupt in [false, true] {
        let scope = support::ReadScope::new(&fixture.db);
        let prepared = prepare(
            &fixture.db,
            &text,
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .unwrap();
        assert!(prepared.physical.explain().contains("MultiwayIntersect"));
        assert!(prepared.physical.explain().contains("NestedLoop"));
        let resources = prepared.resources.clone();
        let mut cursor = QueryCursor::new(prepared);
        {
            let first = cursor.next_batch(&fixture.db, 1).unwrap();
            assert_eq!(first.rows.len(), 1);
            assert!(!first.done);
            assert!(expected.contains(&first.rows[0]));
        }
        if interrupt {
            let error = cursor
                .next_batch_with_interrupt(&fixture.db, 1, &|| true)
                .unwrap_err();
            assert_eq!(error.kind, QueryErrorKind::Interrupted);
            assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
        } else {
            cursor.cancel(&fixture.db).unwrap();
        }
        drop(cursor);
        assert_eq!(resources.used(), 0);
        drop(scope);
        assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
        assert!(fixture.db.is_autocommit());
    }
    let mut options = ExecutionOptions::default();
    options.memory_limit_bytes = Some(512 * 1024);
    let recovered = read(&fixture.db, &text, BTreeMap::new(), options, 3);
    assert_component(&recovered, &expected);
    assert!(recovered.memory_peak_bytes <= 512 * 1024);
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
}
