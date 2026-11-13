//! An incomplete consumer cannot pay a full-clause accumulation cost.
use super::*;

fn prefix(fixture: &AspFixture, query: &str, expected: usize, take: i64) {
    let mut bindings = params();
    bindings.insert("take".into(), Value::Integer(take));
    let result = read(
        &fixture.star.db,
        &format!("PROFILE {query}"),
        bindings,
        ExecutionOptions::default(),
        1,
    );
    assert_eq!(result.rows.len(), expected);
    assert!(
        !result.plan.contains("FactorizedAccumulate"),
        "{}",
        result.plan
    );
    assert!(!result.plan.contains("ASPJoin"), "{}", result.plan);
    let mut remaining = fixture.expected.clone();
    for row in &result.rows {
        let position = remaining
            .iter()
            .position(|candidate| candidate == row)
            .expect("partial output must be a sub-bag of the public CREATE oracle");
        remaining.swap_remove(position);
    }
    if expected == 0 {
        assert!(
            result
                .profile
                .iter()
                .all(|op| op.db_hits == 0 && op.rows == 0)
        );
    }
}

#[test]
fn literal_parameter_and_offset_limits_preserve_the_streaming_choice() {
    let fixture = AspFixture::new();
    for take in [0, 1, 10] {
        prefix(
            &fixture,
            &format!("{ASP} LIMIT {take}"),
            take as usize,
            take,
        );
        prefix(&fixture, &format!("{ASP} LIMIT $take"), take as usize, take);
    }
    prefix(&fixture, &format!("{ASP} SKIP 3 LIMIT $take"), 10, 10);
}

#[test]
fn with_limit_cannot_borrow_full_output_costs_from_a_later_return() {
    let fixture = AspFixture::new();
    let query = ASP.replace(" RETURN ", " WITH h,r,a,s,b,d LIMIT $take RETURN ");
    prefix(&fixture, &query, 1, 1);
    prefix(&fixture, &query, 10, 10);
}

#[test]
fn genuine_sort_and_aggregate_consumers_still_use_complete_costs() {
    let fixture = AspFixture::new();
    let ordered = read(
        &fixture.star.db,
        &format!("PROFILE {ASP} ORDER BY elementId(r),elementId(s) LIMIT 3"),
        params(),
        ExecutionOptions::default(),
        1,
    );
    assert_eq!(ordered.rows.len(), 3);
    actual_operator(&ordered, "ASPJoin");
    let query = ASP.split(" RETURN ").next().unwrap().to_owned() + " RETURN count(*)";
    let aggregated = read(
        &fixture.star.db,
        &format!("PROFILE {query}"),
        params(),
        ExecutionOptions::default(),
        1,
    );
    assert_eq!(aggregated.rows, vec![vec![Value::Integer(256)]]);
    actual_operator(&aggregated, "ASPJoin");
    // This projection already used the eager program fallback before the
    // LIMIT fix: preserve its result, without inventing an optimizer promise.
    let limited = read(
        &fixture.star.db,
        &format!("PROFILE {query} LIMIT 1"),
        params(),
        ExecutionOptions::default(),
        1,
    );
    assert_eq!(limited.rows, aggregated.rows);
    assert!(!limited.plan.contains("ASPJoin"));
}
