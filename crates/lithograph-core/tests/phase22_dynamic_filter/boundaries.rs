use super::*;
use lithograph_core::query::{QueryErrorKind, SnapshotSelector};

fn assert_no_dynamic_filter(actual: &Observed) {
    let profile = actual.profile.as_ref().expect("complete PROFILE execution");
    for operator in profile {
        if let Some(hash) = &operator.hash {
            assert!(
                hash.dynamic_filter.is_none(),
                "unsupported proof must not install a keyset: {}; {operator:?}",
                actual.plan
            );
        }
    }
}

fn without_probe(expected: &[Vec<Value>], seq: usize) -> Vec<Vec<Value>> {
    let id = Value::String(format!("n:{seq}"));
    expected
        .iter()
        .filter(|row| row[0] != id)
        .cloned()
        .collect()
}

fn temp_catalog(db: &Connection) -> Vec<(String, String)> {
    db.prepare("SELECT type,name FROM sqlite_temp_master ORDER BY type,name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn a_unique_range_without_existence_proof_and_null_keys_keep_the_original_bag() {
    let fixture = fixture(64);
    write(&fixture.db, "DROP CONSTRAINT df_probe_k_key");
    write(
        &fixture.db,
        "CREATE CONSTRAINT df_probe_k_unique FOR(p:DFProbe) REQUIRE p.k IS UNIQUE",
    );
    rebuild_and_checkpoint(&fixture.db, 64, PROBE_ROWS, "df_probe_k_unique");
    let actual = checked_join(&fixture, ExecutionOptions::default(), 3);
    assert_no_dynamic_filter(&actual);

    write(&fixture.db, "MATCH(p:DFProbe) WHERE p.seq=501 SET p.k=null");
    rebuild_and_checkpoint(&fixture.db, 64, PROBE_ROWS - 1, "df_probe_k_unique");
    let expected = without_probe(&fixture.expected, 501);
    let actual = checked_join_expected(&fixture, ExecutionOptions::default(), 3, &expected);
    assert_no_dynamic_filter(&actual);
}

#[test]
fn mixed_numeric_values_without_an_integer_proof_preserve_cross_numeric_equality() {
    let fixture = fixture(64);
    write(&fixture.db, "DROP CONSTRAINT df_probe_k_type");
    write(
        &fixture.db,
        "MATCH(p:DFProbe) WHERE p.seq=501 SET p.k=toFloat(p.k)",
    );
    rebuild_and_checkpoint(&fixture.db, 64, PROBE_ROWS, "df_probe_k_key");
    let id = Value::String("n:501".into());
    let mut expected = fixture.expected.clone();
    for row in &mut expected {
        if row[0] == id {
            row[2] = Value::Float(1.0);
        }
    }
    let actual = checked_join_expected(&fixture, ExecutionOptions::default(), 3, &expected);
    assert_no_dynamic_filter(&actual);
}

#[test]
fn nondefault_view_and_query_property_masks_do_not_borrow_the_default_view_keyset() {
    let fixture = fixture(64);
    let visible =
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["DFVisible"]}}"#)
            .unwrap();
    assert_no_dynamic_filter(&checked_join(&fixture, visible, 3));
    let excluded =
        ExecutionOptions::parse_text(r#"{"graphView":{"excludeAnyLabels":["DFProbe"]}}"#).unwrap();
    assert_no_dynamic_filter(&checked_join_expected(&fixture, excluded, 3, &[]));
    let denied =
        ExecutionOptions::parse_text(r#"{"readAccess":{"query":{"denyProperties":["k"]}}}"#)
            .unwrap();
    assert_no_dynamic_filter(&checked_join_expected(&fixture, denied, 3, &[]));
}

#[test]
fn historical_reads_use_their_own_keys_and_schema_after_the_head_changes() {
    let fixture = fixture(64);
    let old = storage::branch_head(&fixture.db, "main").unwrap();
    write(&fixture.db, "DROP CONSTRAINT df_probe_k_type");
    write(&fixture.db, "MATCH(p:DFProbe) WHERE p.seq=501 SET p.k=1001");
    rebuild_and_checkpoint(&fixture.db, 64, PROBE_ROWS, "df_probe_k_key");
    let current = storage::branch_head(&fixture.db, "main").unwrap();
    let expected = without_probe(&fixture.expected, 501);
    let actual = checked_join_expected(&fixture, ExecutionOptions::default(), 3, &expected);
    assert_no_dynamic_filter(&actual);
    let mut options = ExecutionOptions::default();
    options.snapshot = SnapshotSelector::Commit(old.to_hex());
    // Either a correctly pinned keyed read or its original fallback is valid.
    // The complete old bag must not see current values or current type proofs.
    checked_join(&fixture, options, 3);
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), current);
}

#[test]
fn a_staged_write_is_visible_to_the_following_match_without_reusing_the_old_pin() {
    let fixture = fixture(64);
    let initial = checked_join(&fixture, ExecutionOptions::default(), 3);
    assert_dynamic_filter(&initial, 64);
    let head = storage::branch_head(&fixture.db, "main").unwrap();
    let expected = without_probe(&fixture.expected, 501);
    let text = format!(
        "PROFILE MATCH(old:DFProbe) WHERE old.seq=501 SET old.k=1001 \
         WITH count(old) AS changed {JOIN}"
    );
    let actual = write(&fixture.db, &text);
    assert_eq!(bag(&actual.rows), bag(&expected));
    assert_no_dynamic_filter(&actual);
    assert_ne!(storage::branch_head(&fixture.db, "main").unwrap(), head);
    assert!(fixture.db.is_autocommit());
    checked_join_expected(&fixture, ExecutionOptions::default(), 3, &expected);
}

#[test]
fn zero_outer_and_query_only_do_not_bind_an_advisory_index() {
    let fixture = fixture(64);
    let before = temp_catalog(&fixture.db);
    let actual = read(
        &fixture.db,
        &format!("PROFILE UNWIND [] AS outer {JOIN}"),
        ExecutionOptions::default(),
        3,
    );
    assert!(actual.rows.is_empty());
    assert_no_dynamic_filter(&actual);
    for operator in actual.profile.as_ref().unwrap() {
        if let Some(hash) = &operator.hash {
            assert_eq!(
                hash.build_rows, 0,
                "zero outer must not build: {operator:?}"
            );
        }
    }
    assert_eq!(temp_catalog(&fixture.db), before);

    fixture.db.pragma_update(None, "query_only", true).unwrap();
    let before = temp_catalog(&fixture.db);
    let actual = checked_join(&fixture, ExecutionOptions::default(), 3);
    assert_no_dynamic_filter(&actual);
    assert_eq!(temp_catalog(&fixture.db), before);
    fixture.db.pragma_update(None, "query_only", false).unwrap();
}

#[test]
fn cancel_and_interrupt_after_crossing_the_frontier_release_state_for_a_fresh_read() {
    let fixture = fixture(64);
    let head = storage::branch_head(&fixture.db, "main").unwrap();
    for interrupted in [false, true] {
        let scope = ReadScope::new(&fixture.db);
        let prepared = prepare(
            &fixture.db,
            &format!("PROFILE {JOIN}"),
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .unwrap();
        assert!(prepared.physical.explain().contains("HashJoin"));
        let resources = prepared.resources.clone();
        let mut cursor = QueryCursor::new(prepared);
        let tail = Value::String("n:501".into());
        loop {
            let batch = cursor.next_batch(&fixture.db, 1).unwrap();
            let crossed = batch.rows.iter().any(|row| row[0] == tail);
            assert!(
                !batch.done,
                "the first keyed tail row precedes the complete bag"
            );
            if crossed {
                break;
            }
        }
        if interrupted {
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
        assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), head);
        let actual = checked_join(&fixture, ExecutionOptions::default(), 3);
        assert_dynamic_filter(&actual, 64);
    }
}
