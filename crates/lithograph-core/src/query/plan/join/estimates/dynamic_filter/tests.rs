use super::*;
use crate::query::plan::join::cost::{AccessEstimate, HashEstimate, TempWork, Work, expectation};

fn input(n: u64, distinct: u64) -> DynamicInput {
    DynamicInput {
        build: 1,
        probe: 0,
        equality: 0,
        inner_side: HashOperandSide::Right,
        template: 0,
        build_rows: CardinalityEstimate::exact(1024),
        probe_rows: CardinalityEstimate::exact(n),
        distinct: CardinalityEstimate {
            expected: distinct,
            lower: 3,
            upper: 1024,
        },
    }
}

fn prefix(n: u64) -> Cost {
    Cost::root().append(AccessEstimate::stream(
        Some(CardinalityEstimate::exact(n)),
        0,
    ))
}

#[test]
fn first_physical_page_and_all_advised_distinct_seeks_are_paid() {
    let input = input(1_000_000, 96);
    let cost = input
        .prefix(prefix(1_000_000), input.workspace().unwrap())
        .unwrap();
    assert_eq!(
        cost.rows.unwrap(),
        CardinalityEstimate {
            expected: 97,
            lower: 1,
            upper: 1025
        }
    );
    assert_eq!(cost.io, Some(1 + 96 * 2 + 96));
    assert!(cost.cpu.unwrap() >= (1024 * 3 + 96 * 19) / 2 + 96 * 7);
    assert_eq!(cost.memory, 2048 + 1024 * 96 + 512 + 65536);
}

#[test]
fn remaining_upper_uses_unread_owners_instead_of_expected_selectivity() {
    let base = input(1000, 96);
    let cost = base
        .prefix(prefix(1000), base.workspace().unwrap())
        .unwrap();
    assert_eq!(cost.rows.unwrap().upper, 1000);
    let tiny = input(12, 96);
    let cost = tiny.prefix(prefix(12), tiny.workspace().unwrap()).unwrap();
    assert_eq!(
        cost.rows.unwrap(),
        CardinalityEstimate {
            expected: 12,
            lower: 1,
            upper: 12
        }
    );
    assert_eq!(
        cost.io,
        Some(12 + 96 * 2),
        "even keys before H pay seek startup"
    );
    let singleton = input(1, 96);
    let cost = singleton
        .prefix(prefix(1), singleton.workspace().unwrap())
        .unwrap();
    assert_eq!(cost.rows.unwrap(), CardinalityEstimate::exact(1));
}

#[test]
fn workspace_uses_full_distinct_upper_not_sampled_expected_or_recipe_three() {
    let small = input(1_000_000, 3);
    let many = input(1_000_000, 96);
    assert_eq!(small.workspace(), many.workspace());
    let small_cost = small
        .prefix(prefix(1_000_000), small.workspace().unwrap())
        .unwrap();
    let many_cost = many
        .prefix(prefix(1_000_000), many.workspace().unwrap())
        .unwrap();
    assert!(small_cost.cpu < many_cost.cpu);
    assert!(small_cost.io < many_cost.io);
}

#[test]
fn unknown_or_non_scan_prefix_cannot_be_subtracted_into_a_cheap_frontier() {
    let input = input(1000, 96);
    let mut cost = prefix(1000);
    cost.io = None;
    assert!(input.prefix(cost, 1).is_none());
    let mut cost = prefix(1000);
    cost.cpu = Some(1);
    assert!(input.prefix(cost, 1).is_none());
    let mut cost = prefix(1000);
    cost.rows = Some(expectation(1000, 2000));
    assert!(input.prefix(cost, 1).is_none());
}

#[test]
fn one_owner_frontier_has_real_savings_but_a_broad_keyset_still_prefers_inl() {
    let input = input(1_000_000, 96);
    let matches = expectation(1024, 1024);
    let output = Some(matches);
    let hash = HashEstimate {
        owner: Work {
            io: 2048,
            cpu: 5120,
        },
        run: Work::default(),
        probe: Work { io: 1, cpu: 3 },
        matches,
        resident: 100_000,
        temp: TempWork::default(),
    };
    let filtered_prefix = input
        .prefix(prefix(1_000_000), input.workspace().unwrap())
        .unwrap();
    let dynamic = hash.append(filtered_prefix, output);
    assert!(dynamic.beats(hash.append(prefix(1_000_000), output)));
    let mut seek = AccessEstimate::stream(Some(expectation(1, 1)), 2);
    seek.io = seek.io.map(|io| io + 1);
    seek.cpu = seek.cpu.map(|cpu| cpu + 3);
    let mut inl = prefix(1024).append(seek);
    inl.rows = output;
    assert!(
        dynamic.beats(inl),
        "reading one prefix owner removes 255 owners of real scan/probe work"
    );
    let broad = DynamicInput {
        distinct: CardinalityEstimate::exact(1024),
        ..input
    };
    let broad_prefix = broad
        .prefix(prefix(1_000_000), broad.workspace().unwrap())
        .unwrap();
    assert!(
        inl.beats(hash.append(broad_prefix, output)),
        "a complete broad KeySet does not force INL out"
    );
    assert!(
        dynamic.cpu.unwrap() >= 2 * 1024,
        "all K merges remain charged"
    );
}
