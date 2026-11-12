use super::*;
use crate::query::{ExecutionOptions, QueryCursor, QueryResources, prepare};

fn run(db: &Connection, text: &str) {
    let prepared = prepare(db, text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let mut cursor = QueryCursor::new(prepared);
    while !cursor.next_batch(db, 32).unwrap().done {}
    cursor.complete(db).unwrap();
}

fn database() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    db
}

fn fixture(count: usize, expression: &str) -> Connection {
    let db = database();
    if count > 0 {
        run(
            &db,
            &format!(
                "UNWIND range(0,{}) AS i CREATE (:Common {{v:{expression}}}) FINISH",
                count - 1
            ),
        );
    }
    run(
        &db,
        "CREATE CONSTRAINT value_type FOR(n:Common) REQUIRE n.v IS :: INTEGER",
    );
    run(&db, "CREATE RANGE INDEX dist_idx FOR(n:Common) ON(n.v)");
    db
}

fn estimate_at(snapshot: &Snapshot<'_>, name: &str) -> Option<IndexDistribution> {
    let schema = snapshot.schema_state().unwrap();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    // Production callers pay this lease and the global planning grant before
    // entering the sampler; the value-independent API adds neither of its own.
    let memory = control.reserve(16 * 1024).unwrap();
    let result = estimate_index_distribution(
        snapshot,
        &schema.indexes[name],
        &PropertyRules::default(),
        &control,
    )
    .unwrap();
    assert_eq!(resources.used(), 16 * 1024);
    drop(memory);
    assert_eq!(resources.used(), 0);
    result
}

fn estimate(db: &Connection) -> Option<IndexDistribution> {
    let snapshot = Snapshot::resolve(db, storage::branch_head(db, "main").unwrap()).unwrap();
    estimate_at(&snapshot, "dist_idx")
}

#[test]
fn uniform_and_skew_distribution_are_not_bound_to_an_equality_probe_value() {
    let uniform = fixture(384, "i+1000");
    let counts = estimate(&uniform).unwrap();
    assert_eq!(counts.indexed_owners, 384);
    assert_eq!(counts.ndistinct.expected, 384);
    assert_eq!(counts.ndistinct.lower, 32);
    assert_eq!(counts.ndistinct.upper, 384);
    assert_eq!(counts.max_frequency.expected, 1);
    assert_eq!(counts.max_observed_frequency, 1);
    assert_eq!(counts.sampled_owners, 32);
    let skew = fixture(384, "CASE WHEN i<368 THEN 1000 ELSE i+1000 END");
    let counts = estimate(&skew).unwrap();
    assert_eq!(counts.ndistinct.expected, 24);
    assert!(counts.ndistinct.lower <= 17 && counts.ndistinct.upper >= 17);
    assert_eq!(counts.max_frequency.expected, 372);
    assert_eq!(counts.max_observed_frequency, 31);
    assert!(counts.max_frequency.lower <= 368 && counts.max_frequency.upper >= 368);
}

#[test]
fn small_empty_and_repeated_domains_preserve_conservative_ranges() {
    let empty = fixture(0, "1");
    let counts = estimate(&empty).unwrap();
    assert_eq!(counts.indexed_owners, 0);
    assert_eq!(counts.ndistinct, CardinalityEstimate::exact(0));
    assert_eq!(counts.max_frequency, CardinalityEstimate::exact(0));
    let singleton = fixture(1, "1000");
    let counts = estimate(&singleton).unwrap();
    assert_eq!(counts.ndistinct, CardinalityEstimate::exact(1));
    assert_eq!(counts.max_frequency, CardinalityEstimate::exact(1));
    let repeated = fixture(256, "i%64");
    let counts = estimate(&repeated).unwrap();
    assert_eq!(counts.indexed_owners, 256);
    assert!(counts.ndistinct.lower <= 64 && counts.ndistinct.upper >= 64);
    assert!(counts.ndistinct.expected > 1);
    assert!(counts.max_frequency.lower <= 4 && counts.max_frequency.upper >= 4);
}

#[test]
fn generation_identity_and_historical_state_are_exactly_the_selected_generation() {
    let db = fixture(64, "i+1000");
    let head = storage::branch_head(&db, "main").unwrap();
    let before = Snapshot::resolve(&db, head).unwrap();
    let counts = estimate_at(&before, "dist_idx").unwrap();
    let (id, anchor, definition): (i64, Vec<u8>, Vec<u8>) = db
        .query_row(
            "SELECT generation_id,anchor_commit,definition_hash FROM _lithograph_index_generations",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(counts.generation_id, id);
    assert_eq!(
        counts.generation_anchor,
        HashId::from_slice(&anchor).unwrap()
    );
    assert_eq!(
        counts.definition_hash,
        HashId::from_slice(&definition).unwrap()
    );
    drop(before);
    run(
        &db,
        "MATCH(n:Common) WHERE elementId(n)='n:1' SET n.v=9999 FINISH",
    );
    assert!(
        estimate(&db).is_none(),
        "ancestor NDV is not the changed Snapshot NDV"
    );
    let historical = Snapshot::resolve(&db, head).unwrap();
    assert_eq!(estimate_at(&historical, "dist_idx"), Some(counts));
}

#[test]
fn staged_overlay_is_unknown_without_decoding_unproved_changed_values() {
    let db = fixture(64, "i+1000");
    let mut snapshot = Snapshot::resolve(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    let mut layer = storage::LayerBuilder::default();
    let key = storage::find_property_key(&db, "v").unwrap().unwrap();
    layer
        .set_property(
            OwnerKind::Node,
            1,
            key,
            storage::PropertyValue::Integer(9999),
        )
        .unwrap();
    snapshot.apply_layer_checked(&layer, &|| Ok(())).unwrap();
    assert!(estimate_at(&snapshot, "dist_idx").is_none());
}

#[test]
fn malformed_counts_definition_keys_and_missing_generation_are_unknown() {
    let db = fixture(384, "i+1000");
    for count in [0, 1, 16, 31, 32] {
        db.execute(
            "UPDATE _lithograph_index_generations SET indexed_entities=?1,entry_count=?1",
            [count],
        )
        .unwrap();
        assert!(
            estimate(&db).is_none(),
            "observed owners contradict count {count}"
        );
    }
    db.execute(
        "UPDATE _lithograph_index_generations SET indexed_entities=384,entry_count=384",
        [],
    )
    .unwrap();
    db.execute(
        "UPDATE _lithograph_index_generations SET entry_count='broken'",
        [],
    )
    .unwrap();
    assert!(estimate(&db).is_none());
    db.execute(
        "UPDATE _lithograph_index_generations SET entry_count=384",
        [],
    )
    .unwrap();
    db.execute(
        "UPDATE _lithograph_index_entries SET equality_blob=zeroblob(1024)",
        [],
    )
    .unwrap();
    assert!(estimate(&db).is_none(), "no bounded usable keys is unknown");
    db.execute(
        "UPDATE _lithograph_index_generations SET definition_blob='broken'",
        [],
    )
    .unwrap();
    assert!(estimate(&db).is_none());
    db.execute("DELETE FROM _lithograph_index_entries", [])
        .unwrap();
    db.execute("DELETE FROM _lithograph_index_generations", [])
        .unwrap();
    assert!(estimate(&db).is_none());
}

#[test]
fn relationship_distribution_and_schema_only_ancestry_do_not_create_temp_or_write() {
    let db = database();
    run(
        &db,
        "UNWIND range(0,63) AS i CREATE ()-[:T {v:i+1000}]->() FINISH",
    );
    run(
        &db,
        "CREATE CONSTRAINT rel_type FOR()-[r:T]-() REQUIRE r.v IS :: INTEGER",
    );
    run(&db, "CREATE RANGE INDEX rel_dist FOR()-[r:T]-() ON(r.v)");
    run(
        &db,
        "CREATE CONSTRAINT unused FOR(n:Elsewhere) REQUIRE n.x IS :: INTEGER",
    );
    let snapshot = Snapshot::resolve(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    let before = db.total_changes();
    let catalog: i64 = db
        .query_row("PRAGMA temp.schema_version", [], |row| row.get(0))
        .unwrap();
    let counts = estimate_at(&snapshot, "rel_dist").unwrap();
    assert_eq!(counts.indexed_owners, 64);
    assert_eq!(counts.ndistinct.expected, 64);
    assert_eq!(counts.max_observed_frequency, 1);
    assert_eq!(db.total_changes(), before);
    assert_eq!(
        db.query_row("PRAGMA temp.schema_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        catalog
    );
}

#[test]
fn permissions_and_cancellation_precede_optional_metadata_reads_and_io_errors_propagate() {
    let db = fixture(64, "i+1000");
    let snapshot = Snapshot::resolve(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    let schema = snapshot.schema_state().unwrap();
    let options =
        ExecutionOptions::parse_text(r#"{"readAccess":{"query":{"denyProperties":["v"]}}}"#)
            .unwrap();
    let resources = QueryResources::default();
    let allowed = ExecutionControl::new(&resources, &|| false);
    let cancelled = ExecutionControl::new(&resources, &|| true);
    assert_eq!(
        estimate_index_distribution(
            &snapshot,
            &schema.indexes["dist_idx"],
            &PropertyRules::default(),
            &cancelled
        )
        .unwrap_err()
        .kind,
        crate::query::QueryErrorKind::Interrupted
    );
    db.execute("DROP TABLE _lithograph_index_generations", [])
        .unwrap();
    assert!(
        estimate_index_distribution(
            &snapshot,
            &schema.indexes["dist_idx"],
            &options.access.read.query,
            &allowed
        )
        .unwrap()
        .is_none()
    );
    assert!(
        estimate_index_distribution(
            &snapshot,
            &schema.indexes["dist_idx"],
            &PropertyRules::default(),
            &allowed
        )
        .is_err()
    );
}

#[test]
fn mismatched_definition_and_over_cap_ancestry_do_not_use_another_index() {
    let db = fixture(64, "i+1000");
    let snapshot = Snapshot::resolve(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    let schema = snapshot.schema_state().unwrap();
    let mut other = schema.indexes["dist_idx"].clone();
    other.name = "different_definition".to_owned();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    assert!(
        estimate_index_distribution(&snapshot, &other, &PropertyRules::default(), &control)
            .unwrap()
            .is_none()
    );
    drop(snapshot);
    for ordinal in 0..65 {
        storage::commit_layer(
            &db,
            "main",
            storage::branch_head(&db, "main").unwrap(),
            None,
            &storage::LayerBuilder::default(),
            &storage::CommitMetadata {
                author: None,
                message: None,
                committed_at: ordinal + 100_000,
            },
        )
        .unwrap();
    }
    assert!(estimate(&db).is_none());
}

#[test]
fn shared_sampler_keeps_unprobed_bounds_distinct_from_guarded_unavailable_keys() {
    let db = fixture(2, "i+1000");
    db.execute(
        "UPDATE _lithograph_index_entries SET equality_blob=zeroblob(1024)",
        [],
    )
    .unwrap();
    let snapshot = Snapshot::resolve(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    let schema = snapshot.schema_state().unwrap();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let _memory = control.reserve(16 * 1024).unwrap();
    let generation = statistics::find_generation(&snapshot, &schema.indexes["dist_idx"], &control)
        .unwrap()
        .unwrap();
    let mut visits = [0; 3];
    let sample = sample_generation(
        &snapshot,
        &generation,
        OwnerKind::Node,
        &BTreeSet::new(),
        &control,
        |_, key| {
            visits[match key {
                SampleKey::Unprobed => 0,
                SampleKey::Unavailable => 1,
                SampleKey::Bytes(_) => 2,
            }] += 1;
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(visits, [2, MAX_SAMPLES, 0]);
    assert_eq!(sample.observed_owners, 2);
    assert_eq!(sample.sampled_owners, 0);
}
