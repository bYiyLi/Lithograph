use super::*;

fn assert_original_join_path(db: &Connection, text: &str, params: BTreeMap<String, Value>) {
    let prepared = prepare(db, text, params, ExecutionOptions::default()).unwrap();
    let plan = prepared.physical.explain();
    assert!(!plan.contains("NestedLoop"), "{text}: {plan}");
}

fn assert_same_error(
    db: &Connection,
    text: &str,
    oracle: &str,
    params: BTreeMap<String, Value>,
    kind: QueryErrorKind,
) {
    assert_original_join_path(db, text, params.clone());
    let actual = match execute(db, text, params.clone(), ExecutionOptions::default()) {
        Err(error) => error,
        Ok(rows) => panic!("expected {kind:?} for {text}, got rows {rows:?}"),
    };
    let expected = execute(db, oracle, params, ExecutionOptions::default()).unwrap_err();
    assert_eq!(actual.kind, kind, "{text}: {actual:?}");
    assert_eq!(actual.kind, expected.kind, "{text}");
    assert_eq!(actual.message, expected.message, "{text}");
}

#[test]
fn unsupported_whole_where_keeps_throw_volatile_and_or_on_the_original_path() {
    let db = empty_db();
    run(
        &db,
        "CREATE (:Item {v:1}), (:Item {v:2}), (:Pick {v:1}), (:Pick {v:2})",
    );
    for ddl in [
        "CREATE CONSTRAINT item_v FOR(n:Item) REQUIRE n.v IS :: INTEGER",
        "CREATE CONSTRAINT pick_v FOR(n:Pick) REQUIRE n.v IS :: INTEGER",
        "CREATE RANGE INDEX item_v_index FOR(n:Item) ON(n.v)",
    ] {
        run(&db, ddl);
    }
    for predicate in ["a.v=b.v AND rand()>=0", "a.v=b.v OR false"] {
        let text = format!("MATCH(a:Item),(b:Pick) WHERE {predicate} RETURN a.v,b.v");
        assert_original_join_path(&db, &text, BTreeMap::new());
        assert_same_bag(&db, &text);
        assert_eq!(run(&db, &text).len(), 2);
    }
    assert_same_error(
        &db,
        "MATCH(a:Item),(b:Pick) WHERE a.v=b.v AND 1/0=0 RETURN a.v",
        "MATCH(a:Item),(b:Pick) WHERE (a.v=b.v AND 1/0=0) OR false RETURN a.v",
        BTreeMap::new(),
        QueryErrorKind::Type,
    );
}

#[test]
fn null_join_keys_still_evaluate_a_throwing_and_residual() {
    let db = empty_db();
    run(&db, "CREATE (:Item), (:Pick {v:1})");
    for ddl in [
        "CREATE CONSTRAINT item_v FOR(n:Item) REQUIRE n.v IS :: INTEGER",
        "CREATE CONSTRAINT pick_v FOR(n:Pick) REQUIRE n.v IS :: INTEGER",
        "CREATE RANGE INDEX item_v_index FOR(n:Item) ON(n.v)",
    ] {
        run(&db, ddl);
    }
    assert_same_error(
        &db,
        "MATCH(a:Item),(b:Pick) WHERE a.v=b.v AND 1/0=0 RETURN 1",
        "MATCH(a:Item),(b:Pick) WHERE (a.v=b.v AND 1/0=0) OR false RETURN 1",
        BTreeMap::new(),
        QueryErrorKind::Type,
    );
    for predicate in ["false AND a.v=b.v AND 1/0=0", "false AND null AND 1/0=0"] {
        let text = format!("MATCH(a:Item),(b:Pick) WHERE {predicate} RETURN 1");
        assert_original_join_path(&db, &text, BTreeMap::new());
        assert_same_bag(&db, &text);
        assert!(run(&db, &text).is_empty());
    }
}

#[test]
fn zero_outer_defers_inner_type_errors_until_a_real_row_exists() {
    let db = empty_db();
    run(&db, "CREATE (:Present), (:Number {v:1}), (:Text {v:'one'})");
    for predicate in ["n.v=s.v", "1/0=0", "null AND n.v=s.v"] {
        let empty = format!("MATCH(a:Missing),(n:Number),(s:Text) WHERE {predicate} RETURN 1");
        assert_original_join_path(&db, &empty, BTreeMap::new());
        assert_same_bag(&db, &empty);
        assert!(run(&db, &empty).is_empty());
        let actual = format!("MATCH(a:Present),(n:Number),(s:Text) WHERE {predicate} RETURN 1");
        let oracle =
            format!("MATCH(a:Present),(n:Number),(s:Text) WHERE ({predicate}) OR false RETURN 1");
        assert_same_error(&db, &actual, &oracle, BTreeMap::new(), QueryErrorKind::Type);
    }
}

#[test]
fn unknown_incoming_node_relationship_and_path_bindings_do_not_become_join_keys() {
    let db = empty_db();
    run(
        &db,
        "CREATE (a:Item {v:1}), (b:Item {v:2}), (a)-[:EDGE]->(b)",
    );
    run(
        &db,
        "CREATE CONSTRAINT item_v FOR(n:Item) REQUIRE n.v IS :: INTEGER",
    );
    run(&db, "CREATE RANGE INDEX item_v_index FOR(n:Item) ON(n.v)");
    // The existing start-Node producer treats a non-Node binding as no match;
    // preserve that behavior as well as the end/Relationship/Path errors.
    let start = "WITH $input AS n MATCH(a:Item),(n:Item),(b:Item) WHERE a.v=b.v RETURN 1";
    let params = BTreeMap::from([("input".to_owned(), Value::Integer(0))]);
    assert_original_join_path(&db, start, params.clone());
    assert_eq!(
        execute(&db, start, params.clone(), ExecutionOptions::default()).unwrap(),
        execute(&db, &canonical(start), params, ExecutionOptions::default()).unwrap()
    );
    for (variable, reused) in [
        ("n", "()-[:EDGE]->(n:Item)"),
        ("r", "()-[r:EDGE]->()"),
        ("p", "p=()-[:EDGE]->()"),
    ] {
        let params = BTreeMap::from([("input".to_owned(), Value::Integer(0))]);
        let empty = format!(
            "WITH $input AS {variable} MATCH(a:Missing),{reused},(b:Item) WHERE a.v=b.v RETURN 1"
        );
        assert_original_join_path(&db, &empty, params.clone());
        assert!(
            execute(&db, &empty, params.clone(), ExecutionOptions::default())
                .unwrap()
                .is_empty()
        );
        let actual = format!(
            "WITH $input AS {variable} MATCH(a:Item),{reused},(b:Item) WHERE a.v=b.v RETURN 1"
        );
        let oracle = format!(
            "WITH $input AS {variable} MATCH(a:Item),{reused},(b:Item) WHERE (a.v=b.v) OR false RETURN 1"
        );
        assert_same_error(&db, &actual, &oracle, params, QueryErrorKind::Semantic);
    }
}

#[test]
fn incompatible_scalar_list_vector_and_entity_equality_remains_an_error() {
    let db = empty_db();
    run(
        &db,
        "CREATE (a:Number {v:1}), (b:Text {v:'one'}), \
         (:Numbers {v:[1]}), (:Strings {v:['one']}), \
         (:Float32Vector {v:vector([1,2],2,FLOAT32)}), \
         (:Float64Vector {v:vector([1,2],2,FLOAT64)}), \
         (:LongVector {v:vector([1,2,3],3,FLOAT32)}), (a)-[:EDGE]->(b)",
    );
    for ddl in [
        "CREATE CONSTRAINT number_v FOR(n:Number) REQUIRE n.v IS :: INTEGER",
        "CREATE CONSTRAINT text_v FOR(n:Text) REQUIRE n.v IS :: STRING",
        "CREATE CONSTRAINT numbers_v FOR(n:Numbers) REQUIRE n.v IS :: LIST<INTEGER NOT NULL>",
        "CREATE CONSTRAINT strings_v FOR(n:Strings) REQUIRE n.v IS :: LIST<STRING NOT NULL>",
        "CREATE CONSTRAINT vector32_v FOR(n:Float32Vector) REQUIRE n.v IS :: VECTOR<FLOAT32>(2)",
        "CREATE CONSTRAINT vector64_v FOR(n:Float64Vector) REQUIRE n.v IS :: VECTOR<FLOAT64>(2)",
        "CREATE CONSTRAINT long_vector_v FOR(n:LongVector) REQUIRE n.v IS :: VECTOR<FLOAT32>(3)",
    ] {
        run(&db, ddl);
    }
    for (pattern, predicate) in [
        ("(a:Number),(b:Text)", "a.v=b.v"),
        ("(a:Numbers),(b:Strings)", "a.v=b.v"),
        ("(a:Float32Vector),(b:Float64Vector)", "a.v=b.v"),
        ("(a:Float32Vector),(b:LongVector)", "a.v=b.v"),
        ("(a:Number),()-[r:EDGE]->()", "a=r"),
    ] {
        let actual = format!("MATCH {pattern} WHERE {predicate} RETURN 1");
        let oracle = format!("MATCH {pattern} WHERE ({predicate}) OR false RETURN 1");
        assert_same_error(&db, &actual, &oracle, BTreeMap::new(), QueryErrorKind::Type);
    }
}

#[test]
fn property_type_guards_wait_for_the_actual_shared_node_provider() {
    let db = empty_db();
    run(
        &db,
        "CREATE (:Typed {v:1}), (:Typed), (:Other {v:'wrong family'}), (:Target {v:1})",
    );
    for ddl in [
        "CREATE CONSTRAINT typed_v FOR(n:Typed) REQUIRE n.v IS :: INTEGER",
        "CREATE CONSTRAINT target_v FOR(n:Target) REQUIRE n.v IS :: INTEGER",
        "CREATE RANGE INDEX target_v_index FOR(n:Target) ON(n.v)",
    ] {
        run(&db, ddl);
    }
    let text = "MATCH(n),(m:Target),(n:Typed) WHERE n.v=m.v RETURN n.v,m.v";
    assert_same_bag(&db, text);
    assert_eq!(
        run(&db, text),
        vec![vec![Value::Integer(1), Value::Integer(1)]]
    );
    for values in ["[null,1]", "[null,2]"] {
        let text = format!(
            "MATCH(n),(m:Target),(n:Typed) WHERE n.v=m.v AND n.v IN {values} RETURN n.v,m.v"
        );
        assert_same_bag(&db, &text);
    }
}
