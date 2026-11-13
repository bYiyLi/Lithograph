use super::*;

fn access(rows: u64) -> AccessEstimate {
    AccessEstimate::stream(Some(CardinalityEstimate::exact(rows)), 0)
}

fn inputs(rows: u64, string: bool) -> HashInputs {
    let layout = crate::query::stream::HashLayoutFacts {
        entry_inline_bytes: 256,
        bucket_bytes: 8,
        binding_bytes: 256,
        key_heap_bytes: (!string).then_some(0),
        encoded_row_bytes: Some(2048),
        encoding_scratch_bytes: 2048,
        headroom_bytes: 8192,
        soft_cap: 1024 * 1024,
    };
    HashInputs {
        candidates: vec![HashCandidate {
            part: 1,
            orientation: PartOrientation::Written,
            equality: 0,
            inner_side: HashOperandSide::Right,
            probe_guard: 1,
            build: HashBuild {
                access: access(rows),
                layout,
                peak: (!string).then_some(rows.saturating_mul(600)),
                key_property: true,
            },
            available: None,
            probe_property: true,
        }],
        ids: vec![Vec::new(), vec![0]],
        dependencies: vec![1, 2],
        encoded: vec![Some(2048), Some(2048)],
        encoding_scratch: vec![2048, 2048],
        keys: vec![KeyScopes {
            left: vec![None; 2],
            right: vec![None; 2],
        }],
        incoming_unknown: false,
        output_upper: None,
        dynamic: Vec::new(),
    }
}

fn prefix(rows: u64) -> Cost {
    Cost {
        rows: Some(CardinalityEstimate::exact(rows)),
        ..Cost::root()
    }
}

#[test]
fn large_no_index_numeric_and_unknown_string_width_have_real_hash_consumers() {
    for string in [false, true] {
        let inputs = inputs(100, string);
        assert!(inputs.allowed(0, 1));
        let outer = prefix(100);
        let rows = Some(expectation(100, 10_000));
        let hash = inputs.estimate(0, 1, outer).unwrap();
        assert_eq!(hash.matches.expected, 10_000, "no invented NDV");
        let hashed = hash.append(outer, rows);
        let mut nl = outer.append(access(100));
        nl.rows = rows;
        eprintln!("S5 noIndex string={string} hash={hashed:?} rescan={nl:?}");
        assert!(hashed.beats(nl));
        if string {
            assert!(hashed.temp_bytes.unwrap() > 0);
            assert_eq!(
                hash.resident,
                1024 * 1024 + crate::query::spill::grace_resident_bound()
            );
        }
    }
}

#[test]
fn one_outer_and_ready_inl_still_win_without_a_row_count_cutoff() {
    let candidate_inputs = inputs(100, false);
    let outer = prefix(1);
    let output = Some(expectation(100, 100));
    let hash = candidate_inputs
        .estimate(0, 1, outer)
        .unwrap()
        .append(outer, output);
    let mut nl = outer.append(access(100));
    nl.rows = output;
    assert!(nl.beats(hash));
    let candidate_inputs = inputs(10_000, false);
    let outer = prefix(10);
    let output = Some(expectation(10, 100_000));
    let hash = candidate_inputs
        .estimate(0, 1, outer)
        .unwrap()
        .append(outer, output);
    let mut seek = outer.append(AccessEstimate::stream(Some(expectation(1, 1)), 2));
    seek.rows = output;
    assert!(seek.beats(hash));
}

#[test]
fn key_matches_do_not_become_the_residual_output() {
    let inputs = inputs(100, false);
    let outer = prefix(100);
    let hash = inputs.estimate(0, 1, outer).unwrap();
    let output = hash.append(outer, Some(CardinalityEstimate::exact(1)));
    assert_eq!(output.rows.unwrap().expected, 1);
    assert!(output.cpu.unwrap() >= 20_000, "all K matches still pay CPU");
}

#[test]
fn any_shared_inner_binding_rejects_an_independent_build() {
    let mut inputs = inputs(100, false);
    assert!(inputs.allowed(0, 1));
    inputs.dependencies[1] |= 1;
    assert!(
        !inputs.allowed(0, 1),
        "shared Node/Relationship is correlated"
    );
    inputs.dependencies[1] = 2;
    inputs.candidates[0].probe_guard = 4;
    assert!(!inputs.allowed(0, 1), "future proof is unavailable");
    assert!(!inputs.allowed(0, 0), "no speculative pre-outer build");
}

#[test]
fn default_soft_target_is_per_owner_but_quota_has_shared_headroom() {
    let mut inputs = inputs(100, false);
    let mut outer = prefix(100);
    outer.memory = 1024 * 1024;
    assert!(
        inputs.estimate(0, 1, outer).is_some(),
        "NoLimit is not a second quota"
    );
    inputs.candidates[0].available = Some(1024 * 1024);
    assert!(inputs.estimate(0, 1, outer).is_none());
}

#[test]
fn ample_hard_grant_keeps_the_soft_target_after_prefix_and_df_claims() {
    let mut inputs = inputs(1500, false);
    let candidate = &mut inputs.candidates[0];
    let limit = 512 * 1024 * 1024;
    candidate.available = Some(limit - candidate.build.layout.headroom_bytes);
    let resources = crate::query::QueryResources::new(Some(limit));
    let _prefix = resources.reserve(2048).unwrap();
    let _filter = resources.reserve(163 * 1024).unwrap();
    let runtime = crate::query::stream::hash_resident_target(
        &resources,
        0,
        0,
        candidate.build.layout.headroom_bytes,
    );
    assert_eq!(runtime, candidate.build.layout.soft_cap);
    assert_eq!(candidate.resident_target(resources.used(), false), runtime);
    let mut prefix = prefix(1000);
    prefix.memory = 2048;
    let original = inputs.estimate(0, 1, prefix).unwrap();
    let filtered = inputs
        .estimate_with_memory(0, 1, prefix, 163 * 1024)
        .unwrap();
    assert_eq!(original.temp.owner, 0);
    assert_eq!(
        filtered.temp.owner, 0,
        "DF must not subtract from an already clamped soft cap"
    );
    assert_eq!(filtered.resident, original.resident);
}

#[test]
fn tight_hard_grant_subtracts_the_same_real_claims_before_the_soft_cap() {
    let mut inputs = inputs(1500, false);
    let candidate = &mut inputs.candidates[0];
    let limit = 512 * 1024;
    candidate.available = Some(limit - candidate.build.layout.headroom_bytes);
    let resources = crate::query::QueryResources::new(Some(limit));
    let _prefix = resources.reserve(2048).unwrap();
    let before = candidate.resident_target(resources.used(), false);
    let _filter = resources.reserve(163 * 1024).unwrap();
    let runtime = crate::query::stream::hash_resident_target(
        &resources,
        0,
        0,
        candidate.build.layout.headroom_bytes,
    );
    assert_eq!(candidate.resident_target(resources.used(), false), runtime);
    assert_eq!(before - runtime, 163 * 1024);
    let mut prefix = prefix(1000);
    prefix.memory = 2048;
    let original = inputs.estimate(0, 1, prefix).unwrap();
    let filtered = inputs
        .estimate_with_memory(0, 1, prefix, 163 * 1024)
        .unwrap();
    assert!(original.temp.owner > 0 && filtered.temp.owner > 0);
    assert!(filtered.resident < original.resident);
    assert!(original.resident <= limit && filtered.resident <= limit);
}

#[test]
fn observed_skew_and_real_routing_depth_are_retained() {
    let mut inputs = inputs(1_000_000, false);
    let advice = HashKeyAdvice {
        providers: 2,
        indexed_owners: 1_000_000,
        ndistinct: expectation(900_001, 1_000_000),
        max_frequency: expectation(100_000, 1_000_000),
        max_observed_frequency: 32,
        unique: false,
    };
    inputs.keys[0].right[1] = Some(advice);
    let (passes, hot) = routing(&inputs.candidates[0], Some(&advice), 1024 * 1024);
    assert_eq!(passes, 4, "root plus three actual repartitions");
    assert!(hot >= 49, "observed large bucket is not uniform");
    let hash = inputs.estimate(0, 1, prefix(1_000_000)).unwrap();
    assert!(hash.temp.owner > 1_000_000 * 2048);
    assert!(hash.temp.per_probe > 2048 * 8);
}

#[test]
fn prefix_payload_uses_all_real_part_widths() {
    let mut inputs = inputs(10, true);
    inputs.encoded.push(Some(8192));
    inputs.encoding_scratch.push(8192);
    assert_eq!(inputs.prefix_bytes(0b101), Some(10_240));
    assert_eq!(inputs.prefix_bytes(0b010), Some(2048));
    assert_eq!(inputs.prefix_scratch(0b101), 10_240);
    let hash = inputs.estimate(0, 0b101, prefix(100)).unwrap();
    assert!(hash.temp.per_probe >= 20_480);
}

#[test]
fn logical_traffic_and_serialization_scratch_are_independent_cost_facts() {
    let mut inputs = inputs(10_000, false);
    let wide = inputs.estimate(0, 1, prefix(1000)).unwrap();
    inputs.encoded = vec![Some(64), Some(64)];
    inputs.candidates[0].build.layout.encoded_row_bytes = Some(64);
    let compact = inputs.estimate(0, 1, prefix(1000)).unwrap();
    assert_eq!(inputs.prefix_scratch(1), 2048);
    assert_eq!(
        compact.resident, wide.resident,
        "compact JSON does not release Rust scratch"
    );
    assert!(compact.temp.owner < wide.temp.owner);
    assert!(compact.temp.per_probe < wide.temp.per_probe);
    inputs.encoded[0] = None;
    assert!(
        inputs.estimate(0, 1, prefix(1000)).is_none(),
        "unknown prefix payload rejects only Hash advice"
    );
}

fn metadata() -> (MatchStep, JoinAnalysis, JoinEstimates) {
    let parts = ["left", "right"]
        .into_iter()
        .map(|variable| PatternPart {
            path_variable: None,
            start: NodeSpec {
                variable: Some(variable.into()),
                labels: Vec::new(),
                label_names: Vec::new(),
                scan_label: None,
                scan_label_name: None,
                index_seek: None,
                id_seek: None,
                impossible: false,
            },
            relationship: None,
            end: None,
        })
        .collect();
    let key = |variable: &str, mask| HashInputKey {
        variable: variable.into(),
        property: Some("k".into()),
        providers: PartSet(mask),
    };
    (
        MatchStep {
            clause_start: 0,
            optional: false,
            parts,
            predicate: None,
            early_predicates: Vec::new(),
            join: None,
        },
        JoinAnalysis {
            predicates: Vec::new(),
            templates: Vec::new(),
            edges: Vec::new(),
            hashes: vec![HashEquality {
                predicate: 0,
                left: key("left", 1),
                right: key("right", 2),
                family: HashFamily::Number,
            }],
        },
        JoinEstimates {
            bound: vec![(None, None); 2],
            bound_sources: vec![BoundSources::default(); 2],
            parts: (0..2)
                .map(|_| PartEstimate {
                    written: access(128),
                    reversed: None,
                    output: access(128).rows,
                })
                .collect(),
            ..JoinEstimates::default()
        },
    )
}

#[test]
fn optional_hash_metadata_never_spends_the_remaining_greedy_budget() {
    let (step, analysis, estimates) = metadata();
    let scope = JoinClauseScope {
        complete_consumer: true,
        clause_start: 0,
        snapshot_identity: HashId::from_bytes([0; 32]),
        incoming: &[],
        declared_columns: &[],
    };
    let mut budget = PlanningBudget::default();
    budget.states = crate::query::plan::budget::MAX_STATES - 1;
    let before = (budget.states, budget.candidates, budget.bytes);
    assert!(!admit(
        &step,
        &analysis,
        &estimates,
        0,
        &mut budget,
        128,
        &scope
    ));
    assert!(!budget.exhausted());
    assert_eq!(before, (budget.states, budget.candidates, budget.bytes));
    budget.states = 0;
    budget.candidates = crate::query::plan::budget::MAX_CANDIDATES;
    assert!(!admit(
        &step,
        &analysis,
        &estimates,
        0,
        &mut budget,
        128,
        &scope
    ));
    assert!(!budget.exhausted());
    let mut grant = PlanningBudget::default();
    assert!(admit(
        &step, &analysis, &estimates, 0, &mut grant, 128, &scope
    ));
    budget.candidates = 0;
    budget.bytes = crate::query::plan::budget::MAX_BYTES - grant.bytes;
    let before = (budget.states, budget.candidates, budget.bytes);
    assert!(!admit(
        &step,
        &analysis,
        &estimates,
        0,
        &mut budget,
        128,
        &scope
    ));
    assert!(!budget.exhausted());
    assert_eq!(
        budget.states,
        before.0 + step.parts.len() + analysis.hashes.len()
    );
    assert_eq!((budget.candidates, budget.bytes), (before.1, before.2));
}

#[test]
fn unknown_incoming_payload_preserves_memory_hash_but_not_a_fake_grace_width() {
    let mut inputs = inputs(100, false);
    inputs.incoming_unknown = true;
    assert!(inputs.estimate(0, 1, prefix(100)).is_some());
    inputs.candidates[0].build.peak = None;
    assert!(inputs.estimate(0, 1, prefix(100)).is_none());
}

#[test]
fn local_residual_does_not_shrink_the_retained_build_input() {
    let (_, mut analysis, mut estimates) = metadata();
    let raw = inputs(128, false);
    analysis.predicates.push(JoinPredicate {
        expression: Expr::Literal(Value::Boolean(false)),
        guards: vec![PartSet(2)],
    });
    collect_filters(&analysis, &mut estimates);
    assert_eq!(estimates.parts[1].written.rows.unwrap().expected, 0);
    assert_eq!(raw.candidates[0].build.access.rows.unwrap().expected, 128);
    assert_eq!(
        raw.estimate(0, 1, prefix(10)).unwrap().matches.expected,
        1280
    );
}
