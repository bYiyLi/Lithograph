use super::*;
use std::cell::RefCell;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

#[test]
fn property_admission_reports_the_exact_checkpoint_string_peak_before_either_claim() {
    let payload = "λ".repeat(2048);
    let (connection, commit, node, key) = fixture(PropertyValue::String(payload.clone()));
    create_checkpoint(&connection, commit).unwrap();
    let (ledger, observer) = budget(512 * 1024);
    let state =
        Snapshot::resolve_query_state_observed(&connection, commit, observer.clone(), &|| Ok(()))
            .unwrap();
    let snapshot = Snapshot::from_resolved_state(&connection, &state);
    let baseline = ledger.used.get();
    let held = RefCell::new(Some(observer.reserve(ledger.limit - baseline).unwrap()));
    ledger.largest_request.set(0);
    let selects = Arc::new(AtomicUsize::new(0));
    let observed_selects = selects.clone();
    connection
        .authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::Select) {
                observed_selects.fetch_add(1, Ordering::Relaxed);
            }
            Authorization::Allow
        }))
        .unwrap();
    let retained = std::mem::size_of::<PropertyValue>() as u64 + payload.len() as u64 + 256;
    let temporary = std::mem::size_of::<PropertyColumns>() as u64 + payload.len() as u64;
    let calls = Cell::new(0);
    let mut admission = |peak| -> StorageResult<()> {
        assert_eq!(peak, retained + temporary);
        assert_eq!(ledger.used.get(), ledger.limit);
        assert_eq!(
            ledger.largest_request.get(),
            0,
            "no read claim precedes admission"
        );
        calls.set(calls.get() + 1);
        drop(held.borrow_mut().take());
        Ok(())
    };
    snapshot
        .with_property_admitted_checked(
            OwnerKind::Node,
            node,
            key,
            &|| Ok(()),
            &mut admission,
            |value, _| {
                assert!(matches!(value, Some(PropertyValue::String(value)) if value == &payload));
                assert_eq!(ledger.used.get(), baseline + retained);
                Ok::<_, StorageError>(())
            },
        )
        .unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(
        selects.load(Ordering::Relaxed),
        1,
        "freeing the held lease admits the same property statement without a retry"
    );
    connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert!(held.borrow().is_none());
    assert_eq!(ledger.largest_request.get(), retained.max(temporary));
    assert_eq!(ledger.used.get(), baseline);
    drop(snapshot);
    drop(state);
    assert_eq!(ledger.used.get(), 0);
}

#[test]
fn property_admission_is_forwarded_to_overlay_and_cache_string_conversion() {
    for cached in [false, true] {
        let payload = "retained string".repeat(32);
        let (connection, commit, node, key) = fixture(PropertyValue::String(payload.clone()));
        if cached {
            create_checkpoint(&connection, commit).unwrap();
        }
        let (ledger, observer) = budget(512 * 1024);
        let state =
            Snapshot::resolve_query_state_observed(&connection, commit, observer.clone(), &|| {
                Ok(())
            })
            .unwrap();
        let snapshot = Snapshot::from_resolved_state(&connection, &state);
        if cached {
            snapshot
                .prefetch_properties(OwnerKind::Node, &[node], &[key])
                .unwrap();
            assert!(snapshot.state.property_cache.borrow().contains_key(&(
                OwnerKind::Node,
                node,
                key
            )));
        } else {
            assert!(
                snapshot
                    .state
                    .overlay
                    .properties
                    .contains_key(&(OwnerKind::Node, node, key))
            );
        }
        let baseline = ledger.used.get();
        ledger.largest_request.set(0);
        let calls = Cell::new(0);
        let mut admission = |peak| -> StorageResult<()> {
            assert_eq!(peak, payload.len() as u64);
            assert_eq!(ledger.used.get(), baseline);
            assert_eq!(ledger.largest_request.get(), 0);
            calls.set(calls.get() + 1);
            Ok(())
        };
        let length = snapshot
            .with_named_property_admitted_checked(
                OwnerKind::Node,
                node,
                "payload",
                &|| Ok(()),
                &mut admission,
                |value, admission| {
                    assert_eq!(calls.get(), 0, "borrowed values need no decode admission");
                    let Some(PropertyValue::String(value)) = value else {
                        panic!("retained String")
                    };
                    admission(value.len() as u64)?;
                    let _memory = observer.reserve(value.len() as u64)?;
                    let converted = value.clone();
                    assert_eq!(converted, payload);
                    Ok::<_, StorageError>(converted.len())
                },
            )
            .unwrap();
        assert_eq!(length, payload.len());
        assert_eq!(calls.get(), 1);
        assert_eq!(ledger.used.get(), baseline);
        drop(snapshot);
        drop(state);
        assert_eq!(ledger.used.get(), 0);
    }
}

#[test]
fn property_admission_does_not_run_before_real_sql_auth_or_interrupt_errors() {
    for interrupted in [false, true] {
        let (connection, commit, node, _) = fixture(PropertyValue::String("checkpoint".into()));
        create_checkpoint(&connection, commit).unwrap();
        let (ledger, observer) = budget(512 * 1024);
        let state =
            Snapshot::resolve_query_state_observed(&connection, commit, observer, &|| Ok(()))
                .unwrap();
        let snapshot = Snapshot::from_resolved_state(&connection, &state);
        let baseline = ledger.used.get();
        ledger.largest_request.set(0);
        if interrupted {
            connection.progress_handler(1, Some(|| true)).unwrap();
        } else {
            connection
                .authorizer(Some(|context: AuthContext<'_>| {
                    if matches!(
                        context.action,
                        AuthAction::Read {
                            table_name: "_lithograph_cp_properties",
                            ..
                        }
                    ) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }))
                .unwrap();
        }
        let calls = Cell::new(0);
        let visited = Cell::new(false);
        let error = snapshot
            .with_named_property_admitted_checked(
                OwnerKind::Node,
                node,
                "payload",
                &|| Ok(()),
                &mut |_| {
                    calls.set(calls.get() + 1);
                    Ok::<_, StorageError>(())
                },
                |_, _| {
                    visited.set(true);
                    Ok::<_, StorageError>(())
                },
            )
            .unwrap_err();
        if interrupted {
            connection
                .progress_handler(0, None::<fn() -> bool>)
                .unwrap();
        } else {
            connection
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
        }
        let StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _)) = error else {
            panic!("the actual SQLite error must propagate")
        };
        let expected = if interrupted {
            rusqlite::ffi::SQLITE_INTERRUPT
        } else {
            rusqlite::ffi::SQLITE_AUTH
        };
        assert_eq!(code.extended_code, expected);
        assert_eq!(calls.get(), 0);
        assert!(!visited.get());
        assert_eq!(ledger.largest_request.get(), 0);
        assert_eq!(ledger.used.get(), baseline);
        drop(snapshot);
        drop(state);
        assert_eq!(ledger.used.get(), 0);
    }
}
