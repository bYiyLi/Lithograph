//! Bounded ownership and label facts for the topology-proved two-arm paths.
//! Only identical variable names share facts; an equality is not an alias.

use super::{FactorArm, FactorBinding, FactorBindingKind, FactorGroup};
use crate::query::plan::{MatchStep, NodeSpec, PlanningBudget};
use crate::query::{ExecutionControl, QueryResult};
use crate::storage::{LabelId, RelationshipTypeId};

#[derive(Clone, Copy)]
enum Source {
    Node(FactorGroup),
    Relationship(FactorGroup, RelationshipTypeId),
}

struct Collector<'a, 'ctl> {
    step: &'a MatchStep,
    shared: &'a str,
    accumulate: &'a FactorArm,
    build: &'a FactorArm,
    budget: &'a mut PlanningBudget,
    control: &'a ExecutionControl<'ctl>,
}

pub(super) fn collect(
    step: &MatchStep,
    shared: &str,
    accumulate: &FactorArm,
    build: &FactorArm,
    declared: &[String],
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<Vec<FactorBinding>>> {
    control.check()?;
    if !valid_parts(step, accumulate, build)
        || declared.len() != 2 * (accumulate.hops.len() + build.hops.len()) + 1
        || shared_nodes(step, shared).is_none()
    {
        return Ok(None);
    }
    let mut collector = Collector {
        step,
        shared,
        accumulate,
        build,
        budget,
        control,
    };
    collector.collect(declared)
}

fn valid_parts(step: &MatchStep, a: &FactorArm, b: &FactorArm) -> bool {
    if !(2..=5).contains(&step.parts.len())
        || !(1..=2).contains(&a.hops.len())
        || !(1..=2).contains(&b.hops.len())
    {
        return false;
    }
    let mut seen = [false; 5];
    for hop in a.hops.iter().chain(&b.hops) {
        if hop.part >= step.parts.len() || seen[hop.part] {
            return false;
        }
        seen[hop.part] = true;
    }
    true
}

impl Collector<'_, '_> {
    fn inspect(&mut self, states: usize, bytes: usize) -> QueryResult<bool> {
        self.control.work(states.max(1) as u64)?;
        Ok(self.budget.work(states, bytes))
    }

    fn collect(&mut self, declared: &[String]) -> QueryResult<Option<Vec<FactorBinding>>> {
        let bytes = declared
            .len()
            .saturating_mul(std::mem::size_of::<FactorBinding>());
        if !self.inspect(1, bytes)? {
            return Ok(None);
        }
        // Prepay all output name clones before comparing or retaining names.
        for name in declared {
            if name.is_empty() || !self.inspect(1, name.len())? {
                return Ok(None);
            }
        }
        let mut bindings: Vec<FactorBinding> = Vec::with_capacity(declared.len());
        for name in declared {
            if !self.unique_name(&bindings, name)? {
                return Ok(None);
            }
            let Some(source) = self.source(name)? else {
                return Ok(None);
            };
            let (group, kind) = match source {
                Source::Node(group) => {
                    let Some(labels) = self.labels(name)? else {
                        return Ok(None);
                    };
                    (group, FactorBindingKind::Node { labels })
                }
                Source::Relationship(group, type_id) => {
                    (group, FactorBindingKind::Relationship { type_id })
                }
            };
            bindings.push(FactorBinding {
                name: name.clone(),
                group,
                kind,
            });
        }
        self.control.check()?;
        Ok(Some(bindings))
    }

    fn unique_name(&mut self, bindings: &[FactorBinding], name: &str) -> QueryResult<bool> {
        for previous in bindings {
            if !self.inspect(1, previous.name.len().saturating_add(name.len()))?
                || previous.name == name
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn source(&mut self, name: &str) -> QueryResult<Option<Source>> {
        let mut source = None;
        let mut arms = 0_u8;
        let step = self.step;
        let accumulate = self.accumulate;
        let build = self.build;
        for (arm, group, bit) in [
            (accumulate, FactorGroup::Accumulate, 1),
            (build, FactorGroup::Build, 2),
        ] {
            for hop in &arm.hops {
                let part = &step.parts[hop.part];
                for node in [Some(&part.start), part.end.as_ref()].into_iter().flatten() {
                    let Some(variable) = node.variable.as_deref() else {
                        return Ok(None);
                    };
                    if !self.inspect(1, variable.len().saturating_add(name.len()))? {
                        return Ok(None);
                    }
                    if variable != name {
                        continue;
                    }
                    let group = if name == self.shared {
                        FactorGroup::Shared
                    } else {
                        group
                    };
                    source = merge_source(source, Source::Node(group));
                    if source.is_none() {
                        return Ok(None);
                    }
                    arms |= bit;
                }
                let Some(rel) = part.relationship.as_ref() else {
                    return Ok(None);
                };
                let (Some(variable), Some(type_id)) = (rel.variable.as_deref(), rel.type_id) else {
                    return Ok(None);
                };
                if type_id <= 0 || !self.inspect(1, variable.len().saturating_add(name.len()))? {
                    return Ok(None);
                }
                if variable == name {
                    source = merge_source(source, Source::Relationship(group, type_id));
                    if source.is_none() {
                        return Ok(None);
                    }
                    arms |= bit;
                }
            }
        }
        if !ownership(source, arms, name == self.shared) {
            return Ok(None);
        }
        Ok(source)
    }

    fn labels(&mut self, name: &str) -> QueryResult<Option<Vec<LabelId>>> {
        let Some(capacity) = self.label_capacity(name)? else {
            return Ok(None);
        };
        if !self.inspect(0, capacity.saturating_mul(std::mem::size_of::<LabelId>()))? {
            return Ok(None);
        }
        let mut labels = Vec::with_capacity(capacity);
        let step = self.step;
        for part in &step.parts {
            for node in [Some(&part.start), part.end.as_ref()].into_iter().flatten() {
                if !self.inspect(1, name.len())? {
                    return Ok(None);
                }
                if node.variable.as_deref() != Some(name) {
                    continue;
                }
                for label in &node.labels {
                    if !self.append_label(&mut labels, *label)? {
                        return Ok(None);
                    }
                }
            }
        }
        Ok(Some(labels))
    }

    fn label_capacity(&mut self, name: &str) -> QueryResult<Option<usize>> {
        let mut capacity = 0_usize;
        let step = self.step;
        for part in &step.parts {
            for node in [Some(&part.start), part.end.as_ref()].into_iter().flatten() {
                if !self.inspect(1, name.len())? {
                    return Ok(None);
                }
                if node.variable.as_deref() != Some(name) {
                    continue;
                }
                if node.labels.len() != node.label_names.len() {
                    return Ok(None);
                }
                let Some(next) = capacity.checked_add(node.labels.len()) else {
                    return Ok(None);
                };
                capacity = next;
            }
        }
        Ok(Some(capacity))
    }

    fn append_label(&mut self, labels: &mut Vec<LabelId>, label: LabelId) -> QueryResult<bool> {
        if label <= 0 || !self.inspect(1, 0)? {
            return Ok(false);
        }
        for previous in labels.iter() {
            if !self.inspect(1, 0)? {
                return Ok(false);
            }
            if *previous == label {
                return Ok(true);
            }
        }
        labels.push(label);
        Ok(true)
    }
}

fn merge_source(current: Option<Source>, next: Source) -> Option<Source> {
    match (current, next) {
        (None, next) => Some(next),
        (Some(Source::Node(group)), Source::Node(other)) if group == other => {
            Some(Source::Node(group))
        }
        _ => None,
    }
}

fn ownership(source: Option<Source>, arms: u8, shared: bool) -> bool {
    if shared {
        arms == 3 && matches!(source, Some(Source::Node(FactorGroup::Shared)))
    } else {
        arms.count_ones() == 1 && source.is_some()
    }
}

/// At most four path edges and one optional, independent ASP Build node. The
/// caller's topology proof owns names/paths; this helper requires exactly two
/// written occurrences of the shared node, never an identity-equal alias.
pub(super) fn shared_nodes<'a>(step: &'a MatchStep, shared: &str) -> Option<[&'a NodeSpec; 2]> {
    if !(2..=5).contains(&step.parts.len()) {
        return None;
    }
    let mut result = [None, None];
    let mut count = 0;
    for part in &step.parts {
        for node in [Some(&part.start), part.end.as_ref()].into_iter().flatten() {
            if node.variable.as_deref() == Some(shared) {
                *result.get_mut(count)? = Some(node);
                count += 1;
            }
        }
    }
    let [Some(a), Some(b)] = result else {
        return None;
    };
    Some([a, b])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QueryResources;
    use crate::query::plan::{
        Direction, FactorHop, PatternPart, RelationshipSpec, join::PartOrientation,
    };

    fn node(name: &str, label: LabelId) -> NodeSpec {
        NodeSpec {
            variable: Some(name.into()),
            labels: vec![label],
            label_names: vec![format!("L{label}")],
            scan_label: Some(label),
            scan_label_name: Some(format!("L{label}")),
            index_seek: None,
            id_seek: None,
            impossible: false,
        }
    }

    fn part(start: (&str, LabelId), rel: &str, end: (&str, LabelId)) -> PatternPart {
        PatternPart {
            path_variable: None,
            start: node(start.0, start.1),
            end: Some(node(end.0, end.1)),
            relationship: Some(RelationshipSpec {
                variable: Some(rel.into()),
                type_id: Some(7),
                type_name: Some("R".into()),
                direction: Direction::Outgoing,
                index_seek: None,
                id_seek: None,
                impossible: false,
            }),
        }
    }

    fn fixture() -> (MatchStep, FactorArm, FactorArm, Vec<String>) {
        let step = MatchStep {
            clause_start: 0,
            optional: false,
            parts: vec![
                part(("a", 9), "r", ("u", 1)),
                part(("u", 2), "s", ("h", 3)),
                part(("h", 4), "t", ("b", 8)),
            ],
            predicate: None,
            early_predicates: vec![],
            join: None,
        };
        let a = FactorArm {
            hops: [0, 1]
                .map(|part| FactorHop {
                    part,
                    orientation: PartOrientation::Written,
                })
                .to_vec(),
        };
        let b = FactorArm {
            hops: vec![FactorHop {
                part: 2,
                orientation: PartOrientation::Written,
            }],
        };
        (
            step,
            a,
            b,
            ["t", "u", "a", "h", "r", "b", "s"]
                .map(str::to_owned)
                .to_vec(),
        )
    }

    fn bindings(
        step: &MatchStep,
        a: &FactorArm,
        b: &FactorArm,
        names: &[String],
    ) -> Option<Vec<FactorBinding>> {
        let resources = QueryResources::default();
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);
        collect(
            step,
            "h",
            a,
            b,
            names,
            &mut PlanningBudget::default(),
            &control,
        )
        .unwrap()
    }

    #[test]
    fn preserves_declared_order_and_unions_same_name_internal_and_shared_labels() {
        let (step, a, b, names) = fixture();
        let output = bindings(&step, &a, &b, &names).unwrap();
        assert_eq!(
            output
                .iter()
                .map(|binding| &binding.name)
                .collect::<Vec<_>>(),
            names.iter().collect::<Vec<_>>()
        );
        assert!(matches!(&output[1].kind, FactorBindingKind::Node { labels } if labels == &[1, 2]));
        assert_eq!(output[1].group, FactorGroup::Accumulate);
        assert!(matches!(&output[3].kind, FactorBindingKind::Node { labels } if labels == &[3, 4]));
        assert_eq!(output[3].group, FactorGroup::Shared);
        assert_eq!(output[0].group, FactorGroup::Build);
    }

    #[test]
    fn rejects_cross_arm_alias_repeated_relationship_and_inconsistent_labels() {
        let (mut step, a, b, names) = fixture();
        step.parts[2].end.as_mut().unwrap().variable = Some("u".into());
        assert!(bindings(&step, &a, &b, &names).is_none());
        let (mut step, a, b, names) = fixture();
        step.parts[2].relationship.as_mut().unwrap().variable = Some("r".into());
        assert!(bindings(&step, &a, &b, &names).is_none());
        let (mut step, a, b, names) = fixture();
        step.parts[1].start.label_names.clear();
        assert!(bindings(&step, &a, &b, &names).is_none());
    }

    #[test]
    fn shared_lookup_requires_exactly_two_written_occurrences() {
        let (mut step, _, _, _) = fixture();
        let nodes = shared_nodes(&step, "h").unwrap();
        assert_eq!(nodes[0].labels, vec![3]);
        assert_eq!(nodes[1].labels, vec![4]);
        assert!(shared_nodes(&step, "missing").is_none());
        step.parts.push(PatternPart {
            path_variable: None,
            start: node("h", 5),
            relationship: None,
            end: None,
        });
        assert!(shared_nodes(&step, "h").is_none());
    }
}
