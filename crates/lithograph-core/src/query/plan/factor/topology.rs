//! Bounded path topology proof; schema, predicates and budget remain caller-owned.

use super::{FactorArm, FactorHop};
use crate::query::plan::{MatchStep, NodeSpec, PatternPart, join::PartOrientation};

#[derive(Clone, Copy)]
struct Edge<'a> {
    part: usize,
    start: &'a str,
    end: &'a str,
    relationship: &'a str,
    type_id: i64,
}

/// Return A (far leaf to shared) and B (shared to far leaf). The caller prepays
/// identifier comparisons and two vectors of at most two `FactorHop`s each.
pub(super) fn describe(
    step: &MatchStep,
    shared: &str,
    source: Option<&crate::query::plan::join::JoinPlan>,
) -> Option<(FactorArm, FactorArm)> {
    if step.optional || shared.is_empty() || !(2..=4).contains(&step.parts.len()) {
        return None;
    }
    let mut edges = [Edge {
        part: 0,
        start: "",
        end: "",
        relationship: "",
        type_id: 0,
    }; 4];
    for (part, pattern) in step.parts.iter().enumerate() {
        edges[part] = edge(pattern, part)?;
    }
    let edges = &edges[..step.parts.len()];
    if !unique_entities(edges) {
        return None;
    }
    let [left, right] = center_edges(edges, shared)?;
    let (mut a, a_mask) = walk_arm(edges, shared, left)?;
    let (mut b, b_mask) = walk_arm(edges, shared, right)?;
    let complete = (1_u8 << edges.len()) - 1;
    if a_mask & b_mask != 0 || a_mask | b_mask != complete {
        return None;
    }
    if !disjoint_types(edges, a_mask, b_mask) {
        return None;
    }
    let first = first_part(step, source)?;
    if a_mask & (1 << first) == 0 {
        std::mem::swap(&mut a, &mut b);
    }
    reverse(&mut a);
    Some((a, b))
}

fn edge(pattern: &PatternPart, part: usize) -> Option<Edge<'_>> {
    if pattern.path_variable.is_some() {
        return None;
    }
    let start = node_name(&pattern.start)?;
    let end = node_name(pattern.end.as_ref()?)?;
    let rel = pattern.relationship.as_ref()?;
    let relationship = rel.variable.as_deref()?;
    let type_id = rel.type_id?;
    if start == end || !plain_relationship(rel) {
        return None;
    }
    Some(Edge {
        part,
        start,
        end,
        relationship,
        type_id,
    })
}

fn plain_relationship(rel: &crate::query::plan::RelationshipSpec) -> bool {
    rel.variable.as_deref().is_some_and(|name| !name.is_empty())
        && rel.type_id.is_some_and(|id| id > 0)
        && rel
            .type_name
            .as_deref()
            .is_some_and(|name| !name.is_empty())
        && rel.direction != crate::query::plan::Direction::Undirected
        && rel.id_seek.is_none()
        && rel.index_seek.is_none()
        && !rel.impossible
}

fn node_name(node: &NodeSpec) -> Option<&str> {
    let name = node.variable.as_deref()?;
    (!name.is_empty()
        && node.id_seek.is_none()
        && node.index_seek.is_none()
        && !node.impossible
        && node.labels.len() == node.label_names.len())
    .then_some(name)
}

fn unique_entities(edges: &[Edge<'_>]) -> bool {
    for (index, edge) in edges.iter().enumerate() {
        if edges[..index]
            .iter()
            .any(|other| other.relationship == edge.relationship)
            || edges
                .iter()
                .any(|other| [other.start, other.end].contains(&edge.relationship))
        {
            return false;
        }
    }
    true
}

fn center_edges(edges: &[Edge<'_>], shared: &str) -> Option<[usize; 2]> {
    let mut sides = [0; 2];
    let mut count = 0;
    for edge in edges {
        if [edge.start, edge.end].contains(&shared) {
            *sides.get_mut(count)? = edge.part;
            count += 1;
        }
    }
    (count == 2).then_some(sides)
}

fn walk_arm(edges: &[Edge<'_>], shared: &str, first: usize) -> Option<(FactorArm, u8)> {
    let mut hops = Vec::with_capacity(2);
    let mut mask = 0;
    let mut current = shared;
    let mut part = first;
    for _ in 0..2 {
        let edge = edges.get(part)?;
        let (next, orientation) = if edge.start == current {
            (edge.end, PartOrientation::Written)
        } else if edge.end == current {
            (edge.start, PartOrientation::Reversed)
        } else {
            return None;
        };
        if next == shared || mask & (1 << part) != 0 {
            return None;
        }
        hops.push(FactorHop { part, orientation });
        mask |= 1 << part;
        let Some(following) = continuation(edges, next, part)? else {
            return Some((FactorArm { hops }, mask));
        };
        current = next;
        part = following;
    }
    None
}

fn continuation(edges: &[Edge<'_>], node: &str, previous: usize) -> Option<Option<usize>> {
    let mut next = None;
    for edge in edges {
        if edge.part != previous && [edge.start, edge.end].contains(&node) {
            if next.is_some() {
                return None;
            }
            next = Some(edge.part);
        }
    }
    Some(next)
}

fn disjoint_types(edges: &[Edge<'_>], a: u8, b: u8) -> bool {
    for left in edges.iter().filter(|edge| a & (1 << edge.part) != 0) {
        for right in edges.iter().filter(|edge| b & (1 << edge.part) != 0) {
            if left.type_id == right.type_id {
                return false;
            }
        }
    }
    true
}

fn first_part(
    step: &MatchStep,
    source: Option<&crate::query::plan::join::JoinPlan>,
) -> Option<usize> {
    let Some(join) = source else {
        return Some(0);
    };
    if join.stages.len() != step.parts.len() {
        return None;
    }
    let part = join.stages.first()?.part;
    (part < step.parts.len()).then_some(part)
}

pub(super) fn reverse(arm: &mut FactorArm) {
    arm.hops.reverse();
    for hop in &mut arm.hops {
        hop.orientation = match hop.orientation {
            PartOrientation::Written => PartOrientation::Reversed,
            PartOrientation::Reversed => PartOrientation::Written,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{self, step};
    use super::*;
    use crate::query::plan::{Direction, join};
    use crate::storage::HashId;

    fn part(start: &str, relationship: &str, end: &str, type_id: i64) -> PatternPart {
        test_support::part(start, relationship, end, type_id, &format!("R{type_id}"))
    }

    fn hops(arm: &FactorArm) -> Vec<(usize, PartOrientation)> {
        arm.hops
            .iter()
            .map(|hop| (hop.part, hop.orientation))
            .collect()
    }

    fn join_order(step: &mut MatchStep, order: &[usize]) {
        step.join = Some(std::sync::Arc::new(join::JoinPlan {
            clause_start: 0,
            snapshot_identity: HashId::from_bytes([0; 32]),
            declared_columns: vec![],
            predicates: vec![],
            templates: vec![],
            hashes: vec![],
            stages: order
                .iter()
                .map(|part| join::JoinStage {
                    part: *part,
                    orientation: PartOrientation::Written,
                    access: join::StageAccess::Existing,
                    cartesian: false,
                    predicates_after: vec![],
                })
                .collect(),
            factorized: None,
            multiway: None,
        }));
    }

    #[test]
    fn directions_and_endpoint_orders_preserve_part_identity() {
        for direction in [
            Direction::Outgoing,
            Direction::Incoming,
            Direction::Undirected,
        ] {
            for left_reversed in [false, true] {
                for right_reversed in [false, true] {
                    let left = if left_reversed {
                        ("h", "a")
                    } else {
                        ("a", "h")
                    };
                    let right = if right_reversed {
                        ("b", "h")
                    } else {
                        ("h", "b")
                    };
                    let mut value = step(vec![
                        part(left.0, "r", left.1, 1),
                        part(right.0, "s", right.1, 2),
                    ]);
                    for pattern in &mut value.parts {
                        pattern.relationship.as_mut().unwrap().direction = direction;
                    }
                    if direction == Direction::Undirected {
                        assert!(describe(&value, "h", value.join.as_deref()).is_none());
                        continue;
                    }
                    let (a, b) = describe(&value, "h", value.join.as_deref()).unwrap();
                    let orientation = |reversed| {
                        if reversed {
                            PartOrientation::Reversed
                        } else {
                            PartOrientation::Written
                        }
                    };
                    assert_eq!(hops(&a), vec![(0, orientation(left_reversed))]);
                    assert_eq!(hops(&b), vec![(1, orientation(right_reversed))]);
                }
            }
        }
    }

    #[test]
    fn three_hop_arm_accepts_internal_repeated_type() {
        let value = step(vec![
            part("a", "r", "m", 1),
            part("m", "s", "h", 1),
            part("h", "t", "b", 2),
        ]);
        let (a, b) = describe(&value, "h", value.join.as_deref()).unwrap();
        assert_eq!(
            hops(&a),
            vec![(0, PartOrientation::Written), (1, PartOrientation::Written)]
        );
        assert_eq!(hops(&b), vec![(2, PartOrientation::Written)]);
    }

    #[test]
    fn four_hops_use_the_requested_center_and_first_join_stage_arm() {
        let mut value = step(vec![
            part("a", "r", "m", 1),
            part("h", "s", "m", 1),
            part("h", "t", "n", 2),
            part("b", "u", "n", 2),
        ]);
        join_order(&mut value, &[3, 2, 1, 0]);
        let source = value.join.take().unwrap();
        let (a, b) = describe(&value, "h", Some(&source)).unwrap();
        assert_eq!(
            hops(&a),
            vec![
                (3, PartOrientation::Written),
                (2, PartOrientation::Reversed)
            ]
        );
        assert_eq!(
            hops(&b),
            vec![
                (1, PartOrientation::Written),
                (0, PartOrientation::Reversed)
            ]
        );
        assert!(describe(&value, "m", Some(&source)).is_none());
    }

    #[test]
    fn cycles_self_loops_branches_and_disconnected_parts_refuse() {
        for parts in [
            vec![part("h", "r", "a", 1), part("a", "s", "h", 2)],
            vec![
                part("h", "r", "a", 1),
                part("a", "s", "b", 1),
                part("b", "t", "h", 2),
            ],
            vec![
                part("h", "r", "a", 1),
                part("a", "s", "b", 1),
                part("b", "t", "c", 2),
                part("c", "u", "h", 2),
            ],
            vec![part("h", "r", "h", 1), part("h", "s", "b", 2)],
            vec![
                part("h", "r", "a", 1),
                part("h", "s", "b", 2),
                part("h", "t", "c", 3),
            ],
            vec![
                part("a", "r", "h", 1),
                part("h", "s", "b", 2),
                part("x", "t", "y", 3),
            ],
        ] {
            assert!(describe(&step(parts), "h", None).is_none());
        }
    }

    #[test]
    fn aliases_and_cross_arm_type_overlap_refuse() {
        for parts in [
            vec![part("a", "r", "h", 1), part("h", "r", "b", 2)],
            vec![part("a", "a", "h", 1), part("h", "s", "b", 2)],
            vec![part("a", "r", "h", 1), part("h", "s", "b", 1)],
            vec![
                part("a", "r", "m", 1),
                part("m", "s", "h", 2),
                part("h", "t", "b", 1),
            ],
        ] {
            assert!(describe(&step(parts), "h", None).is_none());
        }
    }

    #[test]
    fn unsupported_parts_names_and_access_paths_refuse() {
        let baseline = step(vec![part("a", "r", "h", 1), part("h", "s", "b", 2)]);
        for mutation in 0..9 {
            let mut value = baseline.clone();
            match mutation {
                0 => value.parts[0].path_variable = Some("p".into()),
                1 => value.parts[0].start.id_seek = Some(1),
                2 => value.parts[0].start.impossible = true,
                3 => value.parts[0].start.variable = Some(String::new()),
                4 => value.parts[0].end = None,
                5 => value.parts[0].relationship.as_mut().unwrap().type_id = Some(0),
                6 => value.parts[0].relationship.as_mut().unwrap().id_seek = Some(1),
                7 => value.parts[0].relationship.as_mut().unwrap().impossible = true,
                _ => value.parts[0].relationship.as_mut().unwrap().variable = None,
            }
            assert!(describe(&value, "h", value.join.as_deref()).is_none());
        }
        assert!(describe(&baseline, "a", baseline.join.as_deref()).is_none());
        assert!(describe(&baseline, "", baseline.join.as_deref()).is_none());
        assert!(describe(&step(vec![part("a", "r", "h", 1)]), "h", None).is_none());
    }
}
