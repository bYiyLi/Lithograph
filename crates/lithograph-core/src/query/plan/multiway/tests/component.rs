//! Component selection must price and preserve its actual binary prefix.

use super::*;

#[test]
fn node_only_tail_keeps_real_cyclic_prefix_and_original_part_ids() {
    let (db, cycle) = fixture(&[(0, 1), (1, 2), (2, 0)]);
    assert_eq!(
        run(
            &db,
            "UNWIND range(0,128) AS i CREATE(n:S8ComponentTail {seq:i}) RETURN elementId(n)"
        ),
        129
    );
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    let text = cycle.replace(" RETURN ", ",(tail:S8ComponentTail) RETURN ");
    ADVICE.set(Some(Advice::default()));
    let prepared = prepare(&db, &text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let advice = ADVICE.take().unwrap();
    let physical = prepared.physical.explain();
    eprintln!("component-node-tail text={text} advice={advice:?} plan={physical}");
    let step = &prepared.matches[0];
    let join = step.join.as_ref().expect("complete binary fallback");
    let multiway = join
        .multiway
        .as_ref()
        .expect("beneficial cyclic prefix retains its ordinary node tail");
    assert_eq!(step.parts.len(), 4);
    assert_eq!(join.stages.len(), 4);
    assert_eq!(multiway.edges.len(), 3);
    assert_eq!(join.stages[3].part, 3);
    let parts = multiway
        .edges
        .iter()
        .map(|edge| edge.part)
        .collect::<BTreeSet<_>>();
    assert_eq!(parts, BTreeSet::from([0, 1, 2]));
    assert_eq!(
        parts,
        join.stages[..3]
            .iter()
            .map(|stage| stage.part)
            .collect::<BTreeSet<_>>()
    );
    let (candidate, binary) = advice.comparison.expect("priced the same real prefix");
    assert!(candidate < binary.unwrap());
    assert!(physical.contains("MultiwayIntersect") && physical.contains("Cartesian"));
}

#[test]
fn untyped_relationship_tail_keeps_its_own_physical_type_and_direction() {
    let (db, cycle) = fixture(&[(0, 1), (1, 2), (2, 0)]);
    let text = cycle.replace(" RETURN ", ",(n2)-[tail]->(next:S8Private0) RETURN ");
    let prepared = prepare(&db, &text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let physical = prepared.physical.explain();
    eprintln!("component-untyped-tail text={text} plan={physical}");
    let step = &prepared.matches[0];
    let join = step.join.as_ref().expect("complete binary fallback");
    assert_eq!(join.multiway.as_ref().unwrap().edges.len(), 3);
    assert!(physical.contains("MultiwayIntersect"));
    assert_eq!(join::expansion_metadata(step, 0), None);
    assert_eq!(join::expansion_metadata(step, 1), None);
    assert_eq!(join::expansion_metadata(step, 2), None);
    let tail = join::expansion_metadata(step, 3).expect("tail still expands");
    assert_eq!(
        tail.0, None,
        "tail must not inherit a prefix relationship type"
    );
    assert!(matches!(tail.1, Direction::Outgoing | Direction::Incoming));
}

#[test]
fn closing_relationship_key_keeps_a_real_hash_tail_after_the_cyclic_prefix() {
    let (db, cycle) = fixture(&[(0, 1), (1, 2), (2, 0)]);
    run(
        &db,
        "MATCH(a:S8Private2)-[r:S8PrivateR2]->() SET r.k=a.seq FINISH",
    );
    let keys = (0..1024)
        .map(|index| match index {
            0 | 1 => Value::Integer(0),
            2 => Value::Integer(13),
            3 => Value::Null,
            _ => Value::Integer(100_000 + index),
        })
        .collect();
    let created = run_params(
        &db,
        "UNWIND $keys AS k CREATE(n:S8ComponentBuild {k:k}) RETURN elementId(n)",
        BTreeMap::from([("keys".into(), Value::List(keys))]),
    );
    assert_eq!(created, 1024);
    for text in [
        "CREATE CONSTRAINT s8_component_rel_type FOR ()-[r:S8PrivateR2]-() REQUIRE r.k IS :: INTEGER",
        "CREATE CONSTRAINT s8_component_build_type FOR (n:S8ComponentBuild) REQUIRE n.k IS :: INTEGER",
    ] {
        run(&db, text);
    }
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    let text = cycle.replace(
        " RETURN ",
        ",(tail:S8ComponentBuild) WHERE r2.k=tail.k RETURN ",
    );
    ADVICE.set(Some(Advice::default()));
    crate::query::plan::budget::trace::start();
    let prepared = prepare(&db, &text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let trace = crate::query::plan::budget::trace::take();
    let advice = ADVICE.take().unwrap();
    let step = &prepared.matches[0];
    let join = step.join.as_ref().expect("complete binary fallback");
    eprintln!(
        "component-hash-tail text={text} advice={advice:?} trace={trace:#?} stages={:#?} plan={}",
        join.stages,
        prepared.physical.explain()
    );
    assert_eq!(join.multiway.as_ref().unwrap().edges.len(), 3);
    assert!(matches!(
        join.stages[3].access,
        join::StageAccess::Hash { .. }
    ));
    assert!(prepared.physical.explain().contains("HashJoin"));
}
