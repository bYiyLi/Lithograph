use std::cell::Cell;

use lithograph_core::query::{QueryCursor, QueryErrorKind, prepare};

use super::*;

#[test]
fn literal_and_parameter_limits_do_not_borrow_complete_consumption_costs() {
    let fixture = CycleFixture::new(Shape::Triangle, 16, 1);
    let expected = fixture.expected();
    for take in [0, 1, 10] {
        for parameter in [false, true] {
            let text = if parameter {
                format!("PROFILE {} LIMIT $take", fixture.query())
            } else {
                format!("PROFILE {} LIMIT {take}", fixture.query())
            };
            let params = if parameter {
                BTreeMap::from([("take".into(), Value::Integer(take))])
            } else {
                BTreeMap::new()
            };
            let result = read(&fixture.db, &text, params, ExecutionOptions::default(), 1);
            assert_eq!(result.rows.len(), usize::try_from(take).unwrap());
            assert!(
                !result.plan.contains("MultiwayIntersect"),
                "{}",
                result.plan
            );
            let mut remaining = support::bag(&expected);
            for row in support::bag(&result.rows) {
                let found = remaining
                    .iter()
                    .position(|candidate| candidate == &row)
                    .unwrap();
                remaining.swap_remove(found);
            }
            if take == 0 {
                assert_eq!(result.db_hits, 0);
                assert!(result.profile.iter().all(|operator| operator.rows == 0));
            }
        }
    }
}

#[test]
fn close_interrupt_and_small_budget_release_query_owned_intersection_state() {
    let fixture = CycleFixture::new(Shape::Triangle, 16, 1);
    let expected = fixture.expected();
    let original = storage::branch_head(&fixture.db, "main").unwrap();
    for interrupt in [false, true] {
        let scope = support::ReadScope::new(&fixture.db);
        let prepared = prepare(
            &fixture.db,
            &format!("PROFILE {}", fixture.query()),
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .unwrap();
        assert!(prepared.physical.explain().contains("MultiwayIntersect"));
        let resources = prepared.resources.clone();
        let mut cursor = QueryCursor::new(prepared);
        {
            let batch = cursor.next_batch(&fixture.db, 1).unwrap();
            assert_eq!(batch.rows.len(), 1);
            assert!(!batch.done);
            assert!(expected.contains(&batch.rows[0]));
        }
        if interrupt {
            let error = cursor
                .next_batch_with_interrupt(&fixture.db, 1, &|| true)
                .unwrap_err();
            assert_eq!(error.kind, QueryErrorKind::Interrupted);
        } else {
            cursor.cancel(&fixture.db).unwrap();
        }
        drop(cursor);
        assert_eq!(resources.used(), 0);
        drop(scope);
        assert!(fixture.db.is_autocommit());
        assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), original);
    }
    let mut options = ExecutionOptions::default();
    options.memory_limit_bytes = Some(512 * 1024);
    let result = read(
        &fixture.db,
        &format!("PROFILE {}", fixture.query()),
        BTreeMap::new(),
        options,
        3,
    );
    assert_bag(&result.rows, &expected);
    assert_multiway(&result, expected.len());
    assert!(result.memory_peak_bytes <= 512 * 1024);
}

#[test]
fn cooperative_interrupt_during_the_first_advance_is_terminal_and_recovers() {
    let fixture = CycleFixture::new(Shape::Triangle, 16, 1);
    let scope = support::ReadScope::new(&fixture.db);
    let prepared = prepare(
        &fixture.db,
        &format!("PROFILE {}", fixture.query()),
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert!(prepared.physical.explain().contains("MultiwayIntersect"));
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let calls = Cell::new(0usize);
    let error = cursor
        .next_batch_with_interrupt(&fixture.db, 4096, &|| {
            calls.set(calls.get() + 1);
            calls.get() >= 128
        })
        .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    assert_eq!(calls.get(), 128);
    assert!(cursor.next_batch(&fixture.db, 1).unwrap().done);
    drop(cursor);
    assert_eq!(resources.used(), 0);
    drop(scope);
    let recovered = read(
        &fixture.db,
        &format!("PROFILE {}", fixture.query()),
        BTreeMap::new(),
        ExecutionOptions::default(),
        31,
    );
    assert_bag(&recovered.rows, &fixture.expected());
    assert_multiway(&recovered, fixture.expected().len());
}
