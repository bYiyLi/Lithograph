//! Group consumers retain ordinary typed bags and per-row boundary semantics.

use std::collections::{BTreeMap, BTreeSet};

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, FactorizedRuntimeMetrics, QueryErrorKind};
use lithograph_core::storage;

use super::support::{self, Observed, StarFixture, actual_operator, assert_bag, params, read};

const PREFIX: &str = "MATCH (h:Hub)-[r:A]->(a:LeafA),(h)-[s:B]->(b:LeafB) WHERE h.k=$key";
const IDS: &str = "elementId(h),elementId(r),elementId(a),elementId(s),elementId(b)";

fn typed_star(left: usize, right: usize) -> StarFixture {
    let fixture = StarFixture::new(left, right);
    for ddl in [
        "CREATE CONSTRAINT s7_leaf_a_seq FOR(a:LeafA) REQUIRE a.seq IS :: INTEGER",
        "CREATE CONSTRAINT s7_leaf_b_seq FOR(b:LeafB) REQUIRE b.seq IS :: INTEGER",
        "CREATE CONSTRAINT s7_leaf_a_gap FOR(a:LeafA) REQUIRE a.gap IS :: INTEGER",
    ] {
        support::write(&fixture.db, ddl);
    }
    support::checkpoint(&fixture.db);
    fixture
}

fn profile(fixture: &StarFixture, text: &str) -> Observed {
    read(
        &fixture.db,
        &format!("PROFILE {text}"),
        params(),
        ExecutionOptions::default(),
        3,
    )
}

fn require_group(result: &Observed) -> &FactorizedRuntimeMetrics {
    let accumulated = actual_operator(result, "FactorizedAccumulate");
    assert!(accumulated.rows > 0, "{:?}", result.profile);
    let stats = accumulated.factorized.as_ref().expect("real group metrics");
    assert!(stats.group_members > 0);
    stats
}

fn require_flat_or_boundary(result: &Observed) {
    if result.plan.contains("FactorizedAccumulate") {
        require_group(result);
        assert!(actual_operator(result, "Flatten").rows > 0);
    } else {
        assert!(
            result
                .profile
                .iter()
                .all(|operator| operator.operator != "FactorizedAccumulate")
        );
    }
}

#[test]
fn a_total_integer_filter_runs_per_arm_before_flat_output() {
    let fixture = typed_star(64, 128);
    let chosen = fixture.left[0].endpoint.clone();
    support::write(&fixture.db, "MATCH(a:LeafA) SET a.seq=0");
    let changed = support::execute(
        &fixture.db,
        "MATCH(a:LeafA) WHERE elementId(a)=$chosen SET a.seq=1 RETURN elementId(a)",
        BTreeMap::from([("chosen".to_owned(), chosen.clone())]),
        ExecutionOptions::default(),
        1,
    )
    .unwrap();
    assert_eq!(changed.rows, vec![vec![chosen.clone()]]);
    support::checkpoint(&fixture.db);
    let baseline = profile(&fixture, &format!("{PREFIX} RETURN {IDS}"));
    let result = profile(
        &fixture,
        &format!("{PREFIX} FILTER a.seq=1 AND a.seq IS NOT NULL RETURN {IDS}"),
    );
    let expected = fixture
        .expected_rows()
        .into_iter()
        .filter(|row| row[2] == chosen)
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), 512);
    assert_bag(&baseline.rows, &fixture.expected_rows());
    assert_bag(&result.rows, &expected);
    let before = require_group(&baseline).predicate_evaluations;
    let filtered = require_group(&result).predicate_evaluations;
    // The same slot also owns the producer's shared-key predicate. Isolate the
    // actual additional arm predicate calls without hiding that earlier work.
    assert_eq!(
        filtered.checked_sub(before),
        Some(128),
        "predicate events: baseline={before}, filtered={filtered}"
    );
    assert_eq!(actual_operator(&result, "Flatten").rows, 512);
    assert!(
        result.db_hits <= baseline.db_hits + 1024,
        "an arm predicate must not reread a.seq for 32768 flat tuples: {} / {}",
        result.db_hits,
        baseline.db_hits
    );
}

#[test]
fn composed_renames_and_element_ids_keep_the_original_input_mapping() {
    let fixture = StarFixture::new(64, 128);
    let text = format!(
        "{PREFIX} \
        WITH h AS root,r AS s,a AS b,s AS r,b AS a \
        WITH elementId(root) AS hub,elementId(s) AS lrel,elementId(b) AS leafA,\
             elementId(r) AS rrel,elementId(a) AS leafB \
        RETURN hub AS h,lrel AS r,leafA AS a,rrel AS s,leafB AS b"
    );
    let result = profile(&fixture, &text);
    assert_eq!(result.columns, ["h", "r", "a", "s", "b"]);
    assert_bag(&result.rows, &fixture.expected_rows());
    assert_eq!(result.rows.len(), 32768);
    require_group(&result);
}

#[test]
fn dropping_an_arm_from_projection_preserves_its_multiplicity() {
    let fixture = StarFixture::new(64, 128);
    let result = profile(
        &fixture,
        &format!(
            "{PREFIX} WITH h,r,a \
        RETURN elementId(h) AS hub,elementId(r) AS rel,elementId(a) AS leaf"
        ),
    );
    let expected = fixture
        .expected_rows()
        .into_iter()
        .map(|row| row[..3].to_vec())
        .collect::<Vec<_>>();
    assert_eq!(result.columns, ["hub", "rel", "leaf"]);
    assert_eq!(result.rows.len(), 32768);
    assert_bag(&result.rows, &expected);
    require_group(&result);
    assert_eq!(actual_operator(&result, "Flatten").rows, 32768);
}

#[test]
fn weighted_count_keeps_nullable_members_and_the_empty_global_row() {
    let fixture = typed_star(64, 128);
    let chosen = fixture.left[0].endpoint.clone();
    let changed = support::execute(
        &fixture.db,
        "MATCH(a:LeafA) WHERE elementId(a)=$chosen SET a.seq=null RETURN elementId(a)",
        BTreeMap::from([("chosen".to_owned(), chosen.clone())]),
        ExecutionOptions::default(),
        1,
    )
    .unwrap();
    assert_eq!(changed.rows, vec![vec![chosen]]);
    support::checkpoint(&fixture.db);
    let text = format!(
        "{PREFIX} RETURN count(*) AS total,count(a.seq) AS nonnull,\
        count(a.gap) AS missing,count(null) AS nulls,count(1) AS ones"
    );
    let result = profile(&fixture, &text);
    assert_eq!(
        result.columns,
        ["total", "nonnull", "missing", "nulls", "ones"]
    );
    assert_eq!(
        result.rows,
        vec![vec![
            Value::Integer(32768),
            Value::Integer(32256),
            Value::Integer(0),
            Value::Integer(0),
            Value::Integer(32768)
        ]]
    );
    let evaluations = require_group(&result).expression_evaluations;
    assert_eq!(actual_operator(&result, "Aggregate").rows, 1);
    // Two A-arm expressions visit 128 members each; the two literal arguments
    // are evaluated once. COUNT(*) needs no argument evaluation.
    assert!((1..=258).contains(&evaluations), "{evaluations}");
    assert!(
        result
            .profile
            .iter()
            .all(|operator| operator.operator != "Flatten" || operator.rows == 0)
    );
    let mut missing = params();
    missing.insert("key".to_owned(), Value::Integer(8));
    let empty = read(
        &fixture.db,
        &format!("PROFILE {text}"),
        missing,
        ExecutionOptions::default(),
        1,
    );
    assert_eq!(empty.rows, vec![vec![Value::Integer(0); 5]]);
    assert_eq!(actual_operator(&empty, "Aggregate").rows, 1);
}

#[test]
fn min_max_and_count_preserve_integer_null_and_empty_aggregate_results() {
    let fixture = typed_star(64, 128);
    let text = format!(
        "{PREFIX} RETURN count(*) AS total,count(a.seq) AS present,\
        min(a.seq) AS minA,max(a.seq) AS maxA,min(b.seq) AS minB,max(b.seq) AS maxB"
    );
    let present = profile(&fixture, &text);
    assert_eq!(
        present.rows,
        vec![vec![
            Value::Integer(32768),
            Value::Integer(32768),
            Value::Integer(1),
            Value::Integer(64),
            Value::Integer(1),
            Value::Integer(128)
        ]]
    );
    support::write(&fixture.db, "MATCH(a:LeafA) SET a.seq=null");
    support::checkpoint(&fixture.db);
    let nulls = profile(&fixture, &text);
    assert_eq!(
        nulls.rows,
        vec![vec![
            Value::Integer(32768),
            Value::Integer(0),
            Value::Null,
            Value::Null,
            Value::Integer(1),
            Value::Integer(128)
        ]]
    );
    let mut bindings = params();
    bindings.insert("key".to_owned(), Value::Integer(8));
    let empty = read(
        &fixture.db,
        &format!("PROFILE {text}"),
        bindings,
        ExecutionOptions::default(),
        1,
    );
    assert_eq!(
        empty.rows,
        vec![vec![
            Value::Integer(0),
            Value::Integer(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null
        ]]
    );
    for folded in [&present, &nulls] {
        assert_eq!(actual_operator(folded, "Aggregate").rows, 1);
        let evaluations = require_group(folded).expression_evaluations;
        // COUNT(a.seq), MIN/MAX(a.seq): 3*128 actual member evaluations;
        // MIN/MAX(b.seq): 2*256. No flat 32768-tuple argument enumeration.
        assert!((1..=896).contains(&evaluations), "{evaluations}");
        assert!(
            folded
                .profile
                .iter()
                .all(|operator| operator.operator != "Flatten" || operator.rows == 0)
        );
    }
    assert_eq!(actual_operator(&empty, "Aggregate").rows, 1);
}

#[test]
fn cross_arm_predicates_flatten_and_keep_error_timing() {
    let fixture = typed_star(2, 2);
    let left = fixture.left[0].endpoint.clone();
    let right = fixture.right[0].endpoint.clone();
    for (label, default, selected) in [("LeafA", 0, &left), ("LeafB", 2, &right)] {
        support::write(
            &fixture.db,
            &format!("MATCH(n:{label}) SET n.seq={default}"),
        );
        let changed = support::execute(
            &fixture.db,
            &format!(
                "MATCH(n:{label}) WHERE elementId(n)=$selected SET n.seq=1 RETURN elementId(n)"
            ),
            BTreeMap::from([("selected".to_owned(), (*selected).clone())]),
            ExecutionOptions::default(),
            1,
        )
        .unwrap();
        assert_eq!(changed.rows, vec![vec![(*selected).clone()]]);
    }
    support::checkpoint(&fixture.db);
    let result = profile(
        &fixture,
        &format!("{PREFIX} FILTER a.seq=b.seq RETURN {IDS}"),
    );
    let expected = fixture
        .expected_rows()
        .into_iter()
        .filter(|row| row[2] == left && row[4] == right)
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), 4);
    assert_bag(&result.rows, &expected);
    require_flat_or_boundary(&result);
    let bad = format!("PROFILE {PREFIX} FILTER 1/(a.seq-b.seq)=0 RETURN {IDS}");
    let head = storage::branch_head(&fixture.db, "main").unwrap();
    {
        let _scope = support::ReadScope::new(&fixture.db);
        let error = support::execute(&fixture.db, &bad, params(), ExecutionOptions::default(), 1)
            .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Type);
    }
    let mut bindings = params();
    bindings.insert("key".to_owned(), Value::Integer(8));
    let empty = read(&fixture.db, &bad, bindings, ExecutionOptions::default(), 1);
    assert!(empty.rows.is_empty());
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), head);
}

#[test]
fn volatile_projection_runs_at_the_flat_row_boundary() {
    let fixture = StarFixture::new(64, 128);
    let result = profile(&fixture, &format!("{PREFIX} RETURN {IDS},rand() AS draw"));
    assert_eq!(result.rows.len(), 32768);
    assert!(result.rows.iter().all(|row| row.len() == 6
        && matches!(row.last(), Some(Value::Float(draw)) if (0.0..1.0).contains(draw))));
    let identities = result
        .rows
        .iter()
        .map(|row| row[..5].to_vec())
        .collect::<Vec<_>>();
    assert_bag(&identities, &fixture.expected_rows());
    require_group(&result);
    assert_eq!(actual_operator(&result, "Flatten").rows, 32768);
    // Independent random executions must not be compared value-for-value.
}

#[test]
fn optional_and_named_path_boundaries_keep_nulls_and_path_identity() {
    let fixture = StarFixture::new(2, 2);
    let optional = profile(
        &fixture,
        &format!(
            "{PREFIX} \
        OPTIONAL MATCH(a)-[t:C]->(z:Absent) RETURN {IDS},elementId(t),elementId(z)"
        ),
    );
    let expected = fixture
        .expected_rows()
        .into_iter()
        .map(|mut row| {
            row.extend([Value::Null, Value::Null]);
            row
        })
        .collect::<Vec<_>>();
    assert_bag(&optional.rows, &expected);
    require_flat_or_boundary(&optional);
    let path = profile(
        &fixture,
        "MATCH p=(h:Hub)-[r:A]->(a:LeafA),(h)-[s:B]->(b:LeafB) \
        WHERE h.k=$key RETURN elementId(h),elementId(r),elementId(a),elementId(s),elementId(b),length(p)",
    );
    let expected = fixture
        .expected_rows()
        .into_iter()
        .map(|mut row| {
            row.push(Value::Integer(1));
            row
        })
        .collect::<Vec<_>>();
    assert_bag(&path.rows, &expected);
    assert!(!path.plan.contains("FactorizedAccumulate"), "{}", path.plan);
}

#[test]
fn ordered_skip_limit_and_program_projection_errors_use_flat_semantics() {
    let fixture = StarFixture::new(2, 2);
    let result = profile(
        &fixture,
        &format!(
            "{PREFIX} RETURN {IDS} \
        ORDER BY elementId(r),elementId(s) SKIP 3 LIMIT 5"
        ),
    );
    let mut expected = fixture.expected_rows();
    let id = |value: &Value| match value {
        Value::String(id) => id.clone(),
        _ => panic!("captured ID"),
    };
    expected.sort_by_key(|row| (id(&row[1]), id(&row[3])));
    assert_eq!(
        result.rows,
        expected.into_iter().skip(3).take(5).collect::<Vec<_>>()
    );
    require_flat_or_boundary(&result);
    let bad = format!("PROFILE {PREFIX} WITH 1/(a.seq-a.seq) AS bad SKIP 16 RETURN bad");
    {
        let _scope = support::ReadScope::new(&fixture.db);
        let error = support::execute(&fixture.db, &bad, params(), ExecutionOptions::default(), 1)
            .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Type);
    }
    let mut bindings = params();
    bindings.insert("key".to_owned(), Value::Integer(8));
    assert!(
        read(&fixture.db, &bad, bindings, ExecutionOptions::default(), 1)
            .rows
            .is_empty()
    );
}

#[test]
fn every_flat_tuple_performs_one_write_and_failed_output_rolls_it_back() {
    let fixture = StarFixture::new(2, 2);
    let head = storage::branch_head(&fixture.db, "main").unwrap();
    let text = format!(
        "PROFILE {PREFIX} CREATE(mark:S7Written {{left:elementId(r),right:elementId(s)}}) \
        RETURN {IDS},elementId(mark)"
    );
    let result =
        support::execute(&fixture.db, &text, params(), ExecutionOptions::default(), 3).unwrap();
    assert_eq!(result.counters.nodes_created, 16);
    assert_eq!(result.counters.properties_set, 32);
    assert!(result.commit.is_some());
    assert_ne!(storage::branch_head(&fixture.db, "main").unwrap(), head);
    assert!(result.rows.iter().all(|row| row.len() == 6
        && matches!(row.last(), Some(Value::String(id)) if id.starts_with("n:"))));
    let ids = result
        .rows
        .iter()
        .map(|row| format!("{:?}", row[5]))
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), 16);
    let identities = result
        .rows
        .iter()
        .map(|row| row[..5].to_vec())
        .collect::<Vec<_>>();
    assert_bag(&identities, &fixture.expected_rows());
    let preserved = storage::branch_head(&fixture.db, "main").unwrap();
    let bad = format!("{PREFIX} CREATE(:S7Aborted) RETURN 1/0");
    let error =
        support::execute(&fixture.db, &bad, params(), ExecutionOptions::default(), 1).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Type);
    assert_eq!(
        storage::branch_head(&fixture.db, "main").unwrap(),
        preserved
    );
    let absent = read(
        &fixture.db,
        "MATCH(n:S7Aborted) RETURN elementId(n)",
        BTreeMap::new(),
        ExecutionOptions::default(),
        1,
    );
    assert!(absent.rows.is_empty());
}
