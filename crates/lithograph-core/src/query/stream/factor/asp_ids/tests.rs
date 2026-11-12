use std::cell::Cell;

use super::*;
use crate::query::{QueryErrorKind, QueryResources};

fn populate(ids: &mut AspNodeIds, count: i64, control: &ExecutionControl<'_>) {
    for (index, id) in (1..=count).rev().enumerate() {
        ids.insert(id, control)
            .expect("insert a complete-set identity");
        ids.insert(id, control)
            .expect("duplicate insertion must succeed");
        assert_eq!(
            ids.len(),
            index as u64 + 1,
            "duplicates cannot change multiplicity"
        );
    }
}

fn drain(ids: &mut AspNodeIds, count: i64, control: &ExecutionControl<'_>) -> usize {
    let baseline = control.resources().used();
    let mut next = 1;
    let mut pages = 0;
    while let Some(page) = ids.next_page(control).expect("bounded complete-set page") {
        assert!(!page.0.is_empty() && page.0.len() <= PAGE_MAX);
        assert!(page.1.bytes() >= page_bytes(page.0.capacity()).expect("actual page capacity"));
        assert_eq!(control.resources().used(), baseline + page.1.bytes());
        for id in &page.0 {
            assert_eq!(
                *id, next,
                "all distinct identities must remain sorted across pages"
            );
            next += 1;
        }
        drop(page);
        assert_eq!(
            control.resources().used(),
            baseline,
            "page claim follows its data"
        );
        pages += 1;
    }
    assert_eq!(next, count + 1);
    assert!(
        ids.next_page(control)
            .expect("stable complete EOF")
            .is_none()
    );
    assert_eq!(control.resources().used(), baseline);
    pages
}

#[test]
fn no_limit_still_spills_and_pages_the_complete_deduplicated_set() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut ids = AspNodeIds::new(&control).expect("NodeID set");
    let resident_count =
        FACTOR_RESIDENT_BYTES / map_entry_bytes::<i64>().expect("resident entry fee");
    let count = (resident_count + 2 * PAGE_MAX as u64) as i64;
    populate(&mut ids, count, &control);

    assert!(
        ids.connection.is_some(),
        "NoLimit cannot disable the resident soft limit"
    );
    assert!(ids.resident.is_empty());
    assert_eq!(ids.resident_memory.bytes(), 0);
    assert_eq!(ids.len(), count as u64);
    assert!(
        ids.sqlite_vm_steps() > 0,
        "migration must execute real private SQLite statements"
    );
    assert!(drain(&mut ids, count, &control) >= 3);
    drop(ids);
    assert_eq!(resources.used(), 0);
}

#[test]
fn hard_64k_migrates_and_drains_one_thousand_ids_without_exceeding_quota() {
    let limit = 64 * 1024;
    let resources = QueryResources::new(Some(limit));
    let control = ExecutionControl::new(&resources, &|| false);
    let minimum = size_of::<AspNodeIds>() as u64
        + FACTOR_PRIVATE_RUST_BYTES
        + page_bytes(PAGE_MIN).expect("minimum page fee")
        + map_entry_bytes::<i64>().expect("resident entry fee");
    assert!(
        minimum < limit,
        "the fixture must cover the actual migration floor"
    );
    let mut ids = AspNodeIds::new(&control).expect("budgeted NodeID set");
    populate(&mut ids, 1000, &control);
    assert!(
        ids.connection.is_some(),
        "the hard quota must trigger migration"
    );
    assert_eq!(ids.len(), 1000);
    assert!(drain(&mut ids, 1000, &control) >= 4);
    assert!(resources.peak() <= limit);
    drop(ids);
    assert_eq!(resources.used(), 0);
}

fn assert_cancelled_owner(ids: &AspNodeIds, error: QueryError, resources: &QueryResources) {
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    assert!(ids.failed);
    assert!(
        ids.connection.is_none(),
        "cancellation closes the private transaction"
    );
    assert!(ids.resident.is_empty());
    assert!(ids.private_memory.is_none());
    assert_eq!(ids.resident_memory.bytes(), 0);
    assert_eq!(resources.used(), size_of::<AspNodeIds>() as u64);
}

fn cancel_after_private_insert() {
    let resources = QueryResources::new(Some(64 * 1024));
    let mut ids = {
        let control = ExecutionControl::new(&resources, &|| false);
        let mut ids = AspNodeIds::new(&control).expect("NodeID set");
        populate(&mut ids, 1000, &control);
        ids
    };
    assert!(ids.connection.is_some());
    let before = ids.sqlite_vm_steps();
    let checks = Cell::new(0);
    let interrupted = || {
        checks.set(checks.get() + 1);
        checks.get() >= 4
    };
    // A fresh work counter makes the fourth check the post-INSERT check, after
    // owner/reservation validation and the statement's pre-execution check.
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = ids
        .insert(1001, &control)
        .expect_err("cancel after the actual private INSERT");
    assert_eq!(checks.get(), 4);
    assert!(
        ids.sqlite_vm_steps() > before,
        "SQL must run before cancellation"
    );
    assert_cancelled_owner(&ids, error, &resources);
    drop(ids);
    assert_eq!(resources.used(), 0);
}

fn cancel_while_reading_private_page() {
    let resources = QueryResources::new(Some(64 * 1024));
    let mut ids = {
        let control = ExecutionControl::new(&resources, &|| false);
        let mut ids = AspNodeIds::new(&control).expect("NodeID set");
        populate(&mut ids, 1000, &control);
        ids
    };
    assert!(ids.connection.is_some());
    let baseline = resources.used();
    let before = ids.sqlite_vm_steps();
    let live_checks = Cell::new(0);
    let observed_live = Cell::new(0);
    let interrupted = || {
        let used = resources.used();
        if used <= baseline {
            return false;
        }
        observed_live.set(used);
        live_checks.set(live_checks.get() + 1);
        live_checks.get() >= 2
    };
    // The first live-page check precedes SQL. The second is the 256-row work
    // checkpoint, so cancellation observes both a real page lease and real VM.
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = ids
        .next_page(&control)
        .expect_err("cancel inside private page consumption");
    assert_eq!(live_checks.get(), 2);
    assert!(observed_live.get() > baseline);
    assert!(
        ids.sqlite_vm_steps() > before,
        "page SQL must execute before cancellation"
    );
    assert_cancelled_owner(&ids, error, &resources);
    drop(ids);
    assert_eq!(resources.used(), 0);
}

#[test]
fn cancellation_after_private_work_closes_insert_and_page_owners() {
    cancel_after_private_insert();
    cancel_while_reading_private_page();
}
