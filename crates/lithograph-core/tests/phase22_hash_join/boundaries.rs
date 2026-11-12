use super::*;

fn error(db: &Connection, text: &str) -> QueryError {
    let _scope = ReadScope::new(db);
    let prepared = prepare(db, text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    assert!(!prepared.physical.explain().contains("HashJoin"));
    drop(prepared);
    consume(db, text, BTreeMap::new(), ExecutionOptions::default()).unwrap_err()
}

#[test]
fn unsupported_equality_and_throwing_residuals_preserve_errors_and_zero_outer() {
    let db = empty_db();
    write(
        &db,
        "CREATE (:Build),(:Probe {k:1}),(:Present),(:Number {k:1}),(:Text {k:'one'})",
    );
    constraints(&db, "INTEGER", "INTEGER");
    for text in [
        "MATCH(a:Build),(b:Probe) WHERE a.k=b.k AND 1/0=0 RETURN 1",
        "MATCH(a:Number),(b:Text) WHERE a.k=b.k RETURN 1",
        "MATCH(a:Present),(n:Number),(s:Text) WHERE n.k=s.k RETURN 1",
    ] {
        let actual = error(&db, text);
        let original = error(&db, &canonical(text));
        assert_eq!(actual.kind, QueryErrorKind::Type);
        assert_eq!(
            (actual.kind, actual.message),
            (original.kind, original.message)
        );
    }
    for text in [
        "MATCH(a:Build),(b:Probe) WHERE false AND a.k=b.k AND 1/0=0 RETURN 1",
        "MATCH(a:Missing),(n:Number),(s:Text) WHERE n.k=s.k RETURN 1",
        "UNWIND [] AS outer MATCH(a:Build),(b:Probe) WHERE a.k=b.k AND 1/0=0 RETURN 1",
    ] {
        let result = checked_join(&db, text, ExecutionOptions::default(), &[]);
        assert_fallback(&result);
    }
    assert!(db.is_autocommit());

    // A large safe equijoin selects Hash in the positive test. These predicates
    // must reject it because of proof, rather than merely choosing a cheap loop.
    let (db, build, probe) = integer_fixture();
    let head = storage::branch_head(&db, "main").unwrap();
    let throwing = JOIN.replace("a.k=b.k", "a.k=b.k AND 1/0=0");
    let actual = error(&db, &throwing);
    let original = error(&db, &canonical(&throwing));
    assert_eq!(actual.kind, QueryErrorKind::Type);
    assert_eq!(
        (actual.kind, actual.message),
        (original.kind, original.message)
    );
    for text in [
        JOIN.replace("a.k=b.k", "false AND a.k=b.k AND 1/0=0"),
        format!(
            "UNWIND [] AS outer {}",
            JOIN.replace("a.k=b.k", "a.k=b.k AND 1/0=0")
        ),
    ] {
        assert_fallback(&checked_join(&db, &text, ExecutionOptions::default(), &[]));
    }
    let volatile = JOIN.replace("a.k=b.k", "a.k=b.k AND rand()>=0");
    assert_fallback(&checked_join(
        &db,
        &volatile,
        ExecutionOptions::default(),
        &expected_pairs(&build, &probe, &integer_pairs()),
    ));
    assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
    write(&db, "DROP CONSTRAINT Probe_k");
    write(&db, "MATCH(b:Probe) SET b.k='wrong family'");
    write(
        &db,
        "CREATE CONSTRAINT Probe_k FOR(n:Probe) REQUIRE n.k IS :: STRING",
    );
    checkpoint(&db);
    assert_eq!(error(&db, JOIN).kind, QueryErrorKind::Type);
    assert!(db.is_autocommit());
}

fn relationship_direction_bag(
    ids: &[Value],
    probe: &[Seed],
    reverse: bool,
    undirected: bool,
) -> Vec<Vec<Value>> {
    let mut expected = Vec::new();
    for (rel, key, source, target) in [(2, 5, 0, 1), (3, 5, 0, 1), (4, 13, 1, 1)] {
        for p in if key == 5 { vec![0, 1] } else { vec![2] } {
            let (a, b) = if reverse {
                (target, source)
            } else {
                (source, target)
            };
            expected.push(vec![
                ids[a].clone(),
                ids[rel].clone(),
                ids[b].clone(),
                probe[p].id.clone(),
                Value::Integer(key),
                Value::Integer(key),
            ]);
            if undirected && source != target {
                expected.push(vec![
                    ids[b].clone(),
                    ids[rel].clone(),
                    ids[a].clone(),
                    probe[p].id.clone(),
                    Value::Integer(key),
                    Value::Integer(key),
                ]);
            }
        }
    }
    expected
}

#[test]
fn relationship_identity_uniqueness_parallel_edges_self_loops_and_direction() {
    let db = empty_db();
    let created = write(
        &db,
        "CREATE (a:Endpoint:Visible),(b:Endpoint:Visible),\
        (a)-[r:EDGE {k:5}]->(b),(a)-[s:EDGE {k:5}]->(b),(b)-[t:EDGE {k:13}]->(b) \
        RETURN elementId(a),elementId(b),elementId(r),elementId(s),elementId(t)",
    );
    let ids = &created.rows[0];
    let padded = consume(
        &db,
        "MATCH(a:Endpoint) WHERE elementId(a)=$endpoint UNWIND range(0,252) AS seq \
         CREATE(a)-[r:EDGE {k:1000+seq}]->(a) RETURN elementId(r)",
        BTreeMap::from([("endpoint".into(), ids[0].clone())]),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert_eq!(padded.rows.len(), 253);
    let mut relationship_ids = ids[2..].to_vec();
    relationship_ids.extend(padded.rows.iter().map(|row| row[0].clone()));
    let mut keys = vec![
        Value::Integer(5),
        Value::Integer(5),
        Value::Integer(13),
        Value::Null,
    ];
    keys.extend((4..SIZE).map(|seq| Value::Integer(100_000 + seq as i64)));
    let probe = seed(&db, "Probe", keys);
    write(
        &db,
        "CREATE CONSTRAINT edge_k FOR ()-[r:EDGE]-() REQUIRE r.k IS :: INTEGER",
    );
    write(
        &db,
        "CREATE CONSTRAINT probe_k FOR(n:Probe) REQUIRE n.k IS :: INTEGER",
    );
    checkpoint(&db);
    let identity = "MATCH REPEATABLE ELEMENTS ()-[r:EDGE]->(),()-[s:EDGE]->() WHERE r=s RETURN elementId(r),elementId(s)";
    let expected = relationship_ids
        .iter()
        .map(|id| vec![id.clone(), id.clone()])
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), SIZE);
    // Explicit MATCH mode is a completeness barrier, not a StepCursor Hash input.
    assert_fallback(&checked_join(
        &db,
        identity,
        ExecutionOptions::default(),
        &expected,
    ));
    checked_join(
        &db,
        "MATCH ()-[r:EDGE]->(),()-[s:EDGE]->() WHERE r=s RETURN elementId(r),elementId(s)",
        ExecutionOptions::default(),
        &[],
    );
    for (pattern, reverse, undirected) in [
        ("(a)-[r:EDGE]->(b)", false, false),
        ("(a)<-[r:EDGE]-(b)", true, false),
        ("(a)-[r:EDGE]-(b)", false, true),
    ] {
        let text = format!(
            "MATCH {pattern},(p:Probe) WHERE r.k=p.k RETURN elementId(a),elementId(r),elementId(b),elementId(p),r.k,p.k"
        );
        let expected = relationship_direction_bag(ids, &probe, reverse, undirected);
        assert_eq!(expected.len(), if undirected { 9 } else { 5 });
        assert_hash(&checked_join(
            &db,
            &text,
            ExecutionOptions::default(),
            &expected,
        ));
    }
    assert_eq!(
        error(&db, "MATCH(n:Endpoint),()-[r:EDGE]->() WHERE n=r RETURN 1").kind,
        QueryErrorKind::Type
    );
}

#[test]
fn early_close_and_interrupt_release_hash_state_and_allow_a_fresh_full_read() {
    let (db, build, probe) = integer_fixture();
    let head = storage::branch_head(&db, "main").unwrap();
    let catalog = || {
        db.prepare("SELECT type,name FROM sqlite_temp_master ORDER BY type,name")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let before = catalog();
    for interrupted in [false, true] {
        let scope = ReadScope::new(&db);
        let prepared = prepare(
            &db,
            &format!("PROFILE {JOIN}"),
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .unwrap();
        assert!(prepared.physical.explain().contains("HashJoin"));
        let resources = prepared.resources.clone();
        let mut cursor = QueryCursor::new(prepared);
        let batch = cursor.next_batch(&db, 1).unwrap();
        assert_eq!(batch.rows.len(), 1);
        assert!(!batch.done);
        drop(batch);
        if interrupted {
            assert_eq!(
                cursor
                    .next_batch_with_interrupt(&db, 1, &|| true)
                    .unwrap_err()
                    .kind,
                QueryErrorKind::Interrupted
            );
        } else {
            cursor.cancel(&db).unwrap();
        }
        drop(cursor);
        assert_eq!(resources.used(), 0);
        drop(scope);
        assert!(db.is_autocommit());
        assert_eq!(catalog(), before);
        assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
        assert_hash(&checked_join(
            &db,
            JOIN,
            ExecutionOptions::default(),
            &expected_pairs(&build, &probe, &integer_pairs()),
        ));
    }
}
