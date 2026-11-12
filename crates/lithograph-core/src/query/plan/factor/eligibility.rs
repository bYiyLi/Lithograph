//! Proofs for the first two-arm ASP consumer. Rejection preserves the flat plan.

use super::{FactorArm, FactorBinding, FactorBindingKind, FactorGroup, FactorHop, FactorizedPlan};
use crate::cypher::Value;
use crate::query::QueryResult;
use crate::query::expression::{BinaryOp, Expr};
use crate::query::plan::{
    MatchStep, NodeSpec, PatternPart, PlanningBudget, RelationshipSpec, join,
};
use crate::storage::{ConstraintDefinitionKind, LabelId, PropertyType, SchemaTarget};

struct Proof<'a, 'q, 'db, 'ctl> {
    context: &'a join::JoinPlanningContext<'q, 'db, 'ctl>,
    budget: &'a mut PlanningBudget,
}

struct Arm<'a> {
    part: usize,
    leaf: &'a NodeSpec,
    shared: &'a NodeSpec,
    relationship: &'a RelationshipSpec,
    orientation: join::PartOrientation,
}

#[derive(Clone, Copy)]
enum Binding<'a> {
    Node(&'a NodeSpec, Option<&'a NodeSpec>),
    Relationship(&'a RelationshipSpec),
}

struct Equality<'a> {
    variable: &'a str,
    property: &'a str,
    value: i64,
}

pub(super) fn describe(
    step: &MatchStep,
    scope: &join::JoinClauseScope<'_>,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<FactorizedPlan>> {
    context.control.check()?;
    if step.optional
        || !supported_parts(step)
        || !scope.incoming.is_empty()
        || scope.declared_columns.len() != binding_count(step)
        || scope.clause_start != step.clause_start
        || scope.snapshot_identity != context.snapshot.cache_identity()
        || !context.view.is_full_graph()
        || !context.view.query_access.is_empty()
        || budget.exhausted()
    {
        return Ok(None);
    }
    Proof { context, budget }.describe(step, scope)
}

impl Proof<'_, '_, '_, '_> {
    fn inspect(&mut self, states: usize, bytes: usize) -> QueryResult<bool> {
        self.context.control.work(states.max(1) as u64)?;
        Ok(self.budget.work(states, bytes))
    }

    fn describe(
        &mut self,
        step: &MatchStep,
        scope: &join::JoinClauseScope<'_>,
    ) -> QueryResult<Option<FactorizedPlan>> {
        if !self.prepay(step, scope)? {
            return Ok(None);
        }
        if step.parts.len() > 2 && step.parts.iter().all(|part| part.relationship.is_some()) {
            return self.describe_path(step, scope);
        }
        let shape = if step.parts.len() == 3 {
            let Some(shape) = super::asp::inspect(step, scope, self.context, self.budget)? else {
                return Ok(None);
            };
            Some(shape)
        } else {
            None
        };
        let indices = shape.as_ref().map_or([0, 1], |shape| shape.star_parts);
        if !indices.iter().all(|part| plain_part(&step.parts[*part])) {
            return Ok(None);
        }
        let Some(shared) = shared_node(&step.parts[indices[0]], &step.parts[indices[1]]) else {
            return Ok(None);
        };
        let Some((a, b)) = selected_arms(step, scope, indices, shared) else {
            return Ok(None);
        };
        let Some(sources) = binding_sources(shared, &a, &b) else {
            return Ok(None);
        };
        let predicate = shape
            .as_ref()
            .map(|shape| shape.shared_predicate)
            .or(step.predicate.as_ref());
        let Some(shared_predicate) = predicate else {
            return Ok(None);
        };
        let identity = shape.as_ref().map(|shape| shape.identity);
        if !unique_names(&sources)
            || !self.prove_predicate(
                step,
                shared_predicate,
                identity,
                shared,
                a.shared,
                b.shared,
            )?
        {
            return Ok(None);
        }
        let Some(bindings) = self.bindings(&sources, scope.declared_columns)? else {
            return Ok(None);
        };
        self.context.control.check()?;
        let result = FactorizedPlan {
            clause_start: scope.clause_start,
            shared: shared.to_owned(),
            accumulate: factor_arm(&a),
            build: factor_arm(&b),
            bindings,
            single_shared: true,
            shared_predicate: shared_predicate.clone(),
            asp: None,
        };
        match shape {
            Some(shape) => {
                super::asp::extend(step, result, &shape, self.budget, self.context.control)
            }
            None => Ok(Some(result)),
        }
    }

    fn describe_path(
        &mut self,
        step: &MatchStep,
        scope: &join::JoinClauseScope<'_>,
    ) -> QueryResult<Option<FactorizedPlan>> {
        let Some(predicate) = &step.predicate else {
            return Ok(None);
        };
        let Some(key) = self.equality(predicate)? else {
            return Ok(None);
        };
        let Some((accumulate, build)) = super::topology::describe(step, key.variable) else {
            return Ok(None);
        };
        let Some([a, b]) = super::path_bindings::shared_nodes(step, key.variable) else {
            return Ok(None);
        };
        if !self.prove_predicate(step, predicate, None, key.variable, a, b)? {
            return Ok(None);
        }
        let Some(bindings) = super::path_bindings::collect(
            step,
            key.variable,
            &accumulate,
            &build,
            scope.declared_columns,
            self.budget,
            self.context.control,
        )?
        else {
            return Ok(None);
        };
        Ok(Some(FactorizedPlan {
            clause_start: scope.clause_start,
            shared: key.variable.to_owned(),
            accumulate,
            build,
            bindings,
            single_shared: true,
            shared_predicate: predicate.clone(),
            asp: None,
        }))
    }

    fn prepay(&mut self, step: &MatchStep, scope: &join::JoinClauseScope<'_>) -> QueryResult<bool> {
        let bytes = std::mem::size_of::<FactorizedPlan>()
            .saturating_add(scope.declared_columns.len() * std::mem::size_of::<FactorBinding>())
            .saturating_add(step.parts.len() * std::mem::size_of::<FactorHop>())
            .saturating_add(3 * std::mem::size_of::<Expr>());
        // Two three-name parts and five output names need only a fixed number
        // of pairwise identity/order checks; prepay them before comparisons.
        if !self.inspect(step.parts.len().saturating_mul(64), bytes)? {
            return Ok(false);
        }
        for part in &step.parts {
            for node in [Some(&part.start), part.end.as_ref()].into_iter().flatten() {
                let bytes = node
                    .variable
                    .as_ref()
                    .map_or(0, String::len)
                    .saturating_add(
                        node.labels
                            .len()
                            .saturating_mul(std::mem::size_of::<LabelId>()),
                    );
                if !self.inspect(node.labels.len().saturating_add(1), bytes)? {
                    return Ok(false);
                }
                for label in &node.labels {
                    self.context.control.work(1)?;
                    if *label <= 0 {
                        return Ok(false);
                    }
                }
            }
            let name = part
                .relationship
                .as_ref()
                .and_then(|rel| rel.variable.as_ref());
            if !self.inspect(1, name.map_or(0, String::len))? {
                return Ok(false);
            }
        }
        for name in scope.declared_columns {
            if !self.inspect(1, name.len())? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn prove_predicate(
        &mut self,
        step: &MatchStep,
        predicate: &Expr,
        identity: Option<&Expr>,
        shared: &str,
        a: &NodeSpec,
        b: &NodeSpec,
    ) -> QueryResult<bool> {
        let Some(key) = self.equality(predicate)? else {
            return Ok(false);
        };
        if key.variable != shared || !self.schema_key(&key, a, b)? {
            return Ok(false);
        }
        self.scheduled_predicates(step, identity, &key)
    }

    fn scheduled_predicates(
        &mut self,
        step: &MatchStep,
        identity: Option<&Expr>,
        key: &Equality<'_>,
    ) -> QueryResult<bool> {
        for items in &step.early_predicates {
            if !self.inspect(1, 0)? {
                return Ok(false);
            }
            for item in items {
                if !self.allowed_predicate(item, identity, key)? {
                    return Ok(false);
                }
            }
        }
        if let Some(join) = &step.join {
            for predicate in &join.predicates {
                if !self.allowed_predicate(&predicate.expression, identity, key)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn allowed_predicate(
        &mut self,
        item: &Expr,
        identity: Option<&Expr>,
        key: &Equality<'_>,
    ) -> QueryResult<bool> {
        if Some(item) == identity {
            return Ok(true);
        }
        self.same_equality(item, key)
    }

    fn equality<'e>(&mut self, expression: &'e Expr) -> QueryResult<Option<Equality<'e>>> {
        if !self.inspect(1, 0)? {
            return Ok(None);
        }
        let Expr::Binary(BinaryOp::Equal, left, right) = expression else {
            return Ok(None);
        };
        let (variable, property, value) = match (property(left), property(right)) {
            (Some((variable, property)), None) => (variable, property, right.as_ref()),
            (None, Some((variable, property))) => (variable, property, left.as_ref()),
            _ => return Ok(None),
        };
        if !self.inspect(1, variable.len().saturating_add(property.len()))? {
            return Ok(None);
        }
        let value = match value {
            Expr::Literal(Value::Integer(value)) => Some(*value),
            Expr::Parameter(name) => {
                if !self.inspect(1, name.len())? {
                    return Ok(None);
                }
                match self.context.params.get(name) {
                    Some(Value::Integer(value)) => Some(*value),
                    _ => None,
                }
            }
            _ => None,
        };
        Ok(value.map(|value| Equality {
            variable,
            property,
            value,
        }))
    }

    fn same_equality(&mut self, expression: &Expr, expected: &Equality<'_>) -> QueryResult<bool> {
        Ok(self.equality(expression)?.is_some_and(|key| {
            key.variable == expected.variable
                && key.property == expected.property
                && key.value == expected.value
        }))
    }

    fn schema_key(&mut self, key: &Equality<'_>, a: &NodeSpec, b: &NodeSpec) -> QueryResult<bool> {
        for label in a.label_names.iter().chain(&b.label_names) {
            if !self.inspect(1, label.len())? {
                return Ok(false);
            }
            if self.label_key(label, key.property)? {
                return Ok(true);
            }
            if self.budget.exhausted() {
                return Ok(false);
            }
        }
        Ok(false)
    }

    fn label_key(&mut self, label: &str, property: &str) -> QueryResult<bool> {
        let mut integer = false;
        let mut unique = false;
        let schema = self.context.schema;
        for constraint in schema.constraints.values() {
            if !self.inspect(1, 0)? {
                return Ok(false);
            }
            let SchemaTarget::Node { label: target } = &constraint.target else {
                continue;
            };
            let [key] = constraint.properties.as_slice() else {
                continue;
            };
            let bytes = target
                .len()
                .saturating_add(key.len())
                .saturating_add(label.len())
                .saturating_add(property.len());
            if !self.inspect(1, bytes)? {
                return Ok(false);
            }
            if target != label || key != property {
                continue;
            }
            match &constraint.kind {
                ConstraintDefinitionKind::Type { rule } => {
                    integer |= rule.property_type == PropertyType::Integer
                }
                ConstraintDefinitionKind::Unique | ConstraintDefinitionKind::Key => unique = true,
                ConstraintDefinitionKind::NotNull => {}
            }
            if integer && unique {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn bindings(
        &mut self,
        sources: &[(&str, FactorGroup, Binding<'_>); 5],
        declared: &[String],
    ) -> QueryResult<Option<Vec<FactorBinding>>> {
        let mut bindings = Vec::with_capacity(5);
        for name in declared {
            if !self.inspect(1, 0)?
                || bindings
                    .iter()
                    .any(|binding: &FactorBinding| binding.name == *name)
            {
                return Ok(None);
            }
            let Some((_, group, source)) = sources.iter().find(|(source, _, _)| *source == name)
            else {
                continue;
            };
            let kind = match source {
                Binding::Node(a, b) => {
                    let Some(labels) = self.labels(a, *b)? else {
                        return Ok(None);
                    };
                    FactorBindingKind::Node { labels }
                }
                Binding::Relationship(rel) => {
                    let Some(type_id) = rel.type_id else {
                        return Ok(None);
                    };
                    FactorBindingKind::Relationship { type_id }
                }
            };
            bindings.push(FactorBinding {
                name: name.clone(),
                group: *group,
                kind,
            });
        }
        Ok((bindings.len() == 5).then_some(bindings))
    }

    fn labels(&mut self, a: &NodeSpec, b: Option<&NodeSpec>) -> QueryResult<Option<Vec<LabelId>>> {
        let other = b.map_or(&[][..], |node| node.labels.as_slice());
        let mut labels = Vec::with_capacity(a.labels.len().saturating_add(other.len()));
        for label in a.labels.iter().chain(other) {
            if !self.inspect(1, 0)? {
                return Ok(None);
            }
            let mut present = false;
            for previous in &labels {
                if !self.inspect(1, 0)? {
                    return Ok(None);
                }
                present |= previous == label;
            }
            if !present {
                labels.push(*label);
            }
        }
        Ok(Some(labels))
    }
}

fn supported_parts(step: &MatchStep) -> bool {
    let relationships = step
        .parts
        .iter()
        .filter(|part| part.relationship.is_some())
        .count();
    ((2..=4).contains(&step.parts.len()) && relationships == step.parts.len())
        || (step.parts.len() == 3 && relationships == 2)
}

fn binding_count(step: &MatchStep) -> usize {
    if step.parts.iter().all(|part| part.relationship.is_some()) {
        step.parts.len().saturating_mul(2).saturating_add(1)
    } else {
        6
    }
}

fn selected_arms<'a>(
    step: &'a MatchStep,
    scope: &join::JoinClauseScope<'_>,
    indices: [usize; 2],
    shared: &str,
) -> Option<(Arm<'a>, Arm<'a>)> {
    let first = first_part(step, scope, indices)?;
    let second = if first == indices[0] {
        indices[1]
    } else {
        indices[0]
    };
    let a = arm(step, first, shared, true)?;
    let b = arm(step, second, shared, false)?;
    (a.relationship.type_id != b.relationship.type_id).then_some((a, b))
}

fn plain_part(part: &PatternPart) -> bool {
    part.path_variable.is_none()
        && plain_node(&part.start)
        && part.end.as_ref().is_some_and(plain_node)
        && part.relationship.as_ref().is_some_and(|rel| {
            rel.variable.as_ref().is_some_and(|name| !name.is_empty())
                && rel.type_id.is_some_and(|id| id > 0)
                && rel.type_name.is_some()
                && rel.direction != crate::query::plan::Direction::Undirected
                && rel.id_seek.is_none()
                && rel.index_seek.is_none()
                && !rel.impossible
        })
}

fn plain_node(node: &NodeSpec) -> bool {
    node.variable.as_ref().is_some_and(|name| !name.is_empty())
        && node.id_seek.is_none()
        && node.index_seek.is_none()
        && !node.impossible
        && node.labels.len() == node.label_names.len()
}

fn shared_node<'a>(a: &'a PatternPart, b: &'a PatternPart) -> Option<&'a str> {
    let left = [
        a.start.variable.as_deref()?,
        a.end.as_ref()?.variable.as_deref()?,
    ];
    let right = [
        b.start.variable.as_deref()?,
        b.end.as_ref()?.variable.as_deref()?,
    ];
    if left[0] == left[1] || right[0] == right[1] {
        return None;
    }
    let mut shared = None;
    for name in left {
        if right.contains(&name) {
            if shared.is_some() {
                return None;
            }
            shared = Some(name);
        }
    }
    shared
}

fn first_part(
    step: &MatchStep,
    scope: &join::JoinClauseScope<'_>,
    indices: [usize; 2],
) -> Option<usize> {
    let Some(join) = &step.join else {
        return Some(indices[0]);
    };
    if join.clause_start != scope.clause_start || join.snapshot_identity != scope.snapshot_identity
    {
        return None;
    }
    join.stages
        .iter()
        .find(|stage| indices.contains(&stage.part))
        .map(|stage| stage.part)
}

fn arm<'a>(step: &'a MatchStep, part: usize, shared: &str, accumulate: bool) -> Option<Arm<'a>> {
    let pattern = step.parts.get(part)?;
    let end = pattern.end.as_ref()?;
    let shared_first = pattern.start.variable.as_deref()? == shared;
    let (shared, leaf) = if shared_first {
        (&pattern.start, end)
    } else {
        (end, &pattern.start)
    };
    let orientation = if shared_first == accumulate {
        join::PartOrientation::Reversed
    } else {
        join::PartOrientation::Written
    };
    Some(Arm {
        part,
        leaf,
        shared,
        relationship: pattern.relationship.as_ref()?,
        orientation,
    })
}

fn factor_arm(arm: &Arm<'_>) -> FactorArm {
    FactorArm {
        hops: vec![FactorHop {
            part: arm.part,
            orientation: arm.orientation,
        }],
    }
}

fn binding_sources<'a>(
    shared: &'a str,
    a: &Arm<'a>,
    b: &Arm<'a>,
) -> Option<[(&'a str, FactorGroup, Binding<'a>); 5]> {
    Some([
        (
            shared,
            FactorGroup::Shared,
            Binding::Node(a.shared, Some(b.shared)),
        ),
        (
            a.leaf.variable.as_deref()?,
            FactorGroup::Accumulate,
            Binding::Node(a.leaf, None),
        ),
        (
            a.relationship.variable.as_deref()?,
            FactorGroup::Accumulate,
            Binding::Relationship(a.relationship),
        ),
        (
            b.leaf.variable.as_deref()?,
            FactorGroup::Build,
            Binding::Node(b.leaf, None),
        ),
        (
            b.relationship.variable.as_deref()?,
            FactorGroup::Build,
            Binding::Relationship(b.relationship),
        ),
    ])
}

fn unique_names(sources: &[(&str, FactorGroup, Binding<'_>); 5]) -> bool {
    sources
        .iter()
        .enumerate()
        .all(|(index, source)| sources[..index].iter().all(|other| other.0 != source.0))
}

fn property(expression: &Expr) -> Option<(&str, &str)> {
    let Expr::Property(base, property) = expression else {
        return None;
    };
    let Expr::Variable(variable) = base.as_ref() else {
        return None;
    };
    Some((variable, property))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::plan::Direction;

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

    fn part(start: &str, relationship: &str, end: &str) -> PatternPart {
        PatternPart {
            path_variable: None,
            start: node(start),
            end: Some(node(end)),
            relationship: Some(RelationshipSpec {
                variable: Some(relationship.into()),
                type_id: Some(1),
                type_name: Some("R".into()),
                direction: Direction::Outgoing,
                index_seek: None,
                id_seek: None,
                impossible: false,
            }),
        }
    }

    fn step(parts: Vec<PatternPart>) -> MatchStep {
        MatchStep {
            clause_start: 0,
            optional: false,
            parts,
            predicate: None,
            early_predicates: vec![],
            join: None,
        }
    }

    #[test]
    fn undirected_parts_keep_the_existing_cursor_until_the_factor_runtime_supports_them() {
        let mut candidate = part("h", "r", "a");
        assert!(plain_part(&candidate));
        candidate.relationship.as_mut().unwrap().direction = Direction::Undirected;
        assert!(!plain_part(&candidate));
    }

    #[test]
    fn normalizes_both_written_endpoint_orders_without_changing_part() {
        for (parts, a_orientation, b_orientation) in [
            (
                vec![part("h", "r", "a"), part("h", "s", "b")],
                join::PartOrientation::Reversed,
                join::PartOrientation::Written,
            ),
            (
                vec![part("a", "r", "h"), part("b", "s", "h")],
                join::PartOrientation::Written,
                join::PartOrientation::Reversed,
            ),
        ] {
            let step = step(parts);
            let a = arm(&step, 0, "h", true).unwrap();
            let b = arm(&step, 1, "h", false).unwrap();
            assert_eq!(a.orientation, a_orientation);
            assert_eq!(b.orientation, b_orientation);
            assert_eq!(a.part, 0);
            assert_eq!(b.part, 1);
            assert_eq!(a.leaf.variable.as_deref(), Some("a"));
            assert_eq!(b.leaf.variable.as_deref(), Some("b"));
        }
    }

    #[test]
    fn rejects_cycles_repeated_endpoint_names_and_disconnected_parts() {
        let first = part("h", "r", "a");
        assert!(shared_node(&first, &part("a", "s", "h")).is_none());
        assert!(shared_node(&part("h", "r", "h"), &part("h", "s", "b")).is_none());
        assert!(shared_node(&first, &part("x", "s", "b")).is_none());
        assert_eq!(shared_node(&first, &part("h", "s", "b")), Some("h"));
    }

    #[test]
    fn rejects_cross_group_relationship_or_node_aliases() {
        for parts in [
            vec![part("h", "r", "a"), part("h", "r", "b")],
            vec![part("h", "r", "a"), part("h", "a", "b")],
        ] {
            let step = step(parts);
            let a = arm(&step, 0, "h", true).unwrap();
            let b = arm(&step, 1, "h", false).unwrap();
            let sources = binding_sources("h", &a, &b).unwrap();
            assert!(!unique_names(&sources));
        }
    }
}
