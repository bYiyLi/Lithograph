use super::*;

#[test]
fn repartition_routes_both_sides_and_new_probe_run_preserves_build() {
    let resources = fixture_resources(&value_row(0, 0));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut store = GracePartitions::create(resources.clone(), &control).unwrap();
    for id in 0..600 {
        let mut digest = [0; 32];
        digest[0] = (id % 64) as u8;
        append(
            &mut store,
            HashSide::Build,
            digest,
            value_row(id, 0),
            &control,
        );
        append(
            &mut store,
            HashSide::Probe,
            digest,
            value_row(id, 0),
            &control,
        );
    }
    let RepartitionOutcome::Children(children) =
        store.repartition(PartitionId::ROOT, &control).unwrap()
    else {
        panic!("nonempty root must partition");
    };
    assert!(store.stats(PartitionId::ROOT).is_err());
    for child in children {
        assert_eq!(store.stats(child).unwrap().rows, [75, 75]);
    }
    let mut digest = [0; 32];
    digest[0] = 17;
    let previous = store.route_digest(&digest).unwrap();
    store.repartition(previous, &control).unwrap();
    assert_ne!(store.route_digest(&digest).unwrap(), previous);
    store.retire_probe_run(&control).unwrap();
    let mut rows = [0; 2];
    let mut leaf = store.next_leaf(None);
    while let Some(id) = leaf {
        let stats = store.stats(id).unwrap();
        rows[0] += stats.rows[0];
        rows[1] += stats.rows[1];
        leaf = store.next_leaf(Some(id));
    }
    assert_eq!(rows, [600, 0]);
    let seq = append(
        &mut store,
        HashSide::Probe,
        digest,
        value_row(999, 0),
        &control,
    );
    assert_eq!(seq, 1200);
    let id = store.route_digest(&digest).unwrap();
    let mut page = store
        .read_batch_reserved(id, HashSide::Probe, -1, limit(256, 4096), &control)
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page.next().unwrap().row, value_row(999, 0));
    drop(page);
    store.retire_probe_run(&control).unwrap();
    let counters = store.finish().unwrap();
    assert_eq!(counters.retired_probe_rows, 601);
    assert_eq!(counters.written_rows, [600, 601]);
    assert_eq!(counters.repartition_rows, 1350);
    assert_eq!(counters.sqlite_fullscan_steps, 0);
    assert_eq!(counters.sqlite_sorts, 0);
    assert_eq!(resources.used(), 0);
}

#[test]
fn colliding_digest_preserves_duplicate_rows_and_finite_hot_leaf() {
    let resources = fixture_resources(&value_row(0, 0));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut store = GracePartitions::create(resources.clone(), &control).unwrap();
    for id in [1, 2, 2] {
        append(
            &mut store,
            HashSide::Build,
            [7; 32],
            value_row(id, 0),
            &control,
        );
    }
    let mut leaf = PartitionId::ROOT;
    for depth in 1..=MAX_DEPTH {
        assert!(matches!(
            store.repartition(leaf, &control).unwrap(),
            RepartitionOutcome::Children(_)
        ));
        leaf = store.route_digest(&[7; 32]).unwrap();
        assert_eq!(store.stats(leaf).unwrap().depth, depth);
    }
    assert_eq!(
        store.repartition(leaf, &control).unwrap(),
        RepartitionOutcome::Skew(leaf)
    );
    let page = store
        .read_batch_reserved(leaf, HashSide::Build, -1, limit(256, 64 * 1024), &control)
        .unwrap();
    for (item, expected) in page.zip([1, 2, 2]) {
        assert_eq!(item.row, value_row(expected, 0));
    }
    let counters = store.finish().unwrap();
    assert_eq!(counters.max_depth, 3);
    assert_eq!(counters.partitions, 25);
    assert_eq!(counters.repartition_rows, 9);
    assert_eq!(resources.used(), 0);
}

#[test]
fn full_directory_is_bounded_and_accounts_internal_nodes() {
    let resources = QueryResources::new(Some(128 * 1024));
    let mut directory = Directory::new(&resources).unwrap();
    let mut current = vec![PartitionId::ROOT];
    for _ in 0..MAX_DEPTH {
        current = current
            .into_iter()
            .flat_map(|id| directory.split(id).unwrap())
            .collect();
    }
    assert_eq!(current.len(), 512);
    assert!(resources.used() > allocation_bytes::<PartitionStats>(512).unwrap());
    assert_eq!(resources.used(), Directory::resident_bound());
    assert_eq!(resident_bound(), BASE_MEMORY_BYTES + resources.used());
    assert!(directory.split(current[0]).is_err());
    for ordinal in 0..512_u16 {
        let mut digest = [0; 32];
        digest[..2].copy_from_slice(&ordinal.to_le_bytes());
        assert!(current.contains(&directory.route(&digest).unwrap()));
    }
    drop(directory);
    assert_eq!(resources.used(), 0);
}
