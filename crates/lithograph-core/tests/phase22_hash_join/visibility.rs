use super::*;

#[test]
fn graph_view_query_mask_and_result_only_mask_keep_their_existing_boundaries() {
    let (db, build, probe) = integer_fixture();
    write(&db, "MATCH(a:Build) WHERE a.seq%64>=32 REMOVE a:Visible");
    write(&db, "MATCH(b:Probe) WHERE b.seq=1 REMOVE b:Visible");
    checkpoint(&db);
    let visible =
        ExecutionOptions::parse_text(r#"{"graphView":{"requireAllLabels":["Visible"]}}"#).unwrap();
    let pairs = integer_pairs()
        .into_iter()
        .filter(|&(a, b)| a % 64 < 32 && b != 1)
        .collect::<Vec<_>>();
    assert_eq!(pairs.len(), 8);
    assert_hash(&checked_join(
        &db,
        JOIN,
        visible,
        &expected_pairs(&build, &probe, &pairs),
    ));
    let hidden =
        ExecutionOptions::parse_text(r#"{"readAccess":{"query":{"denyProperties":["k"]}}}"#)
            .unwrap();
    let result = checked_join(&db, JOIN, hidden, &[]);
    assert_fallback(&result);
    let mask =
        ExecutionOptions::parse_text(r#"{"readAccess":{"result":{"denyProperties":["k"]}}}"#)
            .unwrap();
    let text = "MATCH(a:Build),(b:Probe) WHERE a.k=b.k RETURN a.k,b.k,a,b";
    let actual = read(&db, &format!("PROFILE {text}"), mask);
    assert_hash(&actual);
    assert_eq!(actual.rows.len(), 24);
    assert_bag(&actual.rows, &query(&db, text));
    for row in &actual.rows {
        for position in 0..2 {
            assert_ne!(row[position], Value::Null);
            let Value::Node(node) = &row[position + 2] else {
                panic!("complete Core Node value")
            };
            // Result-only masking belongs to final public JSON encoding.
            assert_eq!(node.properties.get("k"), Some(&row[position]));
            assert!(node.properties.contains_key("seq"));
        }
    }
}

#[test]
fn historical_schema_proof_uses_the_pinned_commit_not_the_current_head() {
    let (db, build, probe) = integer_fixture();
    let old = storage::branch_head(&db, "main").unwrap();
    write(&db, "MATCH(a:Build) WHERE a.seq=5 SET a.k=42");
    let pairs = integer_pairs()
        .into_iter()
        .filter(|&(a, _)| a != 5)
        .collect::<Vec<_>>();
    assert_eq!(pairs.len(), 22);
    checked_join(
        &db,
        JOIN,
        ExecutionOptions::default(),
        &expected_pairs(&build, &probe, &pairs),
    );
    write(&db, "DROP CONSTRAINT Build_k");
    write(&db, "DROP CONSTRAINT Probe_k");
    write(&db, "DROP CONSTRAINT Build_seq");
    write(&db, "DROP CONSTRAINT Probe_seq");
    write(&db, "MATCH(a:Build) SET a.k=toString(a.seq)");
    write(&db, "MATCH(b:Probe) SET b.k=toString(b.seq)");
    constraints(&db, "STRING", "STRING");
    let current = storage::branch_head(&db, "main").unwrap();
    let mut historical = ExecutionOptions::default();
    historical.snapshot = SnapshotSelector::Commit(old.to_hex());
    let result = checked_join(
        &db,
        JOIN,
        historical,
        &expected_pairs(&build, &probe, &integer_pairs()),
    );
    assert_hash(&result);
    assert_eq!(result.commit, Some(format!("commit/{}", old.to_hex())));
    assert_eq!(storage::branch_head(&db, "main").unwrap(), current);
    let result = read(&db, &format!("PROFILE {JOIN}"), ExecutionOptions::default());
    assert_hash(&result);
    let expected = (0..SIZE)
        .map(|seq| {
            vec![
                build[seq].id.clone(),
                probe[seq].id.clone(),
                Value::String(seq.to_string()),
                Value::String(seq.to_string()),
            ]
        })
        .collect::<Vec<_>>();
    assert_bag(&result.rows, &expected);
}

#[test]
fn staged_mutation_revision_and_failed_hash_join_write_keep_visibility_and_rollback() {
    let (db, build, probe) = integer_fixture();
    let original = storage::branch_head(&db, "main").unwrap();
    let mutation = JOIN.replace(" RETURN ", " SET a.touched=1 RETURN ");
    let written = write(&db, &format!("PROFILE {mutation}"));
    assert_hash(&written);
    assert_bag(
        &written.rows,
        &expected_pairs(&build, &probe, &integer_pairs()),
    );
    assert_ne!(storage::branch_head(&db, "main").unwrap(), original);
    let before = storage::branch_head(&db, "main").unwrap();
    let bad = "MATCH(a:Build),(b:Probe) WHERE a.k=b.k AND b.seq=0 SET a.k=42 RETURN 1/0";
    let error = consume(&db, bad, BTreeMap::new(), ExecutionOptions::default()).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Type);
    assert!(db.is_autocommit());
    assert_eq!(storage::branch_head(&db, "main").unwrap(), before);
    assert_bag(
        &query(&db, JOIN),
        &expected_pairs(&build, &probe, &integer_pairs()),
    );
    let text = "MATCH(a:Build),(b:Probe) WHERE a.k=b.k AND b.seq=0 SET a.k=42 \
        WITH count(a) AS changed MATCH(c:Build),(d:Probe) WHERE c.k=d.k \
        RETURN changed,elementId(c),elementId(d),c.k,d.k";
    let expected = expected_pairs(
        &build,
        &probe,
        &integer_pairs()
            .into_iter()
            .filter(|&(a, b)| a % 32 == 13 && b == 2)
            .collect::<Vec<_>>(),
    )
    .into_iter()
    .map(|mut row| {
        row.insert(0, Value::Integer(8));
        row
    })
    .collect::<Vec<_>>();
    let result = write(&db, &format!("PROFILE {text}"));
    assert_eq!(result.rows.len(), 8);
    assert_bag(&result.rows, &expected);
    assert_eq!(result.counters.properties_set, 8);
    assert_ne!(storage::branch_head(&db, "main").unwrap(), before);
    assert!(db.is_autocommit());
    let pairs = integer_pairs()
        .into_iter()
        .filter(|&(a, _)| a % 32 == 13)
        .collect::<Vec<_>>();
    assert_hash(&checked_join(
        &db,
        JOIN,
        ExecutionOptions::default(),
        &expected_pairs(&build, &probe, &pairs),
    ));
    let optional = "MATCH(a:Probe) WHERE a.seq=3 OPTIONAL MATCH(b:Build) WHERE a.k=b.k RETURN elementId(a),elementId(b)";
    assert_eq!(
        query(&db, optional),
        vec![vec![probe[3].id.clone(), Value::Null]]
    );
}
