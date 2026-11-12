use std::collections::BTreeSet;

use lithograph_core::query::{QueryCursor, QueryErrorKind, prepare};

use super::*;

#[test]
fn live_factorized_output_can_close_or_interrupt_and_reopen_without_retained_leases() {
    let fixture = StarFixture::new(64, 128);
    let expected = fixture.expected_rows();
    let members = support::bag(&expected).into_iter().collect::<BTreeSet<_>>();
    let head = storage::branch_head(&fixture.db, "main").unwrap();
    for interrupt in [false, true] {
        let scope = support::ReadScope::new(&fixture.db);
        let prepared = prepare(
            &fixture.db,
            &format!("PROFILE {STAR}"),
            params(),
            ExecutionOptions::default(),
        )
        .unwrap();
        assert!(prepared.physical.explain().contains("FactorizedAccumulate"));
        let resources = prepared.resources.clone();
        let mut cursor = QueryCursor::new(prepared);
        {
            let batch = cursor.next_batch(&fixture.db, 2).unwrap();
            assert_eq!(batch.rows.len(), 2);
            assert!(!batch.done);
            assert_eq!(
                support::bag(&batch.rows)
                    .into_iter()
                    .collect::<BTreeSet<_>>()
                    .len(),
                2,
                "each captured relationship pair occurs only once in this fixture"
            );
            assert!(
                batch
                    .rows
                    .iter()
                    .all(|row| members.contains(&format!("{row:?}")))
            );
        }
        if interrupt {
            let error = cursor
                .next_batch_with_interrupt(&fixture.db, 2, &|| true)
                .unwrap_err();
            assert_eq!(error.kind, QueryErrorKind::Interrupted);
            assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
        } else {
            cursor.cancel(&fixture.db).unwrap();
        }
        drop(cursor);
        assert_eq!(resources.used(), 0);
        drop(scope);
        assert!(fixture.db.is_autocommit());
        assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), head);
        let recovered = read(
            &fixture.db,
            &format!("PROFILE {STAR}"),
            params(),
            ExecutionOptions::default(),
            256,
        );
        assert_bag(&recovered.rows, &expected);
        assert_factorized(&recovered, 32768);
    }
}

#[test]
fn an_admitted_group_preserves_resource_failure_when_the_shared_grant_is_consumed() {
    let fixture = StarFixture::new(64, 128);
    let head = storage::branch_head(&fixture.db, "main").unwrap();
    let scope = support::ReadScope::new(&fixture.db);
    let mut options = ExecutionOptions::default();
    options.memory_limit_bytes = Some(8 * 1024 * 1024);
    let prepared = prepare(&fixture.db, &format!("PROFILE {STAR}"), params(), options).unwrap();
    assert!(prepared.physical.explain().contains("FactorizedAccumulate"));
    let resources = prepared.resources.clone();
    let held = resources
        .reserve(resources.limit().unwrap() - resources.used())
        .unwrap();
    let mut cursor = QueryCursor::new(prepared);
    let error = cursor.next_batch(&fixture.db, 1).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    drop(held);
    drop(cursor);
    assert_eq!(resources.used(), 0);
    drop(scope);
    assert!(fixture.db.is_autocommit());
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), head);
    let recovered = read(
        &fixture.db,
        &format!("PROFILE {STAR}"),
        params(),
        ExecutionOptions::default(),
        256,
    );
    assert_bag(&recovered.rows, &fixture.expected_rows());
    assert_factorized(&recovered, 32768);
}
