//! Different MATCH clauses share the real Program stream and one graph pin.

use std::collections::BTreeSet;

use lithograph_core::query::QueryErrorKind;

use super::*;

#[test]
fn factorized_distinct_prefix_feeds_a_fresh_multiway_component() {
    let star = support::StarFixture::new(64, 128);
    let hub = star.hub.clone();
    let fixture = CycleFixture::with_db(star.db, Shape::Triangle, 16, 1);
    let star_match = support::STAR.split(" RETURN ").next().unwrap();
    let text = format!(
        "PROFILE {star_match} WITH DISTINCT h {}",
        fixture.query().replace(" RETURN ", " RETURN elementId(h),")
    );
    let expected = fixture
        .expected()
        .into_iter()
        .map(|mut row| {
            row.insert(0, hub.clone());
            row
        })
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), 352);
    let before = storage::branch_head(&fixture.db, "main").unwrap();
    let actual = read(
        &fixture.db,
        &text,
        support::params(),
        ExecutionOptions::default(),
        7,
    );
    assert_bag(&actual.rows, &expected);
    assert_multiway(&actual, expected.len());
    let factor = actual_operator(&actual, "FactorizedAccumulate");
    assert!(factor.rows > 0);
    let work = factor.factorized.as_ref().expect("actual factor table");
    assert!(work.prefixes > 0 && work.accumulate_rows > 0);
    assert!(work.group_members > 0 && work.group_members < 32768);
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
    assert!(fixture.db.is_autocommit());
}

#[derive(Clone)]
struct Seed {
    id: Value,
    key: Value,
}

fn seed_keys(db: &rusqlite::Connection, label: &str, keys: &[Value]) -> Vec<Seed> {
    let rows = keys
        .iter()
        .enumerate()
        .map(|(seq, key)| {
            Value::Map(BTreeMap::from([
                ("seq".into(), Value::Integer(seq as i64)),
                ("k".into(), key.clone()),
            ]))
        })
        .collect::<Vec<_>>();
    let created = support::execute(
        db,
        &format!("UNWIND $seeds AS seed CREATE(n:{label} {{seq:seed.seq,k:seed.k}}) RETURN n.seq,elementId(n),n.k"),
        BTreeMap::from([("seeds".into(), Value::List(rows))]),
        ExecutionOptions::default(),
        37,
    ).unwrap();
    assert_eq!(created.rows.len(), keys.len());
    let mut result = vec![
        Seed {
            id: Value::Null,
            key: Value::Null
        };
        keys.len()
    ];
    let mut ids = BTreeSet::new();
    let mut sequences = BTreeSet::new();
    for row in created.rows {
        let Value::Integer(seq) = row[0] else {
            panic!("typed public seed sequence")
        };
        let seq = usize::try_from(seq).unwrap();
        assert!(seq < keys.len() && sequences.insert(seq));
        assert_eq!(row[2], keys[seq]);
        assert!(
            matches!(&row[1], Value::String(id) if id.starts_with("n:") && ids.insert(id.clone()))
        );
        result[seq] = Seed {
            id: row[1].clone(),
            key: row[2].clone(),
        };
    }
    assert!(result.iter().all(|seed| seed.id != Value::Null));
    support::write(
        db,
        &format!("CREATE CONSTRAINT {label}_k FOR(n:{label}) REQUIRE n.k IS :: INTEGER"),
    );
    result
}

fn assert_repeated_work(actual: &support::Observed, baseline: &support::Observed, times: u64) {
    let actual = actual_operator(actual, "MultiwayIntersect")
        .multiway
        .as_ref()
        .unwrap();
    let baseline = actual_operator(baseline, "MultiwayIntersect")
        .multiway
        .as_ref()
        .unwrap();
    for (name, actual, once) in [
        ("input lists", actual.input_lists, baseline.input_lists),
        (
            "adjacency rows",
            actual.adjacency_rows,
            baseline.adjacency_rows,
        ),
        ("seeks", actual.seek_operations, baseline.seek_operations),
        (
            "intersection steps",
            actual.intersection_steps,
            baseline.intersection_steps,
        ),
        (
            "candidate nodes",
            actual.candidate_nodes,
            baseline.candidate_nodes,
        ),
        (
            "node assignments",
            actual.node_assignments,
            baseline.node_assignments,
        ),
        (
            "relationship candidates",
            actual.relationship_candidates,
            baseline.relationship_candidates,
        ),
    ] {
        assert_eq!(
            actual,
            once * times,
            "all {times} incoming owners contribute {name}"
        );
    }
}

#[test]
fn typed_hash_pairs_feed_fresh_multiway_components_with_complete_identities() {
    let fixture = CycleFixture::new(Shape::Triangle, 16, 1);
    let build_keys = (0..256)
        .map(|seq| Value::Integer(seq % 32))
        .collect::<Vec<_>>();
    let probe_keys = (0..256)
        .map(|seq| match seq {
            0 | 1 => Value::Integer(5),
            2 => Value::Integer(13),
            3 => Value::Null,
            4 => Value::Integer(99_999),
            _ => Value::Integer(100_000 + seq),
        })
        .collect::<Vec<_>>();
    let build = seed_keys(&fixture.db, "S8Build", &build_keys);
    let probe = seed_keys(&fixture.db, "S8Probe", &probe_keys);
    support::checkpoint(&fixture.db);
    let mut pairs = Vec::new();
    for left in &build {
        for right in &probe {
            if left.key != Value::Null && left.key == right.key {
                pairs.push(vec![
                    left.id.clone(),
                    right.id.clone(),
                    left.key.clone(),
                    right.key.clone(),
                ]);
            }
        }
    }
    assert_eq!(pairs.len(), 24);
    let first_pair = pairs[0].clone();
    let params = BTreeMap::from([
        ("firstUid".into(), first_pair[0].clone()),
        ("firstVid".into(), first_pair[1].clone()),
    ]);
    let cycle_expected = fixture.expected();
    let mut expected = Vec::new();
    for pair in pairs {
        for cycle in &cycle_expected {
            let mut row = pair.clone();
            row.extend(cycle.iter().cloned());
            expected.push(row);
        }
    }
    assert_eq!(expected.len(), 24 * 352);
    let prefix = "PROFILE MATCH(u:S8Build),(v:S8Probe) WHERE u.k=v.k \
         WITH elementId(u) AS uid,elementId(v) AS vid,u.k AS uk,v.k AS vk";
    let cycle = fixture
        .query()
        .replace(" RETURN ", " RETURN uid,vid,uk,vk,");
    let text = format!("{prefix} {cycle}");
    // Keep the same two-MATCH planning share and incoming columns. A pure
    // single-MATCH baseline may choose another valid variable order under its
    // larger budget. Filter one captured pair only after the complete Hash
    // prefix; no LIMIT or aggregate changes the consumption proof.
    let one_owner = format!("{prefix} WHERE uid=$firstUid AND vid=$firstVid {cycle}");
    let one_expected = cycle_expected
        .iter()
        .map(|cycle| {
            let mut row = first_pair.clone();
            row.extend(cycle.iter().cloned());
            row
        })
        .collect::<Vec<_>>();
    let before = storage::branch_head(&fixture.db, "main").unwrap();
    let baseline = read(
        &fixture.db,
        &one_owner,
        params.clone(),
        ExecutionOptions::default(),
        13,
    );
    assert_bag(&baseline.rows, &one_expected);
    assert_multiway(&baseline, 352);
    let baseline_hash = actual_operator(&baseline, "HashJoin");
    assert_eq!(
        baseline_hash.rows, 24,
        "the one-owner WHERE is downstream of the complete Hash pair bag"
    );
    let baseline_work = baseline_hash.hash.as_ref().unwrap();
    assert!(baseline_work.build_rows > 0 && baseline_work.probe_rows > 0);
    let actual = read(&fixture.db, &text, params, ExecutionOptions::default(), 13);
    assert_bag(&actual.rows, &expected);
    assert_multiway(&actual, expected.len());
    assert_repeated_work(&actual, &baseline, 24);
    let joined = actual_operator(&actual, "HashJoin");
    assert_eq!(joined.rows, 24);
    let work = joined.hash.as_ref().expect("actual typed hash execution");
    assert!(work.build_rows > 0 && work.probe_rows > 0);
    assert_eq!(work.build_rows, baseline_work.build_rows);
    assert_eq!(work.probe_rows, baseline_work.probe_rows);
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
    assert!(fixture.db.is_autocommit());
}

#[test]
fn multiple_incoming_tags_add_all_multiway_work_instead_of_replacing_the_last_owner() {
    let fixture = CycleFixture::new(Shape::Triangle, 16, 1);
    let cycle_expected = fixture.expected();
    let baseline = read(
        &fixture.db,
        &format!("PROFILE {}", fixture.query()),
        BTreeMap::new(),
        ExecutionOptions::default(),
        17,
    );
    assert_bag(&baseline.rows, &cycle_expected);
    assert_multiway(&baseline, 352);
    let mut expected = Vec::new();
    for tag in [1, 2] {
        for cycle in &cycle_expected {
            let mut row = vec![Value::Integer(tag)];
            row.extend(cycle.iter().cloned());
            expected.push(row);
        }
    }
    assert_eq!(expected.len(), 704);
    let text = format!(
        "PROFILE UNWIND [1,2] AS tag {}",
        fixture.query().replace(" RETURN ", " RETURN tag,")
    );
    let actual = read(
        &fixture.db,
        &text,
        BTreeMap::new(),
        ExecutionOptions::default(),
        11,
    );
    assert_bag(&actual.rows, &expected);
    assert_multiway(&actual, expected.len());
    assert_repeated_work(&actual, &baseline, 2);
}

#[test]
fn multiway_distinct_anchors_feed_an_identity_binary_join_without_losing_rids() {
    let star = support::StarFixture::new(4, 4);
    let hub = star.hub.clone();
    let left = star.left.clone();
    let fixture = CycleFixture::with_db(star.db, Shape::Triangle, 16, 1);
    let mut expected = Vec::new();
    for anchor in &fixture.nodes[0] {
        for edge in &left {
            expected.push(vec![
                anchor.clone(),
                hub.clone(),
                edge.relationship.clone(),
                edge.endpoint.clone(),
                edge.endpoint.clone(),
            ]);
        }
    }
    assert_eq!(expected.len(), 16 * 8);
    let cycle = fixture.query();
    let cycle_match = cycle.split(" RETURN ").next().unwrap();
    let text = format!(
        "PROFILE {cycle_match} WITH DISTINCT n0 AS anchor \
         MATCH(u:Hub)-[ar:A]->(v:LeafA),(same:LeafA) WHERE v=same \
         RETURN elementId(anchor),elementId(u),elementId(ar),elementId(v),elementId(same)"
    );
    let actual = read(
        &fixture.db,
        &text,
        BTreeMap::new(),
        ExecutionOptions::default(),
        19,
    );
    assert_bag(&actual.rows, &expected);
    assert_multiway(&actual, 352);
    let binary = actual
        .profile
        .iter()
        .filter(|operator| {
            matches!(
                operator.operator.as_str(),
                "IndexNestedLoop" | "HashJoin" | "NestedLoop"
            ) && operator.rows > 0
        })
        .collect::<Vec<_>>();
    assert_eq!(
        binary.len(),
        1,
        "a real binary join must accompany the intersection: {}; {:?}",
        actual.plan,
        actual.profile
    );
    if binary[0].operator == "NestedLoop" {
        // A legal shared-planner fallback still applies the real equality.
        // Keep this fixture bounded: four possible `same` nodes per input,
        // rather than the old 131072-candidate debug workload.
        assert_eq!(binary[0].rows, (expected.len() * 4) as u64);
        assert!(actual.profile.iter().any(
            |operator| operator.operator == "Filter" && operator.rows == expected.len() as u64
        ));
    }
}

#[test]
fn parameterized_residual_keeps_the_original_where_across_repreparations() {
    let fixture = CycleFixture::new(Shape::Triangle, 16, 1);
    let text = fixture.query().replace(" RETURN ", " WHERE $gate RETURN ");
    let expected = fixture.expected();
    for value in [
        Value::Boolean(true),
        Value::Boolean(false),
        Value::Null,
        Value::Boolean(true),
    ] {
        let actual = read(
            &fixture.db,
            &format!("PROFILE {text}"),
            BTreeMap::from([("gate".into(), value.clone())]),
            ExecutionOptions::default(),
            5,
        );
        if value == Value::Boolean(true) {
            assert_bag(&actual.rows, &expected);
            assert_multiway(&actual, expected.len());
        } else {
            assert!(actual.rows.is_empty());
        }
    }
    let scope = support::ReadScope::new(&fixture.db);
    let error = support::execute(
        &fixture.db,
        &text,
        BTreeMap::from([("gate".into(), Value::Integer(1))]),
        ExecutionOptions::default(),
        1,
    )
    .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Type);
    drop(scope);
    let recovered = read(
        &fixture.db,
        &format!("PROFILE {text}"),
        BTreeMap::from([("gate".into(), Value::Boolean(true))]),
        ExecutionOptions::default(),
        7,
    );
    assert_bag(&recovered.rows, &expected);
    assert_multiway(&recovered, expected.len());
}
