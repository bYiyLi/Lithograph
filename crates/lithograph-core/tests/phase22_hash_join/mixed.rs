use super::*;

#[test]
fn hash_and_identity_index_nested_loop_share_the_real_match_cursor() {
    let (db, build, probe) = integer_fixture();
    let text = "MATCH(a:Build),(b:Probe),(same:Build) WHERE a.k=b.k AND a=same \
        RETURN elementId(a) AS build,elementId(b) AS probe,elementId(same) AS same,\
        a.k AS buildKey,b.k AS probeKey,same.k AS sameKey";
    // The third identity is the captured Build identity, not a Cartesian
    // oracle over all three labels. The pair's original oracle is tested above.
    let expected = integer_pairs()
        .into_iter()
        .map(|(a, b)| {
            vec![
                build[a].id.clone(),
                probe[b].id.clone(),
                build[a].id.clone(),
                build[a].key.clone(),
                probe[b].key.clone(),
                build[a].key.clone(),
            ]
        })
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), 24);
    let actual = read(&db, &format!("PROFILE {text}"), ExecutionOptions::default());
    assert_eq!(
        actual.columns,
        ["build", "probe", "same", "buildKey", "probeKey", "sameKey"]
    );
    assert_bag(&actual.rows, &expected);
    let profile = actual.profile.as_ref().expect("complete PROFILE execution");
    let hash = profile
        .iter()
        .find(|operator| operator.operator == "HashJoin" && operator.rows > 0)
        .unwrap_or_else(|| panic!("actual mixed Hash producer: {}; {profile:?}", actual.plan));
    let counters = hash.hash.as_ref().expect("actual Hash runtime counters");
    assert!(counters.build_rows > 0 && counters.probe_rows > 0);
    for producer in ["IndexNestedLoop", "NodeIdSeek"] {
        assert!(
            profile.iter().any(|operator| {
                operator.operator == producer
                    && operator.rows > 0
                    && (producer != "NodeIdSeek" || operator.db_hits > 0)
            }),
            "actual {producer} beside Hash: {}; {profile:?}",
            actual.plan
        );
    }
}
