//! Shared plain pattern fixtures for the factor planner's proof tests.

use crate::query::plan::{Direction, MatchStep, NodeSpec, PatternPart, RelationshipSpec};

pub(super) fn node(name: &str) -> NodeSpec {
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

pub(super) fn part(
    start: &str,
    relationship: &str,
    end: &str,
    type_id: i64,
    type_name: &str,
) -> PatternPart {
    PatternPart {
        path_variable: None,
        start: node(start),
        end: Some(node(end)),
        relationship: Some(RelationshipSpec {
            variable: Some(relationship.into()),
            type_id: Some(type_id),
            type_name: Some(type_name.into()),
            direction: Direction::Outgoing,
            index_seek: None,
            id_seek: None,
            impossible: false,
        }),
    }
}

pub(super) fn step(parts: Vec<PatternPart>) -> MatchStep {
    MatchStep {
        clause_start: 0,
        optional: false,
        parts,
        predicate: None,
        early_predicates: vec![],
        join: None,
    }
}
