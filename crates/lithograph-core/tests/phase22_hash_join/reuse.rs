use super::*;

const SIDE_ROWS: u64 = 64;

fn fixture() -> (Connection, Vec<Seed>, Vec<Seed>) {
    let db = empty_db();
    let build = seed(&db, "Build", (1..=64).map(Value::Integer).collect());
    let probe = seed(&db, "Probe", (64..=127).map(Value::Integer).collect());
    constraints(&db, "INTEGER", "INTEGER");
    (db, build, probe)
}

fn repeated_profile(db: &Connection, text: &str, writes: bool) -> Observed {
    // The caller-owned read context applies only to reads. Do not conceal a
    // mutating program's publication or rollback with a test savepoint.
    let scope = (!writes).then(|| ReadScope::new(db));
    let prepared = prepare(
        db,
        &format!("PROFILE {text}"),
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap();
    let plan = prepared.physical.explain();
    assert!(
        plan.contains("HashJoin"),
        "real repeated StepCursor route: {plan}"
    );
    let columns = prepared.columns.clone();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let result = (|| -> Result<_, QueryError> {
        let mut rows = Vec::new();
        loop {
            let done = {
                let batch = cursor.next_batch(db, 3)?;
                let done = batch.done;
                rows.extend(batch.rows);
                done
            };
            if done {
                let summary = cursor.complete(db)?;
                let profile = summary
                    .metrics
                    .operator_profile()
                    .expect("PROFILE")
                    .to_vec();
                assert_eq!(
                    profile.iter().map(|operator| operator.db_hits).sum::<u64>(),
                    summary.metrics.db_hits,
                    "all repeated-base work is accounted: {plan}; {profile:?}"
                );
                return Ok(Observed {
                    columns,
                    plan,
                    rows,
                    profile: Some(profile),
                    counters: summary.counters.clone(),
                    commit: summary.commit.clone(),
                });
            }
        }
    })();
    drop(cursor);
    assert_eq!(
        resources.used(),
        0,
        "repeated-base success/error releases leases"
    );
    drop(scope);
    assert!(db.is_autocommit());
    result.unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

fn assert_build_once(result: &Observed, bases: u64) {
    assert_hash(result);
    let hashes = result
        .profile
        .as_ref()
        .unwrap()
        .iter()
        .filter(|operator| operator.operator == "HashJoin")
        .collect::<Vec<_>>();
    assert_eq!(hashes.len(), 1, "{:?}", result.profile);
    assert_eq!(hashes[0].rows, bases);
    let hash = hashes[0].hash.as_ref().unwrap();
    assert_eq!(
        hash.build_rows, SIDE_ROWS,
        "one clause-owned build: {hash:?}"
    );
    assert_eq!(
        hash.probe_rows,
        SIDE_ROWS * bases,
        "every new base probes all rows: {hash:?}"
    );
}

#[test]
fn unwind_bases_reuse_one_hash_build_without_retaining_an_old_prefix() {
    let (db, build, probe) = fixture();
    let head = storage::branch_head(&db, "main").unwrap();
    let text = "UNWIND range(1,270) AS seed MATCH(b:Build),(p:Probe) WHERE b.k=p.k \
        RETURN seed AS seed,elementId(b) AS build,elementId(p) AS probe,\
        b.k AS buildKey,p.k AS probeKey";
    let expected = (1..=270)
        .map(|seed| {
            vec![
                Value::Integer(seed),
                build[63].id.clone(),
                probe[0].id.clone(),
                Value::Integer(64),
                Value::Integer(64),
            ]
        })
        .collect::<Vec<_>>();
    let result = repeated_profile(&db, text, false);
    assert_eq!(
        result.columns,
        ["seed", "build", "probe", "buildKey", "probeKey"]
    );
    assert_bag(&result.rows, &expected);
    assert_build_once(&result, 270);
    assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
}

#[test]
fn streamed_write_bases_reuse_one_hash_build_across_the_256_row_boundary() {
    let (db, build, probe) = fixture();
    let created = write(
        &db,
        "UNWIND range(1,260) AS seq CREATE(s:Seed {seq:seq}) \
        RETURN seq,elementId(s)",
    );
    assert_eq!(created.rows.len(), 260);
    write(
        &db,
        "CREATE CONSTRAINT Seed_seq FOR(s:Seed) REQUIRE s.seq IS :: INTEGER",
    );
    checkpoint(&db);
    let head = storage::branch_head(&db, "main").unwrap();
    // This measured query has requires_program=false; the earlier UNWIND only
    // seeds the fixture. It uses prepare_write_query -> execute_write ->
    // execute_match_clause_spilled -> for_each_batch -> spill_match_step, the
    // existing 256-row write bridge with clause-owned reusable read bindings.
    let text = "MATCH(s:Seed) MATCH(b:Build),(p:Probe) WHERE b.k=p.k SET s.marker=7 \
        RETURN s.seq AS seed,elementId(s) AS source,elementId(b) AS build,elementId(p) AS probe";
    let expected = created
        .rows
        .iter()
        .map(|source| {
            vec![
                source[0].clone(),
                source[1].clone(),
                build[63].id.clone(),
                probe[0].id.clone(),
            ]
        })
        .collect::<Vec<_>>();
    let result = repeated_profile(&db, text, true);
    assert_eq!(result.columns, ["seed", "source", "build", "probe"]);
    assert_bag(&result.rows, &expected);
    assert_build_once(&result, 260);
    // Each distinct incoming Seed owns one changed canonical property slot.
    // Repeatedly setting the same Build marker would correctly count only one.
    assert_eq!(result.counters.properties_set, 260);
    assert_ne!(storage::branch_head(&db, "main").unwrap(), head);
    assert_eq!(
        query(&db, "MATCH(s:Seed) RETURN s.seq,s.marker ORDER BY s.seq"),
        (1..=260)
            .map(|seq| vec![Value::Integer(seq), Value::Integer(7)])
            .collect::<Vec<_>>()
    );
}
