use super::*;
use crate::cypher::Value;
use crate::query::{ExecutionOptions, QueryCursor, QueryResources, QuerySummary, prepare};

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

fn index(db: &Connection) {
    run(
        db,
        "CREATE CONSTRAINT t FOR(n:Common) REQUIRE n.v IS :: INTEGER",
    );
    run(db, "CREATE RANGE INDEX values_range FOR(n:Common) ON(n.v)");
}

fn fixture(skew: bool) -> Connection {
    let db = database();
    let value = if skew {
        "CASE WHEN i<368 THEN 0 ELSE i END"
    } else {
        "i"
    };
    run(
        &db,
        &format!("UNWIND range(0,383) AS i CREATE (:Common {{k:i,v:{value}}}) FINISH"),
    );
    index(&db);
    db
}

fn counts(db: &Connection, owners: i64) {
    db.execute(
        "UPDATE _lithograph_index_generations SET indexed_entities=?1,entry_count=?1",
        [owners],
    )
    .unwrap();
}

#[test]
fn contradictory_numeric_counts_do_not_override_observed_frequency_or_owners() {
    let db = fixture(true);
    for owners in [0, 1, 16, 31, 32, 127, 128, 129] {
        counts(&db, owners);
        assert!(estimate(&db, 0).is_none(), "hot count {owners}");
    }
    let uniform = fixture(false);
    for owners in [0, 1, 16, 31, 32] {
        counts(&uniform, owners);
        assert!(estimate(&uniform, 383).is_none(), "cold count {owners}");
        assert!(estimate(&uniform, 9999).is_none(), "absent count {owners}");
    }
}

#[test]
fn contradictory_prefix_union_includes_bounds_and_deduplicates_sampled_owners() {
    let db = fixture(true);
    // Ordered frequency sees IDs1..129. The 32 strata include21 IDs above
    // that prefix; the last-owner point probe adds ID384 independently.
    counts(&db, 150);
    assert!(estimate(&db, 0).is_none());
    counts(&db, 151);
    let possible = estimate(&db, 0).unwrap();
    assert_eq!((possible.rows.lower, possible.rows.upper), (128, 151));
}

#[test]
fn contradictory_nonmatching_key_inside_the_frequency_prefix_is_disjoint() {
    let db = database();
    run(
        &db,
        "UNWIND range(0,383) AS i CREATE (:Common {v:CASE WHEN i=11 THEN 1 ELSE 0 END}) FINISH",
    );
    index(&db);
    // ID12 is sampled, below last-prefix ID130, and has a different key.
    counts(&db, 151);
    assert!(estimate(&db, 0).is_none());
    counts(&db, 152);
    assert!(estimate(&db, 0).is_some());
}

#[test]
fn ordered_frequency_probe_uses_the_covering_equality_index_without_sort() {
    let db = fixture(true);
    let generation = db
        .query_row(
            "SELECT generation_id FROM _lithograph_index_generations",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let key = equality_key(&storage::PropertyValue::Integer(0), &control)
        .unwrap()
        .unwrap();
    let mut statement = db
        .prepare(&format!("EXPLAIN QUERY PLAN {FREQUENCY_SQL}"))
        .unwrap();
    let details = statement
        .query_map(
            rusqlite::params![
                generation,
                OwnerKind::Node as i64,
                key,
                (FREQUENCY_CAP + MAX_CHANGED + 1) as i64
            ],
            |row| row.get::<_, String>(3),
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        details
            .iter()
            .any(|detail| detail.contains("_lithograph_index_entries_equality")),
        "{details:?}"
    );
    assert!(
        !details.iter().any(|detail| detail.contains("TEMP B-TREE")),
        "{details:?}"
    );
}

#[test]
fn contradictory_removed_owner_counts_are_unknown_even_without_retained_samples() {
    let db = database();
    run(&db, "CREATE (:Common {v:0}),(:Common {v:0}) FINISH");
    index(&db);
    run(&db, "MATCH(n:Common) SET n.v=1 FINISH");
    assert_eq!(
        estimate(&db, 9999).unwrap().rows,
        CardinalityEstimate::exact(0)
    );
    counts(&db, 1);
    assert!(estimate(&db, 9999).is_none());
}

#[test]
fn contradictory_zero_counts_cannot_hide_present_entries_with_unusable_payloads() {
    let db = fixture(false);
    counts(&db, 0);
    db.execute(
        "UPDATE _lithograph_index_entries SET equality_blob=NULL",
        [],
    )
    .unwrap();
    assert!(estimate(&db, 9999).is_none());
    db.execute(
        "UPDATE _lithograph_index_entries SET owner_id='broken' WHERE owner_id=384",
        [],
    )
    .unwrap();
    assert!(estimate(&db, 9999).is_none());
}

fn rare_profile(db: &Connection) -> (Vec<Vec<Value>>, QuerySummary) {
    let options =
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Rare"]}}"#).unwrap();
    let prepared = prepare(
        db,
        "PROFILE MATCH(n:Common) WHERE n.v=$x RETURN n.k ORDER BY n.k",
        BTreeMap::from([("x".to_owned(), Value::Integer(0))]),
        options,
    )
    .unwrap();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(db, 32).unwrap();
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(db).unwrap();
            return (rows, batch.summary.unwrap());
        }
    }
}

#[test]
fn contradictory_counts_preserve_the_real_rare_label_access_and_complete_rows() {
    let db = database();
    run(
        &db,
        "UNWIND range(0,9999) AS i CREATE (:Common {k:i,v:0}) FINISH",
    );
    run(&db, "MATCH(n:Common) WHERE n.k<512 SET n:Rare FINISH");
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    index(&db);
    let head = storage::branch_head(&db, "main").unwrap();
    let (expected, before) = rare_profile(&db);
    assert_eq!(
        expected,
        (0..512)
            .map(|key| vec![Value::Integer(key)])
            .collect::<Vec<_>>()
    );
    counts(&db, 0);
    let (actual, after) = rare_profile(&db);
    assert_eq!(actual, expected);
    assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
    for summary in [&before, &after] {
        let operators = summary.metrics.operator_profile().unwrap();
        assert!(
            operators
                .iter()
                .any(|op| op.operator == "LabelIndexScan" && op.db_hits > 0),
            "{operators:?}"
        );
        assert!(
            !operators.iter().any(|op| op.operator == "IndexSeek"),
            "{operators:?}"
        );
    }
    assert!(after.metrics.db_hits <= before.metrics.db_hits);
}

fn estimate(db: &Connection, value: i64) -> Option<IndexEstimate> {
    let snapshot = Snapshot::resolve(db, storage::branch_head(db, "main").unwrap()).unwrap();
    let schema = snapshot.schema_state().unwrap();
    let index = schema.indexes.get("values_range").unwrap();
    let seek = super::super::super::super::StandardIndexSeek {
        index_name: index.name.clone(),
        kind: index.kind,
        predicates: vec![(
            0,
            super::super::super::super::StandardIndexPredicate::Equal(Value::Integer(value)),
        )],
    };
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let result =
        estimate_index_access(&snapshot, index, &seek, &PropertyRules::default(), &control)
            .unwrap();
    assert_eq!(resources.used(), 0, "temporary estimate memory leaked");
    result
}

#[test]
fn ready_samples_quantify_uniform_skew_mcv_and_exact_cold_frequency() {
    let db = fixture(false);
    let uniform = estimate(&db, 383).unwrap();
    assert_eq!(uniform.rows, CardinalityEstimate::exact(1));
    assert_eq!(uniform.indexed_owners, 384);
    assert_eq!(uniform.ndistinct, Some(384));
    assert_eq!(uniform.sampled_owners, 32);
    assert_eq!(uniform.mcv_rows, None);
    let skew = fixture(true);
    let hot = estimate(&skew, 0).unwrap();
    assert_eq!(hot.rows.lower, 128);
    assert_eq!(hot.rows.upper, 384);
    // Actual: 368 hot owners and 17 distinct values. The fixed 32 strata
    // produce 372 hot owners (1.09% error) and NDV 24, retaining the interval.
    assert_eq!(hot.rows.expected, 372);
    assert_eq!(hot.mcv_rows, Some(372));
    assert_eq!(hot.ndistinct, Some(24));
    assert_eq!(
        estimate(&skew, 383).unwrap().rows,
        CardinalityEstimate::exact(1)
    );
    assert_eq!(
        estimate(&skew, 9999).unwrap().rows,
        CardinalityEstimate::exact(0)
    );
}

#[test]
fn bounded_ancestor_overlay_and_staged_changes_adjust_frequency() {
    let db = fixture(true);
    run(
        &db,
        "MATCH(n:Common) WHERE elementId(n)='n:1' SET n.v=500 FINISH",
    );
    assert_eq!(
        estimate(&db, 500).unwrap().rows,
        CardinalityEstimate::exact(1)
    );
    assert_eq!(estimate(&db, 0).unwrap().indexed_owners, 384);
    let commit = storage::branch_head(&db, "main").unwrap();
    let mut snapshot = Snapshot::resolve(&db, commit).unwrap();
    let schema = snapshot.schema_state().unwrap();
    let mut layer = storage::LayerBuilder::default();
    let key = storage::find_property_key(&db, "v").unwrap().unwrap();
    layer
        .set_property(
            OwnerKind::Node,
            2,
            key,
            storage::PropertyValue::Integer(501),
        )
        .unwrap();
    snapshot.apply_layer_checked(&layer, &|| Ok(())).unwrap();
    let index = schema.indexes.get("values_range").unwrap();
    let seek = super::super::super::super::StandardIndexSeek {
        index_name: index.name.clone(),
        kind: index.kind,
        predicates: vec![(
            0,
            super::super::super::super::StandardIndexPredicate::Equal(Value::Integer(501)),
        )],
    };
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let sampled =
        estimate_index_access(&snapshot, index, &seek, &PropertyRules::default(), &control)
            .unwrap()
            .unwrap();
    assert_eq!(sampled.rows, CardinalityEstimate::exact(1));
    assert_eq!(
        estimate(&db, 501).unwrap().rows,
        CardinalityEstimate::exact(0)
    );
}

#[test]
fn malformed_or_missing_derived_counts_and_large_overlay_are_unknown() {
    let db = fixture(true);
    for column in ["indexed_entities", "entry_count"] {
        db.execute(
            &format!("UPDATE _lithograph_index_generations SET {column}='broken'"),
            [],
        )
        .unwrap();
        assert!(estimate(&db, 383).is_none());
        db.execute(
            &format!("UPDATE _lithograph_index_generations SET {column}=384"),
            [],
        )
        .unwrap();
    }
    run(&db, "MATCH(n:Common) SET n.v=42 FINISH");
    assert!(estimate(&db, 42).is_none());
    db.execute("DELETE FROM _lithograph_index_entries", [])
        .unwrap();
    db.execute("DELETE FROM _lithograph_index_generations", [])
        .unwrap();
    assert!(estimate(&db, 383).is_none());
}

#[test]
fn malformed_derived_owner_identity_is_unknown_instead_of_conversion_error() {
    let db = fixture(false);
    db.execute(
        "UPDATE _lithograph_index_entries SET owner_id='broken' WHERE owner_id=384",
        [],
    )
    .unwrap();
    assert!(estimate(&db, 383).is_none());
    assert!(estimate(&db, 1).is_none());
}

#[test]
fn empty_ready_generation_is_advice_and_still_executes_the_match() {
    let db = fixture(false);
    run(&db, "MATCH(n:Common) DETACH DELETE n FINISH");
    run(&db, "DROP INDEX values_range");
    run(&db, "CREATE RANGE INDEX values_range FOR(n:Common) ON(n.v)");
    let empty = estimate(&db, 1).unwrap();
    assert_eq!(empty.rows, CardinalityEstimate::exact(0));
    assert_eq!(empty.indexed_owners, 0);
    assert_eq!(empty.ndistinct, Some(0));
    assert_eq!((empty.mcv_rows, empty.sampled_owners), (None, 0));
    run(&db, "MATCH(n:Common) WHERE n.v=1 RETURN n.v");
}
