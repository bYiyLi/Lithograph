use super::*;

fn no_multiway(result: &support::Observed) {
    assert!(
        !result.plan.contains("MultiwayIntersect"),
        "{}",
        result.plan
    );
}

#[test]
fn written_incoming_direction_preserves_the_same_physical_directed_edges() {
    let fixture = CycleFixture::new(Shape::Triangle, 16, 1);
    let text = fixture.query().replace(
        "(n0:S8D0)-[r0:S8A]->(n1:S8D1)",
        "(n1:S8D1)<-[r0:S8A]-(n0:S8D0)",
    );
    assert_ne!(text, fixture.query());
    let result = read(
        &fixture.db,
        &format!("PROFILE {text}"),
        BTreeMap::new(),
        ExecutionOptions::default(),
        7,
    );
    assert_bag(&result.rows, &fixture.expected());
    assert_multiway(&result, fixture.expected().len());
    for missing in [
        fixture.query().replace("r0:S8A", "r0:S8Absent"),
        fixture.query().replace("-[r0:S8A]->", "<-[r0:S8A]-"),
    ] {
        let empty = read(
            &fixture.db,
            &format!("PROFILE {missing}"),
            BTreeMap::new(),
            ExecutionOptions::default(),
            1,
        );
        assert!(empty.rows.is_empty());
    }
}

#[test]
fn optional_and_nullable_or_erroneous_predicates_keep_the_existing_semantics() {
    use lithograph_core::query::QueryErrorKind;

    let fixture = CycleFixture::new(Shape::Triangle, 3, 1);
    let optional = fixture.query().replacen("MATCH ", "OPTIONAL MATCH ", 1);
    let result = read(
        &fixture.db,
        &format!("PROFILE {optional}"),
        BTreeMap::new(),
        ExecutionOptions::default(),
        1,
    );
    no_multiway(&result);
    assert_bag(&result.rows, &fixture.expected());
    let nullable = fixture
        .query()
        .replace(" RETURN ", " WHERE $selector RETURN ");
    let result = read(
        &fixture.db,
        &format!("PROFILE {nullable}"),
        BTreeMap::from([("selector".into(), Value::Null)]),
        ExecutionOptions::default(),
        1,
    );
    assert!(result.rows.is_empty());
    no_multiway(&result);
    for (text, params) in [
        (
            nullable,
            BTreeMap::from([("selector".into(), Value::Integer(1))]),
        ),
        (
            fixture.query().replace(" RETURN ", " WHERE 1/0 RETURN "),
            BTreeMap::new(),
        ),
    ] {
        let scope = support::ReadScope::new(&fixture.db);
        let error = support::execute(&fixture.db, &text, params, ExecutionOptions::default(), 1)
            .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Type);
        drop(scope);
        assert!(fixture.db.is_autocommit());
    }
    let recovered = read(
        &fixture.db,
        &fixture.query(),
        BTreeMap::new(),
        ExecutionOptions::default(),
        3,
    );
    assert_bag(&recovered.rows, &fixture.expected());
}

#[test]
fn untyped_undirected_and_path_patterns_retain_their_original_fallback() {
    let fixture = CycleFixture::new(Shape::Triangle, 3, 1);
    let base = fixture.query();
    for text in [
        base.replace("r0:S8A", "r0"),
        base.replace("-[r0:S8A]->", "-[r0:S8A]-"),
        base.replacen("MATCH ", "MATCH p=", 1),
    ] {
        let result = read(
            &fixture.db,
            &format!("PROFILE {text}"),
            BTreeMap::new(),
            ExecutionOptions::default(),
            1,
        );
        assert_bag(&result.rows, &fixture.expected());
        no_multiway(&result);
    }
}

#[test]
fn same_node_identity_and_parallel_self_loops_keep_per_match_relationship_uniqueness() {
    let db = support::empty_db();
    let created = support::write(
        &db,
        "CREATE(n:S8Loop) CREATE(n)-[r:S8T]->(n),(n)-[s:S8T]->(n),(n)-[t:S8T]->(n) \
         RETURN elementId(n),elementId(r),elementId(s),elementId(t)",
    );
    assert_eq!(created.rows.len(), 1);
    let ids = &created.rows[0];
    let mut expected = Vec::new();
    for first in 1..4 {
        for second in 1..4 {
            for third in 1..4 {
                if first != second && first != third && second != third {
                    expected.push(vec![
                        ids[0].clone(),
                        ids[0].clone(),
                        ids[0].clone(),
                        ids[first].clone(),
                        ids[second].clone(),
                        ids[third].clone(),
                    ]);
                }
            }
        }
    }
    assert_eq!(expected.len(), 6);
    support::checkpoint(&db);
    let result = read(
        &db,
        "PROFILE MATCH(a:S8Loop)-[r:S8T]->(b:S8Loop),(b)-[s:S8T]->(c:S8Loop),(c)-[t:S8T]->(a) \
         RETURN elementId(a),elementId(b),elementId(c),elementId(r),elementId(s),elementId(t)",
        BTreeMap::new(),
        ExecutionOptions::default(),
        1,
    );
    assert_bag(&result.rows, &expected);
    // Distinct variables may bind the same node. A tiny three-loop fixture
    // need not select the more expensive intersection algorithm.
}
