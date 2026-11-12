//! Optional, bounded proofs for a complete inner MATCH. A successful proof
//! permits moving its total predicates, never removing the original WHERE.

use std::collections::BTreeMap;

use crate::cypher::Value;
use crate::query::QueryResult;
use crate::query::expression::{BinaryOp, Expr, UnaryOp};
use crate::query::plan::{MAX_DEPTH, MatchStep, PlanningBudget};
use crate::storage::{ConstraintDefinitionKind, PropertyType, SchemaTarget};

use super::*;

mod hashes;
mod predicates;
mod templates;
#[cfg(test)]
mod tests;
mod types;
mod variables;

use types::{Family, Operand};
use variables::{Kind, Occurrence, Variable};

struct Analysis<'a, 'q, 'db, 'ctl> {
    context: &'a JoinPlanningContext<'q, 'db, 'ctl>,
    budget: &'a mut PlanningBudget,
    variables: BTreeMap<&'a str, Variable<'a>>,
    predicates: Vec<JoinPredicate>,
    edges: Vec<Hyperedge>,
    templates: Vec<ParameterTemplate>,
    hashes: Vec<HashEquality>,
    cache_allowed: Option<bool>,
}

pub(crate) fn analyze_join(
    step: &MatchStep,
    scope: &JoinClauseScope<'_>,
    context: &JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<JoinAnalysis>> {
    context.control.check()?;
    if step.optional
        || !(2..=128).contains(&step.parts.len())
        || scope.snapshot_identity != context.snapshot.cache_identity()
        || budget.exhausted()
    {
        return Ok(None);
    }
    let mut analysis = Analysis {
        context,
        budget,
        variables: BTreeMap::new(),
        predicates: Vec::new(),
        edges: Vec::new(),
        templates: Vec::new(),
        hashes: Vec::new(),
        cache_allowed: None,
    };
    if !analysis.prove(step, scope.incoming)? || analysis.budget.exhausted() {
        return Ok(None);
    }
    context.control.check()?;
    Ok(Some(JoinAnalysis {
        predicates: analysis.predicates,
        edges: analysis.edges,
        templates: analysis.templates,
        hashes: analysis.hashes,
    }))
}

impl<'a> Analysis<'a, '_, '_, '_> {
    fn prove(&mut self, step: &'a MatchStep, incoming: &[String]) -> QueryResult<bool> {
        let mut predicate = 0;
        Ok(self.collect_variables(step, incoming)?
            && self.predicate_tree(step.predicate.as_ref(), 1)?
            && self.shared_nodes()?
            && self.template_tree(step.predicate.as_ref(), 1, &mut predicate)?)
    }

    fn work(&mut self, states: usize, bytes: usize) -> QueryResult<bool> {
        self.context.control.work(states.max(1) as u64)?;
        Ok(self.budget.work(states, bytes))
    }

    fn predicate_tree(&mut self, expression: Option<&Expr>, depth: usize) -> QueryResult<bool> {
        let Some(expression) = expression else {
            return Ok(true);
        };
        if depth > MAX_DEPTH || !self.work(1, 0)? {
            return Ok(false);
        }
        if let Expr::Binary(BinaryOp::And, left, right) = expression {
            return Ok(self.predicate_tree(Some(left), depth + 1)?
                && self.predicate_tree(Some(right), depth + 1)?);
        }
        let Some(guards) = self.total_predicate(expression)? else {
            return Ok(false);
        };
        if !self.charge_expression(expression, 1)?
            || !self.work(
                0,
                (2 * std::mem::size_of::<JoinPredicate>())
                    .saturating_add(guards.len().saturating_mul(std::mem::size_of::<PartSet>())),
            )?
        {
            return Ok(false);
        }
        self.predicates.push(JoinPredicate {
            expression: expression.clone(),
            guards,
        });
        Ok(true)
    }

    // Complete totality first. An earlier equality must not initialize optional
    // Index capability advice when a later residual makes reordering unsafe.
    fn template_tree(
        &mut self,
        expression: Option<&Expr>,
        depth: usize,
        predicate: &mut usize,
    ) -> QueryResult<bool> {
        let Some(expression) = expression else {
            return Ok(true);
        };
        if depth > MAX_DEPTH || !self.work(1, 0)? {
            return Ok(false);
        }
        if let Expr::Binary(BinaryOp::And, left, right) = expression {
            return Ok(self.template_tree(Some(left), depth + 1, predicate)?
                && self.template_tree(Some(right), depth + 1, predicate)?);
        }
        let index = *predicate;
        *predicate += 1;
        self.equality_templates(expression, index)
    }

    fn charge_expression(&mut self, expression: &Expr, depth: usize) -> QueryResult<bool> {
        if depth > MAX_DEPTH || !self.work(1, std::mem::size_of::<Expr>())? {
            return Ok(false);
        }
        match expression {
            Expr::Binary(_, left, right) => Ok(self.charge_expression(left, depth + 1)?
                && self.charge_expression(right, depth + 1)?),
            Expr::Unary(_, value) | Expr::IsNull { value, .. } => {
                self.charge_expression(value, depth + 1)
            }
            Expr::Property(base, name) => {
                Ok(self.work(0, name.len())? && self.charge_expression(base, depth + 1)?)
            }
            Expr::Literal(value) => {
                let bytes = crate::query::resources::value_bytes(value)?;
                self.work(0, usize::try_from(bytes).unwrap_or(usize::MAX))
            }
            Expr::Variable(name) | Expr::Parameter(name) => self.work(0, name.len()),
            Expr::List(values) => {
                for value in values {
                    if !self.charge_expression(value, depth + 1)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

fn bit(part: usize) -> PartSet {
    PartSet(1_u128 << part)
}

fn without(set: PartSet, part: usize) -> PartSet {
    PartSet(set.0 & !bit(part).0)
}

fn guards(left: PartSet, right: PartSet) -> Vec<PartSet> {
    let mut guards = Vec::new();
    if left.0 != 0 {
        guards.push(left);
    }
    if right.0 != 0 && left != right {
        guards.push(right);
    }
    guards
}
