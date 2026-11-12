use super::*;

fn estimates(parts: usize) -> JoinEstimates {
    JoinEstimates {
        parts: (0..parts)
            .map(|_| {
                let rows = Some(CardinalityEstimate::exact(100));
                PartEstimate {
                    written: AccessEstimate::stream(rows, 0),
                    reversed: None,
                    output: rows,
                }
            })
            .collect(),
        equality_keys: parts,
        ..JoinEstimates::default()
    }
}

fn equality(left: usize, right: usize) -> EqualityDomain {
    EqualityDomain {
        left: 1 << left,
        right: 1 << right,
        left_key: left,
        right_key: right,
        distinct: 100,
    }
}

#[test]
fn equality_cycles_only_apply_independent_selectivity_in_each_prefix() {
    let mut stats = estimates(3);
    stats.equalities = vec![equality(0, 1), equality(1, 2), equality(2, 0)];
    assert_eq!(stats.rows(0b111).unwrap().expected, 100);
    for prefix in [0b011, 0b101, 0b110] {
        assert_eq!(stats.rows(prefix).unwrap().expected, 100);
    }
}

#[test]
fn reverse_templates_for_different_property_names_do_not_square_selectivity() {
    // The key IDs identify (a,x) and (b,y), independently of template direction.
    let mut stats = estimates(2);
    stats.equalities = vec![equality(0, 1), equality(1, 0)];
    assert_eq!(stats.rows(3).unwrap().expected, 100);
}

#[test]
fn unknown_domains_do_not_become_zero_or_require_unbound_variables() {
    let mut stats = estimates(2);
    stats.variables.push(VariableDomain {
        providers: 2,
        count: None,
    });
    assert_eq!(stats.rows(1).unwrap().expected, 100);
    stats.parts[1].output = None;
    assert!(stats.rows(3).is_none());
}

fn part(variable: &str) -> PatternPart {
    PatternPart {
        path_variable: None,
        relationship: None,
        end: None,
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
    }
}

fn schema() -> crate::storage::SchemaState {
    use crate::storage::{IndexDefinition, IndexTarget, StandardIndexKind};
    let mut schema = crate::storage::SchemaState::default();
    for (name, variable, property) in [
        ("ax", "a", "x"),
        ("by", "b", "y"),
        ("az", "a", "z"),
        ("bw", "b", "w"),
    ] {
        schema.indexes.insert(
            name.into(),
            IndexDefinition {
                name: name.into(),
                kind: StandardIndexKind::Range,
                target: IndexTarget::NodeProperties {
                    label: variable.into(),
                    properties: vec![property.into()],
                },
                owning_constraint: None,
                labels_or_types: Vec::new(),
                additional_properties: Vec::new(),
                configuration: None,
            },
        );
    }
    schema
}

fn template(
    part: usize,
    source: &str,
    property: &str,
    index: &str,
    providers: u128,
) -> ParameterTemplate {
    ParameterTemplate {
        part,
        target: ProbeTarget::StartNode,
        source_guard: PartSet(providers),
        access: ProbeAccess::IndexEquality {
            source_variable: source.into(),
            source_property: property.into(),
            index_name: index.into(),
            kind: crate::storage::StandardIndexKind::Range,
            definition_hash: HashId::from_bytes([0; 32]),
        },
    }
}

fn sample(distinct: u64) -> Option<crate::query::schema::IndexDistribution> {
    Some(crate::query::schema::IndexDistribution {
        indexed_owners: distinct,
        ndistinct: CardinalityEstimate::exact(distinct),
        max_frequency: CardinalityEstimate::exact(1),
        max_observed_frequency: 1,
        sampled_owners: 32,
        generation_id: 1,
        generation_anchor: HashId::from_bytes([0; 32]),
        definition_hash: HashId::from_bytes([1; 32]),
    })
}

fn step(parts: Vec<PatternPart>) -> MatchStep {
    MatchStep {
        clause_start: 0,
        parts,
        optional: false,
        predicate: None,
        early_predicates: Vec::new(),
        join: None,
    }
}

#[test]
fn actual_templates_merge_both_operand_properties_providers_and_ndv() {
    let step = step(vec![part("a"), part("b"), part("a")]);
    let analysis = JoinAnalysis {
        hashes: Vec::new(),
        predicates: Vec::new(),
        edges: Vec::new(),
        templates: vec![
            template(0, "b", "y", "ax", 2),
            template(1, "a", "x", "by", 5),
            template(2, "b", "y", "ax", 2),
        ],
    };
    let samples = BTreeMap::from([("ax", sample(100)), ("by", sample(100_000))]);
    let mut stats = estimates(3);
    collect_equalities(&step, &analysis, &samples, &schema(), &mut stats);
    assert_eq!(
        stats.equalities.len(),
        1,
        "two directions and reused occurrence are one equality"
    );
    let equality = &stats.equalities[0];
    assert_eq!(
        (equality.left, equality.right, equality.distinct),
        (5, 2, 100_000)
    );
    stats.parts[1].output = Some(CardinalityEstimate::exact(100_000));
    stats.variables.push(VariableDomain {
        providers: 5,
        count: Some(100),
    });
    for prefix in [3, 6, 7] {
        assert_eq!(
            stats.rows(prefix).unwrap().expected,
            100,
            "prefix={prefix:b}"
        );
    }
}

#[test]
fn independent_and_properties_keep_separate_equivalence_classes() {
    let step = step(vec![part("a"), part("b")]);
    let analysis = JoinAnalysis {
        hashes: Vec::new(),
        predicates: Vec::new(),
        edges: Vec::new(),
        templates: vec![
            template(0, "b", "y", "ax", 2),
            template(1, "a", "x", "by", 1),
            template(0, "b", "w", "az", 2),
            template(1, "a", "z", "bw", 1),
        ],
    };
    let samples = BTreeMap::from([
        ("ax", sample(10)),
        ("by", sample(10)),
        ("az", sample(10)),
        ("bw", sample(10)),
    ]);
    let mut stats = estimates(2);
    collect_equalities(&step, &analysis, &samples, &schema(), &mut stats);
    assert_eq!(stats.equalities.len(), 2);
    assert_eq!(stats.equality_keys, 4);
    assert_eq!(stats.rows(3).unwrap().expected, 100);
}

#[test]
fn shared_variable_division_matches_repeated_ceil_without_repeated_work() {
    for expected in [0u64, 1, 2, u64::MAX] {
        for count in [0u64, 1, 2, u64::MAX] {
            for copies in [0u32, 1, 2, 127] {
                let mut repeated = expected;
                for _ in 0..copies {
                    repeated = repeated.div_ceil(count.max(1));
                }
                assert_eq!(
                    expected.div_ceil(count.max(1).saturating_pow(copies)),
                    repeated,
                    "expected={expected} count={count} copies={copies}"
                );
            }
        }
    }
}

fn bound_stats() -> JoinEstimates {
    let mut stats = estimates(3);
    let native = AccessEstimate::stream(Some(expectation(1, 32)), 1);
    stats.parts[1].reversed = Some(stats.parts[1].written);
    stats.bound = vec![(None, None), (Some(native), Some(native)), (None, None)];
    stats.bindings = vec![(0, 0), (1, 4), (0, 0)];
    stats.bound_sources = vec![BoundSources::default(); 3];
    stats.bound_sources[1] = BoundSources {
        written: Some(BoundSource {
            guard: 1,
            access: AccessEstimate::stream(Some(expectation(32, 32)), 1),
        }),
        reversed: Some(BoundSource {
            guard: 4,
            access: AccessEstimate::stream(Some(expectation(7, 7)), 1),
        }),
    };
    stats
}

#[test]
fn bound_source_advice_obeys_ready_provider_direction_and_relationship_priority() {
    let mut stats = bound_stats();
    let rows = |stats: &JoinEstimates, orientation, prefix| {
        stats
            .access(1, orientation, StageAccess::Existing, prefix)
            .unwrap()
            .rows
            .unwrap()
            .expected
    };
    assert_eq!(
        rows(&stats, PartOrientation::Written, 0),
        100,
        "no future source evidence"
    );
    assert_eq!(rows(&stats, PartOrientation::Written, 1), 32);
    assert_eq!(rows(&stats, PartOrientation::Reversed, 4), 7);
    assert_eq!(
        rows(&stats, PartOrientation::Written, 4),
        100,
        "the other endpoint is not bound"
    );
    stats.bound_sources[1].written.as_mut().unwrap().guard = 4;
    assert_eq!(
        rows(&stats, PartOrientation::Written, 1),
        1,
        "bound without this source guard keeps its prior advice"
    );
    stats.bound_sources[1].written.as_mut().unwrap().guard = 1;
    stats.bound[1].0 = None;
    assert_eq!(
        rows(&stats, PartOrientation::Written, 1),
        100,
        "Relationship ID/Index still bypasses the bound start"
    );
}

#[test]
fn native_source_cost_keeps_local_empty_filter_and_physical_reads_separate() {
    let mut stats = bound_stats();
    let analysis = JoinAnalysis {
        hashes: Vec::new(),
        edges: Vec::new(),
        templates: Vec::new(),
        predicates: vec![JoinPredicate {
            expression: Expr::Literal(Value::Boolean(false)),
            guards: vec![PartSet(2)],
        }],
    };
    collect_filters(&analysis, &mut stats);
    let selected = stats
        .access(1, PartOrientation::Written, StageAccess::Existing, 1)
        .unwrap();
    assert_eq!(selected.rows.unwrap(), CardinalityEstimate::exact(0));
    assert_eq!(
        selected.io,
        Some(33),
        "zero output never makes the required source read free"
    );
    assert_eq!(selected.cpu, Some(65));
}
