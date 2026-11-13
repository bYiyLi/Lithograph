use super::*;
use lithograph_core::query::QueryErrorKind;

const HASH_SIP: &str = "MATCH(a:G01Build),(b:G01Probe) WHERE a.k=b.k \
    WITH DISTINCT elementId(a) AS aid,a.k AS key WITH [aid] AS ids,key \
    MATCH(x:G01Build) WHERE elementId(x) IN ids RETURN elementId(x),x.k";

fn assert_hash(actual: &Observed) {
    let operator = actual_operator(actual, "HashJoin");
    assert_eq!(
        operator.rows, 24,
        "every duplicate-key pair reaches DISTINCT"
    );
    let work = operator.hash.as_ref().expect("actual typed Hash work");
    assert!(work.build_rows > 0 && work.probe_rows > 0, "{work:?}");
}

#[test]
fn hash_duplicate_pairs_feed_completed_sip_lists_in_the_same_query() {
    let fixture = fixture::HashFixture::new();
    let before = storage::branch_head(&fixture.db, "main").unwrap();
    let actual = read(
        &fixture.db,
        &format!("PROFILE {HASH_SIP}"),
        BTreeMap::new(),
        ExecutionOptions::default(),
        3,
    );
    assert_bag(&actual.rows, &fixture.expected);
    assert_hash(&actual);
    assert_sip(&actual, 16);
    assert_pinned(&actual, before);

    // Keep the Hash prefix intact. Only the final IN is deliberately outside
    // the current SIP proof; its original scan must preserve the same bag.
    let original = read(
        &fixture.db,
        &format!("PROFILE {}", fallback(HASH_SIP)),
        BTreeMap::new(),
        ExecutionOptions::default(),
        7,
    );
    assert_bag(&original.rows, &fixture.expected);
    assert_eq!(actual.columns, original.columns);
    assert_hash(&original);
    assert_no_sip(&original);
    assert_pinned(&original, before);
    assert!(
        actual.db_hits < original.db_hits,
        "actual finite reads skip unchosen Build owners"
    );

    let plain = read(
        &fixture.db,
        HASH_SIP,
        BTreeMap::new(),
        ExecutionOptions::default(),
        1,
    );
    assert_bag(&plain.rows, &fixture.expected);
    assert!(plain.profile.is_empty());
    assert_pinned(&plain, before);
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
    assert!(fixture.db.is_autocommit());
}

#[test]
fn streamed_lists_validate_every_member_before_narrowing_and_recover_after_type_errors() {
    let fixture = fixture::HashFixture::new();
    let text = "WITH $members AS ids MATCH(x:G01Build) WHERE elementId(x) IN ids \
        RETURN elementId(x),x.k";
    let mut members = fixture
        .expected
        .iter()
        .map(|row| row[0].clone())
        .collect::<Vec<_>>();
    members.extend([members[0].clone(), Value::Null]);
    let params = BTreeMap::from([("members".into(), Value::List(members.clone()))]);
    let before = storage::branch_head(&fixture.db, "main").unwrap();
    let actual = read(
        &fixture.db,
        &format!("PROFILE {text}"),
        params.clone(),
        ExecutionOptions::default(),
        1,
    );
    assert_bag(&actual.rows, &fixture.expected);
    assert_sip(&actual, 16);
    assert_pinned(&actual, before);

    members.insert(0, Value::Integer(1));
    let bad = BTreeMap::from([("members".into(), Value::List(members))]);
    let mut errors = Vec::new();
    for query in [text.to_owned(), fallback(text)] {
        let scope = support::ReadScope::new(&fixture.db);
        let error = support::execute(
            &fixture.db,
            &format!("PROFILE {query}"),
            bad.clone(),
            ExecutionOptions::default(),
            1,
        )
        .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Type);
        errors.push(error.message);
        drop(scope);
    }
    assert_eq!(
        errors[0], errors[1],
        "no mixed-list member may be silently excluded"
    );
    let recovered = read(
        &fixture.db,
        &format!("PROFILE {text}"),
        params,
        ExecutionOptions::default(),
        3,
    );
    assert_bag(&recovered.rows, &fixture.expected);
    assert_sip(&recovered, 16);
    assert_pinned(&recovered, before);
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
    assert!(fixture.db.is_autocommit());
}

#[test]
fn two_legacy_streaming_sip_sites_keep_their_own_keys_seeks_and_rows() {
    let fixture = fixture::HashFixture::new();
    let text = "UNWIND $idsList AS ids \
        MATCH(x:G01Build) WHERE elementId(x) IN ids \
        WITH x,[elementId(x)] AS nextIds \
        MATCH(y:G01Build) WHERE elementId(y) IN nextIds \
        RETURN elementId(x),elementId(y),x.k,y.k";
    let lists = fixture
        .expected
        .iter()
        .map(|row| Value::List(vec![row[0].clone()]))
        .collect::<Vec<_>>();
    let expected = fixture
        .expected
        .iter()
        .map(|row| {
            vec![
                row[0].clone(),
                row[0].clone(),
                row[1].clone(),
                row[1].clone(),
            ]
        })
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), 16);
    let before = storage::branch_head(&fixture.db, "main").unwrap();
    let actual = read(
        &fixture.db,
        &format!("PROFILE {text}"),
        BTreeMap::from([("idsList".into(), Value::List(lists))]),
        ExecutionOptions::default(),
        1,
    );
    assert_bag(&actual.rows, &expected);
    assert_eq!(
        actual.columns,
        ["elementId(x)", "elementId(y)", "x.k", "y.k"]
    );
    // Without a costed Join or bound point, install_match_profile_sites keeps
    // this Program on its legacy access sites. Do not accidentally let a new
    // Join/point hide the per-pull legacy next_access_operator advancement.
    for operator in [
        "HashJoin",
        "MultiwayIntersect",
        "FactorizedAccumulate",
        "IndexNestedLoop",
        "NodeIdSeek",
    ] {
        assert!(!actual.plan.contains(operator), "{}", actual.plan);
    }
    let sites = actual
        .profile
        .iter()
        .filter(|operator| operator.semi_mask.is_some())
        .collect::<Vec<_>>();
    assert_eq!(
        sites.len(),
        2,
        "two real finite-read sites: {:?}",
        actual.profile
    );
    assert!(
        sites[0].id < sites[1].id,
        "two distinct physical access slots"
    );
    for site in sites {
        assert_eq!(site.operator, "LabelIndexScan");
        let mask = site.semi_mask.as_ref().unwrap();
        assert_eq!(
            (mask.keys, mask.seeks, site.rows),
            (16, 16, 16),
            "every fresh MATCH keeps its own events: {:?}",
            actual.profile
        );
        assert!(site.db_hits > 0, "both sites execute actual point reads");
    }
    assert_eq!(
        actual
            .profile
            .iter()
            .map(|operator| operator.db_hits)
            .sum::<u64>(),
        actual.db_hits
    );
    assert_pinned(&actual, before);
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), before);
    assert!(fixture.db.is_autocommit());
}
