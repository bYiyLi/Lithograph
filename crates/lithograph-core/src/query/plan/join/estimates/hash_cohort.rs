//! Whole-owner-domain eligibility for an already sampled property Index.

use super::super::{HashInputKey, JoinAnalysis};
use crate::query::graph::ResolvedGraphView;
use crate::query::plan::{MatchStep, NodeSpec};
use crate::storage::StandardIndexKind;

/// This is only the static input-cohort gate. The caller separately checks the
/// actual Index Label/property, prefix bindings and provider readiness. Neither
/// a subset NDV nor a property value's encoded width is proved by this helper.
pub(super) fn full_cohort(
    step: &MatchStep,
    part: usize,
    key: &HashInputKey,
    analysis: &JoinAnalysis,
    view: &ResolvedGraphView,
) -> bool {
    let Some(pattern) = step.parts.get(part) else {
        return false;
    };
    if part >= u128::BITS as usize
        || pattern.path_variable.is_some()
        || pattern.relationship.is_some()
        || pattern.end.is_some()
        || !whole_node(&pattern.start, key)
        || view != &ResolvedGraphView::default()
    {
        return false;
    }
    let bit = 1_u128 << part;
    key.providers.0 & bit != 0
        && !analysis
            .predicates
            .iter()
            .any(|predicate| predicate.guards.len() == 1 && predicate.guards[0].0 & bit != 0)
}

fn whole_node(node: &NodeSpec, key: &HashInputKey) -> bool {
    node.variable.as_deref() == Some(key.variable.as_str())
        && node.labels.len() == 1
        && node.label_names.len() == 1
        && !node.impossible
        && node.id_seek.is_none()
        && node
            .index_seek
            .as_ref()
            .is_none_or(|seek| seek.kind == StandardIndexKind::Lookup)
}

#[cfg(test)]
mod tests {
    use super::super::super::{JoinPredicate, PartSet};
    use super::*;
    use crate::cypher::Value;
    use crate::query::access::AccessPolicy;
    use crate::query::expression::Expr;
    use crate::query::plan::{Direction, PatternPart, RelationshipSpec};
    use crate::query::schema::{StandardIndexPredicate, StandardIndexSeek};

    fn step() -> MatchStep {
        MatchStep {
            clause_start: 0,
            optional: false,
            parts: vec![PatternPart {
                path_variable: None,
                start: NodeSpec {
                    variable: Some("n".into()),
                    labels: vec![1],
                    label_names: vec!["Item".into()],
                    scan_label: Some(1),
                    scan_label_name: Some("Item".into()),
                    index_seek: None,
                    id_seek: None,
                    impossible: false,
                },
                relationship: None,
                end: None,
            }],
            predicate: None,
            early_predicates: Vec::new(),
            join: None,
        }
    }

    fn key(providers: u128) -> HashInputKey {
        HashInputKey {
            variable: "n".into(),
            property: Some("key".into()),
            providers: PartSet(providers),
        }
    }

    fn analysis(guards: &[&[u128]]) -> JoinAnalysis {
        JoinAnalysis {
            predicates: guards
                .iter()
                .map(|groups| JoinPredicate {
                    expression: Expr::Literal(Value::Boolean(true)),
                    guards: groups.iter().copied().map(PartSet).collect(),
                })
                .collect(),
            edges: Vec::new(),
            templates: Vec::new(),
            hashes: Vec::new(),
        }
    }

    fn eligible(step: &MatchStep) -> bool {
        full_cohort(
            step,
            0,
            &key(1),
            &analysis(&[]),
            &ResolvedGraphView::default(),
        )
    }

    #[test]
    fn one_complete_label_node_and_lookup_producers_keep_the_full_domain() {
        let mut step = step();
        assert!(eligible(&step));
        step.parts[0].start.index_seek = Some(StandardIndexSeek {
            index_name: "nodes".into(),
            kind: StandardIndexKind::Lookup,
            predicates: vec![(0, StandardIndexPredicate::Lookup)],
        });
        assert!(eligible(&step));
    }

    #[test]
    fn provider_and_variable_identity_are_required_without_shifting_past_128() {
        let step = step();
        let analysis = analysis(&[]);
        let view = ResolvedGraphView::default();
        assert!(!full_cohort(&step, 0, &key(2), &analysis, &view));
        let mut wrong = key(1);
        wrong.variable = "other".into();
        assert!(!full_cohort(&step, 0, &wrong, &analysis, &view));
        assert!(!full_cohort(&step, 128, &key(1), &analysis, &view));
        let mut large = step.clone();
        large.parts.resize(129, step.parts[0].clone());
        assert!(!full_cohort(&large, 128, &key(1), &analysis, &view));
    }

    #[test]
    fn extra_unknown_or_absent_labels_and_point_or_property_subsets_are_rejected() {
        let baseline = step();
        let mut extra = baseline.clone();
        extra.parts[0].start.labels.push(2);
        extra.parts[0].start.label_names.push("Extra".into());
        assert!(!eligible(&extra));
        let mut unresolved = baseline.clone();
        unresolved.parts[0].start.label_names.push("Unknown".into());
        assert!(!eligible(&unresolved));
        let mut broad = baseline.clone();
        broad.parts[0].start.labels.clear();
        assert!(!eligible(&broad));
        let mut point = baseline.clone();
        point.parts[0].start.id_seek = Some(1);
        assert!(!eligible(&point));
        let mut property = baseline.clone();
        property.parts[0].start.index_seek = Some(StandardIndexSeek {
            index_name: "filtered".into(),
            kind: StandardIndexKind::Range,
            predicates: Vec::new(),
        });
        assert!(!eligible(&property));
        let mut impossible = baseline;
        impossible.parts[0].start.impossible = true;
        assert!(!eligible(&impossible));
    }

    #[test]
    fn relationship_expansion_and_path_or_endpoint_shapes_are_not_owner_cohorts() {
        let baseline = step();
        let mut expanded = baseline.clone();
        expanded.parts[0].relationship = Some(RelationshipSpec {
            variable: Some("r".into()),
            type_id: Some(1),
            type_name: Some("R".into()),
            direction: Direction::Outgoing,
            index_seek: None,
            id_seek: None,
            impossible: false,
        });
        assert!(!eligible(&expanded));
        let mut path = baseline.clone();
        path.parts[0].path_variable = Some("p".into());
        assert!(!eligible(&path));
        let mut endpoint = baseline;
        endpoint.parts[0].end = Some(endpoint.parts[0].start.clone());
        assert!(!eligible(&endpoint));
    }

    #[test]
    fn local_any_provider_guard_blocks_only_the_covered_part() {
        let step = step();
        let view = ResolvedGraphView::default();
        assert!(!full_cohort(&step, 0, &key(1), &analysis(&[&[1]]), &view));
        assert!(!full_cohort(&step, 0, &key(1), &analysis(&[&[3]]), &view));
        assert!(full_cohort(&step, 0, &key(1), &analysis(&[&[2]]), &view));
        assert!(full_cohort(&step, 0, &key(1), &analysis(&[&[1, 2]]), &view));
    }

    #[test]
    fn nondefault_resolved_view_is_rejected_without_resolving_or_reading_sql() {
        let options = serde_json::json!({
            "readAccess": {"query": {"denyProperties": ["hidden"]}}
        });
        let policy = AccessPolicy::parse(options.as_object().unwrap()).unwrap();
        let view = ResolvedGraphView::default().with_query_access(policy.read.query);
        assert!(!full_cohort(&step(), 0, &key(1), &analysis(&[]), &view));
    }
}
