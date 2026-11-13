use std::collections::BTreeSet;

use super::*;

fn fixture() -> (support::StarFixture, Vec<Vec<Value>>) {
    let fixture = support::StarFixture::new(64, 128);
    let ids = fixture
        .left
        .iter()
        .map(|arm| node_id(&arm.endpoint).to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), 64);
    let expected = ids
        .iter()
        .map(|id| vec![Value::String(id.clone())])
        .collect::<Vec<_>>();
    let labelled = support::write(
        &fixture.db,
        "MATCH(a:LeafA) SET a:G01SipProbe RETURN elementId(a)",
    );
    assert_bag(&labelled.rows, &expected);
    assert_eq!(labelled.counters.labels_added, 64);
    let noise = support::write(
        &fixture.db,
        "UNWIND range(0,127) AS seq CREATE(n:G01SipProbe {noise:seq}) RETURN elementId(n)",
    );
    assert_eq!(noise.rows.len(), 128);
    assert_eq!(noise.counters.nodes_created, 128);
    let mut noise_ids = BTreeSet::new();
    for row in noise.rows {
        assert_eq!(row.len(), 1);
        let id = node_id(&row[0]);
        assert!(!ids.contains(id) && noise_ids.insert(id.to_owned()));
    }
    support::checkpoint(&fixture.db);
    (fixture, expected)
}

fn assert_factor(actual: &Observed) {
    let operator = actual_operator(actual, "FactorizedAccumulate");
    assert!(
        operator.rows > 0,
        "the factor table must execute, not only appear in a plan"
    );
    let work = operator
        .factorized
        .as_ref()
        .expect("actual factor table work");
    assert!(work.prefixes > 0 && work.accumulate_rows > 0);
    assert_eq!(work.group_members, 384);
    let flatten = actual_operator(actual, "Flatten");
    assert_eq!(
        flatten.rows, 32768,
        "DISTINCT consumes every parallel RID tuple"
    );
    assert_eq!(flatten.factorized.as_ref().unwrap().flattened_rows, 32768);
}

#[test]
fn factorized_complete_output_supplies_a_real_sip_list_without_crossing_its_proof_boundary() {
    let (fixture, expected) = fixture();
    let before = storage::branch_head(&fixture.db, "main").unwrap();
    let star = support::STAR.split(" RETURN ").next().unwrap();
    let text = format!(
        "{star} WITH DISTINCT elementId(a) AS aid WITH [aid] AS ids \
         MATCH(x:G01SipProbe) WHERE elementId(x) IN ids RETURN elementId(x)"
    );
    let actual = read(
        &fixture.db,
        &format!("PROFILE {text}"),
        support::params(),
        ExecutionOptions::default(),
        5,
    );
    assert_bag(&actual.rows, &expected);
    assert_factor(&actual);
    assert_sip(&actual, 64);
    assert_pinned(&actual, before);

    let original = read(
        &fixture.db,
        &format!("PROFILE {}", fallback(&text)),
        support::params(),
        ExecutionOptions::default(),
        11,
    );
    assert_bag(&original.rows, &expected);
    assert_eq!(actual.columns, original.columns);
    assert_factor(&original);
    assert_no_sip(&original);
    assert_pinned(&original, before);
    assert!(
        actual.db_hits < original.db_hits,
        "SIP must skip the128 unrelated target owners"
    );

    let plain = read(
        &fixture.db,
        &text,
        support::params(),
        ExecutionOptions::default(),
        13,
    );
    assert_bag(&plain.rows, &expected);
    assert!(plain.profile.is_empty());
    assert_pinned(&plain, before);
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
    assert!(fixture.db.is_autocommit());
}
