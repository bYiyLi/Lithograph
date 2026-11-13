use super::*;

fn node(name: &str) -> NodeSpec {
    NodeSpec {
        variable: Some(name.into()),
        labels: vec![],
        label_names: vec![],
        scan_label: None,
        scan_label_name: None,
        index_seek: None,
        id_seek: None,
        impossible: false,
    }
}

fn fixture() -> (MatchStep, JoinAnalysis, Vec<String>) {
    let mut parts = Vec::new();
    for part in 0..3 {
        parts.push(PatternPart {
            path_variable: None,
            start: node(&format!("n{part}")),
            end: Some(node(&format!("n{}", (part + 1) % 3))),
            relationship: Some(crate::query::plan::RelationshipSpec {
                variable: Some(format!("r{part}")),
                type_id: Some(part + 1),
                type_name: Some(format!("R{part}")),
                direction: Direction::Outgoing,
                index_seek: None,
                id_seek: None,
                impossible: false,
            }),
        });
    }
    parts.push(PatternPart {
        path_variable: None,
        start: node("tail"),
        end: None,
        relationship: None,
    });
    let key = |variable: &str, providers| HashInputKey {
        variable: variable.into(),
        property: Some("k".into()),
        providers: PartSet(providers),
    };
    let templates = (0..3)
        .flat_map(|part| {
            [ProbeTarget::StartNode, ProbeTarget::EndNode].map(|target| ParameterTemplate {
                part,
                target,
                source_guard: PartSet(1),
                access: ProbeAccess::Adjacency {
                    source_variable: "source".into(),
                },
            })
        })
        .collect();
    let analysis = JoinAnalysis {
        predicates: vec![JoinPredicate {
            expression: Expr::Literal(Value::Boolean(true)),
            guards: vec![PartSet(4), PartSet(8)],
        }],
        edges: vec![],
        templates,
        hashes: vec![HashEquality {
            predicate: 0,
            left: key("r2", 4),
            right: key("tail", 8),
            family: HashFamily::Number,
        }],
    };
    let columns = ["n0", "r0", "n1", "r1", "n2", "r2", "tail"]
        .map(str::to_owned)
        .to_vec();
    let step = MatchStep {
        clause_start: 0,
        optional: false,
        parts,
        predicate: None,
        early_predicates: vec![],
        join: None,
    };
    (step, analysis, columns)
}

fn scope(columns: &[String]) -> JoinClauseScope<'_> {
    JoinClauseScope {
        complete_consumer: true,
        clause_start: 0,
        snapshot_identity: HashId::from_bytes([0; 32]),
        incoming: &[],
        declared_columns: columns,
    }
}

fn estimates() -> JoinEstimates {
    let access = AccessEstimate::stream(Some(CardinalityEstimate::exact(16)), 0);
    JoinEstimates {
        parts: (0..4)
            .map(|_| PartEstimate {
                written: access,
                reversed: Some(access),
                output: access.rows,
            })
            .collect(),
        variables: (0..7)
            .map(|_| VariableDomain {
                providers: 1,
                count: Some(16),
            })
            .collect(),
        ..JoinEstimates::default()
    }
}

#[test]
fn no_hash_advice_preserves_the_original_work_and_headroom() {
    let (step, mut analysis, columns) = fixture();
    analysis.hashes.clear();
    assert_eq!(advice_headroom_work(&step, &analysis), 0);
    assert_eq!(
        advice_headroom(&step, &analysis, &scope(&columns), 276),
        276
    );
}

#[test]
fn samples_leave_the_exact_hash_entry_boundary_after_bound_source_work() {
    let (step, analysis, columns) = fixture();
    let scope = scope(&columns);
    let estimates = estimates();
    let base = 276;
    let reserve = advice_headroom(&step, &analysis, &scope, base);
    assert_eq!(reserve, 974);
    assert!(advice_headroom_work(&step, &analysis) > 0);
    for available in [reserve, reserve - 1] {
        let mut budget = PlanningBudget::default();
        assert!(budget.work(crate::query::plan::budget::MAX_STATES - available, 0));
        // The last sample is followed by six real bound-source ledger debits.
        assert!(budget.work(6, 0));
        let before = budget.states;
        let accepted = admit(&step, &analysis, &estimates, 0, &mut budget, base, &scope);
        assert_eq!(accepted, available == reserve);
        assert!(!budget.exhausted());
        if accepted {
            assert_eq!(budget.remaining_states(), base + 592);
        } else {
            assert_eq!(budget.states - before, 12, "only the actual inspection ran");
        }
    }
}

#[test]
fn index_and_filter_bounds_leave_hash_entry_without_repeating_the_advice_reserve() {
    let (step, mut analysis, columns) = fixture();
    for index in ["left_index", "right_index"] {
        analysis.templates.push(ParameterTemplate {
            part: 3,
            target: ProbeTarget::StartNode,
            source_guard: PartSet(4),
            access: ProbeAccess::IndexEquality {
                source_variable: "r2".into(),
                source_property: "k".into(),
                index_name: index.into(),
                kind: crate::storage::StandardIndexKind::Range,
                definition_hash: HashId::from_bytes([0; 32]),
            },
        });
    }
    analysis.predicates.push(JoinPredicate {
        expression: Expr::Literal(Value::Boolean(false)),
        guards: vec![PartSet(8)],
    });
    let scope = scope(&columns);
    let mut estimates = estimates();
    estimates.equality_keys = 2;
    estimates.equalities.push(EqualityDomain {
        left: 4,
        right: 8,
        distinct: 16,
        left_key: 0,
        right_key: 1,
    });
    estimates.filters.push(LocalFilter {
        providers: 8,
        divisor: 1,
        empty: true,
    });
    let reserve = advice_headroom(&step, &analysis, &scope, 276);
    let mut budget = PlanningBudget::default();
    assert!(budget.work(crate::query::plan::budget::MAX_STATES - reserve, 0));
    assert!(
        budget.work(8, 0),
        "six bound-source and two distribution memo debits"
    );
    assert!(admit(
        &step,
        &analysis,
        &estimates,
        2,
        &mut budget,
        276,
        &scope
    ));
    assert!(!budget.exhausted());
    assert_eq!(post_sample_work(4, 2, 6, 2), 27);
}

#[test]
fn endpoint_comparisons_and_fact_capacity_are_prepaid_before_byte_admission() {
    let (mut step, analysis, columns) = fixture();
    let scope = scope(&columns);
    let estimates = estimates();
    let baseline = collection_bytes(&step, 1).0;
    let ordinary = step.parts.len() * (96 + std::mem::size_of::<Option<u64>>())
        + std::mem::size_of::<Vec<u64>>();
    let facts = 3 * step.parts.len() * std::mem::size_of::<(u128, u64)>();
    assert_eq!(collection_bytes(&step, 0).0, ordinary + facts + 6);
    step.parts[0].start.variable = Some("s".repeat(257));
    step.parts[0].end.as_mut().unwrap().variable = Some("s".repeat(257));
    let (bytes, _) = collection_bytes(&step, 1);
    assert_eq!(
        bytes - baseline,
        255,
        "the endpoint comparison prepays min(nameLen)"
    );
    let required = bytes
        + greedy_storage(
            step.parts.len(),
            &analysis,
            &scope,
            greedy_candidates(step.parts.len(), &analysis),
        );
    for available in [required, required - 1] {
        let mut budget = PlanningBudget::default();
        assert!(budget.work(0, crate::query::plan::budget::MAX_BYTES - available));
        let before = budget.bytes;
        assert_eq!(
            admit(&step, &analysis, &estimates, 0, &mut budget, 276, &scope),
            available == required
        );
        assert!(!budget.exhausted());
        assert_eq!(
            budget.bytes - before,
            if available == required { bytes } else { 0 }
        );
    }
}
