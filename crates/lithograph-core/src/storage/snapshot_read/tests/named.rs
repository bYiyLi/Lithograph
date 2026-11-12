use super::*;

#[test]
fn named_lookup_borrows_prefetched_value_without_another_checkpoint_decode() {
    let (connection, commit, node, key) = fixture(PropertyValue::String("x".repeat(128 * 1024)));
    create_checkpoint(&connection, commit).unwrap();
    let (budget, observer) = budget(2 * 1024 * 1024);
    let state =
        Snapshot::resolve_query_state_observed(&connection, commit, observer, &|| Ok(())).unwrap();
    let snapshot = Snapshot::from_resolved_state(&connection, &state);
    snapshot
        .prefetch_properties(OwnerKind::Node, &[node], &[key])
        .unwrap();
    let retained = budget.used.get();
    snapshot
        .with_named_property_checked(OwnerKind::Node, node, "payload", &|| Ok(()), |value| {
            assert!(
                matches!(value, Some(PropertyValue::String(value)) if value.len() == 128 * 1024)
            );
            assert_eq!(
                budget.used.get(),
                retained,
                "reuse the accounted cache value"
            );
            Ok::<_, StorageError>(())
        })
        .unwrap();
    assert_eq!(budget.used.get(), retained);
    drop(snapshot);
    drop(state);
    assert_eq!(budget.used.get(), 0);
}
