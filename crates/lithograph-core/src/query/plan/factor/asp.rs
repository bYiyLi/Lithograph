//! A three-part Node-identity extension of a separately proved two-arm star.
//! Reuse the existing Join equality; this module never invents a Hash family.

use super::{FactorAsp, FactorBinding, FactorBindingKind, FactorGroup, FactorizedPlan};
use crate::query::expression::{BinaryOp, Expr};
use crate::query::plan::{MatchStep, NodeSpec, PlanningBudget, join};
use crate::query::{ExecutionControl, QueryResult};

pub(super) struct AspShape<'a> {
    pub(super) shared_predicate: &'a Expr,
    pub(super) identity: &'a Expr,
    pub(super) star_parts: [usize; 2],
    pub(super) build_part: usize,
    pub(super) probe_variable: &'a str,
    pub(super) build_variable: &'a str,
    pub(super) equality: usize,
    pub(super) inner_side: join::HashOperandSide,
}

pub(super) fn inspect<'a>(
    step: &'a MatchStep,
    scope: &join::JoinClauseScope<'_>,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<AspShape<'a>>> {
    context.control.check()?;
    if step.optional
        || step.parts.len() != 3
        || !scope.incoming.is_empty()
        || scope.declared_columns.len() != 6
        || scope.clause_start != step.clause_start
        || scope.snapshot_identity != context.snapshot.cache_identity()
        || !context.view.is_full_graph()
        || !context.view.query_access.is_empty()
    {
        return Ok(None);
    }
    if !prepay(step, budget, context.control)? {
        return Ok(None);
    }
    let Some((star_parts, build_part)) = parts(step) else {
        return Ok(None);
    };
    let build = &step.parts[build_part].start;
    let Some(build_variable) = build.variable.as_deref() else {
        return Ok(None);
    };
    let Some((shared_predicate, identity, probe_variable)) =
        predicates(step.predicate.as_ref(), build_variable)
    else {
        return Ok(None);
    };
    let Some(probe_part) = probe_part(step, star_parts, probe_variable, build_variable) else {
        return Ok(None);
    };
    let Some(plan) = step.join.as_ref() else {
        return Ok(None);
    };
    if plan.clause_start != scope.clause_start || plan.snapshot_identity != scope.snapshot_identity
    {
        return Ok(None);
    }
    let Some((equality, inner_side)) =
        hash_identity(plan, probe_variable, build_variable, probe_part, build_part)
    else {
        return Ok(None);
    };
    context.control.check()?;
    Ok(Some(AspShape {
        shared_predicate,
        identity,
        star_parts,
        build_part,
        probe_variable,
        build_variable,
        equality,
        inner_side,
    }))
}

fn prepay(
    step: &MatchStep,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    // All later Hash/predicate searches have at most two entries. Include a
    // four-slot predicate-after allocation before install can add its guard.
    if !budget.work(96, 4 * std::mem::size_of::<usize>()) {
        return Ok(false);
    }
    for part in &step.parts {
        control.work(1)?;
        for node in [Some(&part.start), part.end.as_ref()].into_iter().flatten() {
            let bytes = node.variable.as_ref().map_or(0, String::len);
            if !budget.work(node.labels.len().saturating_add(1), bytes) {
                return Ok(false);
            }
            for label in &node.labels {
                control.work(1)?;
                if *label <= 0 {
                    return Ok(false);
                }
            }
        }
        if let Some(name) = part
            .relationship
            .as_ref()
            .and_then(|rel| rel.variable.as_ref())
            && !budget.work(1, name.len())
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn parts(step: &MatchStep) -> Option<([usize; 2], usize)> {
    let mut star = [usize::MAX; 2];
    let mut count = 0;
    let mut build = None;
    for (index, part) in step.parts.iter().enumerate() {
        if part.path_variable.is_some() {
            return None;
        }
        if part.relationship.is_some() && part.end.is_some() {
            *star.get_mut(count)? = index;
            count += 1;
        } else if part.relationship.is_none() && part.end.is_none() && plain_node(&part.start) {
            if build.replace(index).is_some() {
                return None;
            }
        } else {
            return None;
        }
    }
    (count == 2).then_some((star, build?))
}

fn plain_node(node: &NodeSpec) -> bool {
    node.variable.as_ref().is_some_and(|name| !name.is_empty())
        && node.id_seek.is_none()
        && node.index_seek.is_none()
        && !node.impossible
        && node.labels.len() == node.label_names.len()
}

fn predicates<'a>(
    predicate: Option<&'a Expr>,
    build: &str,
) -> Option<(&'a Expr, &'a Expr, &'a str)> {
    let Expr::Binary(BinaryOp::And, left, right) = predicate? else {
        return None;
    };
    match (identity_probe(left, build), identity_probe(right, build)) {
        (Some(probe), None) => Some((right, left, probe)),
        (None, Some(probe)) => Some((left, right, probe)),
        _ => None,
    }
}

fn identity_probe<'a>(expression: &'a Expr, build: &str) -> Option<&'a str> {
    let Expr::Binary(BinaryOp::Equal, left, right) = expression else {
        return None;
    };
    let (Expr::Variable(left), Expr::Variable(right)) = (left.as_ref(), right.as_ref()) else {
        return None;
    };
    if left == build && right != build {
        Some(right)
    } else if right == build && left != build {
        Some(left)
    } else {
        None
    }
}

fn probe_part(step: &MatchStep, star: [usize; 2], probe: &str, build: &str) -> Option<usize> {
    let mut owner = None;
    for index in star {
        let part = &step.parts[index];
        let relationship = part.relationship.as_ref()?;
        if relationship.variable.as_deref() == Some(build) {
            return None;
        }
        for node in [Some(&part.start), part.end.as_ref()].into_iter().flatten() {
            let name = node.variable.as_deref()?;
            if name == build {
                return None;
            }
            if name == probe && owner.replace(index).is_some() {
                return None;
            }
        }
    }
    owner
}

fn hash_identity(
    plan: &join::JoinPlan,
    probe: &str,
    build: &str,
    probe_part: usize,
    build_part: usize,
) -> Option<(usize, join::HashOperandSide)> {
    if plan.stages.len() != 3 || plan.predicates.len() != 2 || plan.hashes.len() > 2 {
        return None;
    }
    let probe_providers = join::PartSet(1_u128.checked_shl(probe_part as u32)?);
    let build_providers = join::PartSet(1_u128.checked_shl(build_part as u32)?);
    for (index, equality) in plan.hashes.iter().enumerate() {
        if equality.family != join::HashFamily::NodeIdentity {
            continue;
        }
        let predicate = &plan.predicates.get(equality.predicate)?.expression;
        if identity_probe(predicate, build) != Some(probe) {
            continue;
        }
        if key_matches(&equality.left, probe, probe_providers)
            && key_matches(&equality.right, build, build_providers)
        {
            return Some((index, join::HashOperandSide::Right));
        }
        if key_matches(&equality.right, probe, probe_providers)
            && key_matches(&equality.left, build, build_providers)
        {
            return Some((index, join::HashOperandSide::Left));
        }
    }
    None
}

fn key_matches(key: &join::HashInputKey, name: &str, providers: join::PartSet) -> bool {
    key.variable == name && key.property.is_none() && key.providers == providers
}

pub(super) fn extend(
    step: &MatchStep,
    mut base: FactorizedPlan,
    shape: &AspShape<'_>,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<FactorizedPlan>> {
    control.check()?;
    if base.asp.is_some() || !base.single_shared || base.bindings.len() != 5 {
        return Ok(None);
    }
    let Some(hash) = step
        .join
        .as_ref()
        .and_then(|plan| plan.hashes.get(shape.equality))
    else {
        return Ok(None);
    };
    let (build, probe) = hash.keys(shape.inner_side);
    if hash.family != join::HashFamily::NodeIdentity
        || probe.variable != shape.probe_variable
        || build.variable != shape.build_variable
    {
        return Ok(None);
    }
    let Some(part) = step.parts.get(shape.build_part) else {
        return Ok(None);
    };
    let node = &part.start;
    let Some(position) = step.join.as_ref().and_then(|plan| {
        plan.declared_columns
            .iter()
            .position(|name| name == shape.build_variable)
    }) else {
        return Ok(None);
    };
    if position > base.bindings.len() {
        return Ok(None);
    }
    let Some(probe_group) = extend_budget(&base, shape, node, budget, control)? else {
        return Ok(None);
    };
    if base.bindings.try_reserve_exact(1).is_err() {
        return Ok(None);
    }
    base.bindings.insert(
        position,
        FactorBinding {
            name: shape.build_variable.to_owned(),
            group: probe_group,
            kind: FactorBindingKind::Node {
                labels: node.labels.clone(),
            },
        },
    );
    base.asp = Some(FactorAsp {
        part: shape.build_part,
        probe_variable: shape.probe_variable.to_owned(),
        build_variable: shape.build_variable.to_owned(),
        probe_group,
    });
    control.check()?;
    Ok(Some(base))
}

fn extend_budget(
    base: &FactorizedPlan,
    shape: &AspShape<'_>,
    node: &NodeSpec,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<FactorGroup>> {
    let mut probe_group = None;
    for binding in &base.bindings {
        control.work(1)?;
        if !budget.work(1, binding.name.len()) || binding.name == shape.build_variable {
            return Ok(None);
        }
        if binding.name == shape.probe_variable {
            if probe_group.is_some()
                || binding.group == FactorGroup::Shared
                || !matches!(binding.kind, FactorBindingKind::Node { .. })
            {
                return Ok(None);
            }
            probe_group = Some(binding.group);
        }
    }
    let Some(probe_group) = probe_group else {
        return Ok(None);
    };
    let names = shape
        .probe_variable
        .len()
        .saturating_add(shape.build_variable.len().saturating_mul(2));
    let bytes = std::mem::size_of::<FactorAsp>()
        .saturating_add(6 * std::mem::size_of::<FactorBinding>())
        .saturating_add(
            node.labels
                .len()
                .saturating_mul(std::mem::size_of::<crate::storage::LabelId>()),
        )
        .saturating_add(names);
    Ok(budget
        .work(node.labels.len().saturating_add(1), bytes)
        .then_some(probe_group))
}

/// Called only after the owner accepts the complete candidate cost. Validation
/// precedes mutation, so false preserves the existing executable Join order.
pub(super) fn install(plan: &mut join::JoinPlan, description: &FactorizedPlan) -> bool {
    if description.clause_start != plan.clause_start {
        return false;
    }
    let Some(asp) = &description.asp else {
        return false;
    };
    let ([a], [b]) = (
        description.accumulate.hops.as_slice(),
        description.build.hops.as_slice(),
    ) else {
        return false;
    };
    let Some(probe_part) = provider_part(plan, asp.probe_variable.as_str(), asp.part) else {
        return false;
    };
    if !probe_owner(asp.probe_group, a.part, b.part, probe_part) {
        return false;
    }
    let Some((equality, inner_side)) = hash_identity(
        plan,
        &asp.probe_variable,
        &asp.build_variable,
        probe_part,
        asp.part,
    ) else {
        return false;
    };
    let order = [a.part, b.part, asp.part];
    if order.iter().any(|part| *part >= 3) || order[0] == order[1] || order[..2].contains(&order[2])
    {
        return false;
    }
    if order.iter().any(|part| {
        plan.stages
            .iter()
            .filter(|stage| stage.part == *part)
            .count()
            != 1
    }) {
        return false;
    }
    let Some(build) = plan.stages.iter_mut().find(|stage| stage.part == asp.part) else {
        return false;
    };
    if build.predicates_after.try_reserve_exact(1).is_err() {
        return false;
    }
    reorder(plan, order);
    let predicate = plan.hashes[equality].predicate;
    for (stage, hop) in plan.stages[..2].iter_mut().zip([a, b]) {
        stage.orientation = hop.orientation;
        stage.access = join::StageAccess::Existing;
        stage.predicates_after.retain(|index| *index != predicate);
    }
    let stage = &mut plan.stages[2];
    stage.orientation = join::PartOrientation::Written;
    stage.access = join::StageAccess::Hash {
        equality,
        inner_side,
        dynamic_filter: None,
    };
    stage.cartesian = false;
    if !stage.predicates_after.contains(&predicate) {
        stage.predicates_after.push(predicate);
    }
    true
}

fn probe_owner(group: FactorGroup, accumulate: usize, build: usize, actual: usize) -> bool {
    match group {
        FactorGroup::Accumulate => actual == accumulate,
        FactorGroup::Build => actual == build,
        FactorGroup::Shared => false,
    }
}

fn provider_part(plan: &join::JoinPlan, probe: &str, build_part: usize) -> Option<usize> {
    if plan.hashes.len() > 2 {
        return None;
    }
    for hash in &plan.hashes {
        for key in [&hash.left, &hash.right] {
            if key.variable != probe || key.providers.0.count_ones() != 1 {
                continue;
            }
            let part = key.providers.0.trailing_zeros() as usize;
            if part != build_part && part < 3 {
                return Some(part);
            }
        }
    }
    None
}

fn reorder(plan: &mut join::JoinPlan, order: [usize; 3]) {
    for (index, part) in order.into_iter().enumerate() {
        if let Some(position) = plan.stages.iter().position(|stage| stage.part == part) {
            plan.stages.swap(index, position);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::Value;
    use crate::query::QueryResources;
    use crate::query::plan::{Direction, FactorArm, FactorHop, PatternPart, RelationshipSpec};
    use crate::storage::HashId;

    fn identity(left: &str, right: &str) -> Expr {
        Expr::Binary(
            BinaryOp::Equal,
            Box::new(Expr::Variable(left.into())),
            Box::new(Expr::Variable(right.into())),
        )
    }

    fn key(name: &str, providers: u128) -> join::HashInputKey {
        join::HashInputKey {
            variable: name.into(),
            property: None,
            providers: join::PartSet(providers),
        }
    }

    fn plan() -> join::JoinPlan {
        join::JoinPlan {
            clause_start: 0,
            snapshot_identity: HashId::from_bytes([0; 32]),
            declared_columns: vec![],
            templates: vec![],
            factorized: None,
            predicates: vec![
                join::JoinPredicate {
                    expression: Expr::Literal(Value::Boolean(true)),
                    guards: vec![],
                },
                join::JoinPredicate {
                    expression: identity("a", "d"),
                    guards: vec![],
                },
            ],
            hashes: vec![join::HashEquality {
                predicate: 1,
                left: key("a", 1),
                right: key("d", 4),
                family: join::HashFamily::NodeIdentity,
            }],
            stages: [0, 2, 1]
                .into_iter()
                .map(|part| join::JoinStage {
                    part,
                    orientation: join::PartOrientation::Written,
                    access: join::StageAccess::Existing,
                    cartesian: false,
                    predicates_after: if part == 0 { vec![0, 1] } else { vec![] },
                })
                .collect(),
        }
    }

    fn description() -> FactorizedPlan {
        FactorizedPlan {
            clause_start: 0,
            shared: "h".into(),
            bindings: vec![],
            single_shared: true,
            shared_predicate: Expr::Literal(Value::Boolean(true)),
            accumulate: FactorArm {
                hops: vec![FactorHop {
                    part: 0,
                    orientation: join::PartOrientation::Reversed,
                }],
            },
            build: FactorArm {
                hops: vec![FactorHop {
                    part: 1,
                    orientation: join::PartOrientation::Written,
                }],
            },
            asp: Some(FactorAsp {
                part: 2,
                probe_variable: "a".into(),
                build_variable: "d".into(),
                probe_group: FactorGroup::Accumulate,
            }),
        }
    }

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

    fn star(end: &str, name: &str, type_id: i64) -> PatternPart {
        PatternPart {
            path_variable: None,
            start: node("h"),
            end: Some(node(end)),
            relationship: Some(RelationshipSpec {
                variable: Some(name.into()),
                type_id: Some(type_id),
                type_name: Some("R".into()),
                direction: Direction::Outgoing,
                index_seek: None,
                id_seek: None,
                impossible: false,
            }),
        }
    }

    fn extension_step(inner_side: join::HashOperandSide) -> MatchStep {
        let mut plan = plan();
        plan.declared_columns = ["h", "r", "a", "s", "b", "d"].map(str::to_owned).to_vec();
        if inner_side == join::HashOperandSide::Left {
            let hash = &mut plan.hashes[0];
            std::mem::swap(&mut hash.left, &mut hash.right);
        }
        let mut build = node("d");
        build.labels = vec![7];
        build.label_names = vec!["Build".into()];
        MatchStep {
            clause_start: 0,
            optional: false,
            parts: vec![
                star("a", "r", 1),
                star("b", "s", 2),
                PatternPart {
                    path_variable: None,
                    start: build,
                    relationship: None,
                    end: None,
                },
            ],
            predicate: None,
            early_predicates: vec![],
            join: Some(std::sync::Arc::new(plan)),
        }
    }

    fn base_description() -> FactorizedPlan {
        let mut base = description();
        base.asp = None;
        base.bindings = ["h", "r", "a", "s", "b"]
            .into_iter()
            .map(|name| {
                let group = match name {
                    "h" => FactorGroup::Shared,
                    "r" | "a" => FactorGroup::Accumulate,
                    _ => FactorGroup::Build,
                };
                let kind = match name {
                    "r" => FactorBindingKind::Relationship { type_id: 1 },
                    "s" => FactorBindingKind::Relationship { type_id: 2 },
                    _ => FactorBindingKind::Node { labels: vec![] },
                };
                FactorBinding {
                    name: name.into(),
                    group,
                    kind,
                }
            })
            .collect();
        base
    }

    #[test]
    fn identity_accepts_commutation_but_not_self_equality_or_property_keys() {
        assert_eq!(identity_probe(&identity("a", "d"), "d"), Some("a"));
        assert_eq!(identity_probe(&identity("d", "a"), "d"), Some("a"));
        assert_eq!(identity_probe(&identity("d", "d"), "d"), None);
        let property = Expr::Binary(
            BinaryOp::Equal,
            Box::new(Expr::Property(
                Box::new(Expr::Variable("a".into())),
                "k".into(),
            )),
            Box::new(Expr::Variable("d".into())),
        );
        assert_eq!(identity_probe(&property, "d"), None);
    }

    #[test]
    fn installs_existing_hash_and_moves_identity_guard_after_all_providers() {
        let mut plan = plan();
        assert!(install(&mut plan, &description()));
        assert_eq!(
            plan.stages
                .iter()
                .map(|stage| stage.part)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(
            plan.stages[2].access,
            join::StageAccess::Hash {
                equality: 0,
                inner_side: join::HashOperandSide::Right,
                dynamic_filter: None,
            }
        );
        assert_eq!(plan.stages[0].predicates_after, vec![0]);
        assert_eq!(plan.stages[2].predicates_after, vec![1]);
        assert_eq!(plan.hashes.len(), 1);
    }

    #[test]
    fn mismatched_provider_proof_preserves_the_original_stage_order() {
        let mut plan = plan();
        plan.hashes[0].right.providers = join::PartSet(2);
        assert!(!install(&mut plan, &description()));
        assert_eq!(
            plan.stages
                .iter()
                .map(|stage| stage.part)
                .collect::<Vec<_>>(),
            vec![0, 2, 1]
        );
        assert!(
            plan.stages
                .iter()
                .all(|stage| stage.access == join::StageAccess::Existing)
        );
    }

    #[test]
    fn shared_probe_group_is_rejected_before_stage_mutation() {
        let mut plan = plan();
        let mut description = description();
        description.asp.as_mut().unwrap().probe_group = FactorGroup::Shared;
        assert!(!install(&mut plan, &description));
        assert_eq!(
            plan.stages
                .iter()
                .map(|stage| stage.part)
                .collect::<Vec<_>>(),
            vec![0, 2, 1]
        );
    }

    #[test]
    fn extend_accepts_both_real_hash_inner_sides_and_installs_the_same_equality() {
        let resources = QueryResources::default();
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);
        let shared = Expr::Literal(Value::Boolean(true));
        let identity = identity("a", "d");
        for inner_side in [join::HashOperandSide::Left, join::HashOperandSide::Right] {
            let mut step = extension_step(inner_side);
            let shape = AspShape {
                shared_predicate: &shared,
                identity: &identity,
                star_parts: [0, 1],
                build_part: 2,
                probe_variable: "a",
                build_variable: "d",
                equality: 0,
                inner_side,
            };
            let extended = extend(
                &step,
                base_description(),
                &shape,
                &mut PlanningBudget::default(),
                &control,
            )
            .unwrap()
            .unwrap();
            assert_eq!(extended.bindings.len(), 6);
            let binding = &extended.bindings[5];
            assert_eq!(binding.name, "d");
            assert_eq!(binding.group, FactorGroup::Accumulate);
            assert_eq!(
                extended.asp.as_ref().unwrap().probe_group,
                FactorGroup::Accumulate
            );
            assert!(matches!(&binding.kind, FactorBindingKind::Node { labels } if labels == &[7]));
            let plan = std::sync::Arc::get_mut(step.join.as_mut().unwrap()).unwrap();
            assert!(install(plan, &extended));
            assert_eq!(
                plan.stages[2].access,
                join::StageAccess::Hash {
                    equality: 0,
                    inner_side,
                    dynamic_filter: None,
                }
            );
        }
    }

    #[test]
    fn extend_and_install_preserve_b_first_order_when_probe_belongs_to_build_group() {
        let resources = QueryResources::default();
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);
        let shared = Expr::Literal(Value::Boolean(true));
        let identity = identity("a", "d");
        let mut step = extension_step(join::HashOperandSide::Right);
        std::sync::Arc::get_mut(step.join.as_mut().unwrap())
            .unwrap()
            .stages
            .swap(0, 2);
        let shape = AspShape {
            shared_predicate: &shared,
            identity: &identity,
            star_parts: [0, 1],
            build_part: 2,
            probe_variable: "a",
            build_variable: "d",
            equality: 0,
            inner_side: join::HashOperandSide::Right,
        };
        let mut base = base_description();
        std::mem::swap(&mut base.accumulate, &mut base.build);
        base.accumulate.hops[0].orientation = join::PartOrientation::Reversed;
        base.build.hops[0].orientation = join::PartOrientation::Written;
        for binding in &mut base.bindings {
            binding.group = match binding.group {
                FactorGroup::Accumulate => FactorGroup::Build,
                FactorGroup::Build => FactorGroup::Accumulate,
                FactorGroup::Shared => FactorGroup::Shared,
            };
        }
        let extended = extend(
            &step,
            base,
            &shape,
            &mut PlanningBudget::default(),
            &control,
        )
        .unwrap()
        .unwrap();
        assert_eq!(extended.bindings[5].name, "d");
        assert_eq!(extended.bindings[5].group, FactorGroup::Build);
        assert_eq!(
            extended.asp.as_ref().unwrap().probe_group,
            FactorGroup::Build
        );
        let plan = std::sync::Arc::get_mut(step.join.as_mut().unwrap()).unwrap();
        assert!(install(plan, &extended));
        assert_eq!(
            plan.stages
                .iter()
                .map(|stage| stage.part)
                .collect::<Vec<_>>(),
            vec![1, 0, 2]
        );
        assert_eq!(
            plan.stages[2].access,
            join::StageAccess::Hash {
                equality: 0,
                inner_side: join::HashOperandSide::Right,
                dynamic_filter: None,
            }
        );
        assert!(
            plan.stages[..2]
                .iter()
                .all(|stage| !stage.predicates_after.contains(&1))
        );
        assert!(plan.stages[2].predicates_after.contains(&1));
    }
}
