//! Conservative expression proof; evaluation stays in the access-aware engine.

use super::*;
use crate::query::expression::{BinaryOp, UnaryOp};
use crate::query::plan::{FactorBindingKind, FactorGroup};
use crate::query::stream::factor::GroupProjection;
use crate::storage::{ConstraintDefinitionKind, PropertyType, SchemaTarget};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ValueGroup {
    Prefix,
    Arm(usize),
}

pub(super) fn predicate_group(
    expression: &Expr,
    packet: &GroupPacket,
    runtime: &ProgramRuntime<'_, '_>,
) -> QueryResult<Option<ValueGroup>> {
    runtime.control.work(1)?;
    if !packet.cartesian_proven {
        return Ok(None);
    }
    match expression {
        Expr::Literal(Value::Boolean(_) | Value::Null) => Ok(Some(ValueGroup::Prefix)),
        Expr::Parameter(name) => Ok(matches!(
            runtime.params.get(name),
            Some(Value::Boolean(_) | Value::Null)
        )
        .then_some(ValueGroup::Prefix)),
        Expr::IsNull { value, .. } => value_group(value, packet, runtime),
        Expr::Unary(UnaryOp::Not, value) => predicate_group(value, packet, runtime),
        Expr::Binary(BinaryOp::And | BinaryOp::Or | BinaryOp::Xor, left, right) => Ok(combine(
            predicate_group(left, packet, runtime)?,
            predicate_group(right, packet, runtime)?,
        )),
        Expr::Binary(
            BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::Less
            | BinaryOp::LessEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterEqual,
            left,
            right,
        ) => Ok(combine(
            value_group(left, packet, runtime)?,
            value_group(right, packet, runtime)?,
        )),
        _ => Ok(None),
    }
}

pub(super) fn value_group(
    expression: &Expr,
    packet: &GroupPacket,
    runtime: &ProgramRuntime<'_, '_>,
) -> QueryResult<Option<ValueGroup>> {
    resolve_value_group(expression, runtime, |variable, property| {
        property_group(variable, property, packet, runtime)
    })
}

pub(super) fn plan_value_group(
    expression: &Expr,
    plan: &crate::query::plan::FactorizedPlan,
    runtime: &ProgramRuntime<'_, '_>,
) -> QueryResult<Option<ValueGroup>> {
    resolve_value_group(expression, runtime, |variable, property| {
        binding_property_group(variable, property, plan, runtime)
    })
}

fn resolve_value_group(
    expression: &Expr,
    runtime: &ProgramRuntime<'_, '_>,
    property_group: impl FnOnce(&str, &str) -> QueryResult<Option<ValueGroup>>,
) -> QueryResult<Option<ValueGroup>> {
    runtime.control.work(1)?;
    match expression {
        Expr::Literal(Value::Integer(_) | Value::Null) => Ok(Some(ValueGroup::Prefix)),
        Expr::Parameter(name) => Ok(matches!(
            runtime.params.get(name),
            Some(Value::Integer(_) | Value::Null)
        )
        .then_some(ValueGroup::Prefix)),
        Expr::Property(base, property) => {
            let Expr::Variable(variable) = base.as_ref() else {
                return Ok(None);
            };
            property_group(variable, property)
        }
        _ => Ok(None),
    }
}

fn combine(left: Option<ValueGroup>, right: Option<ValueGroup>) -> Option<ValueGroup> {
    match (left?, right?) {
        (ValueGroup::Prefix, group) | (group, ValueGroup::Prefix) => Some(group),
        (left, right) if left == right => Some(left),
        _ => None,
    }
}

fn property_group(
    variable: &str,
    property: &str,
    packet: &GroupPacket,
    runtime: &ProgramRuntime<'_, '_>,
) -> QueryResult<Option<ValueGroup>> {
    let name = match &packet.projection {
        Some(projections) => {
            let Some(projection) = projections
                .iter()
                .find(|projection| projection.names().1 == variable)
            else {
                return Ok(None);
            };
            let GroupProjection::Slot { name, .. } = projection else {
                return Ok(None);
            };
            name.as_str()
        }
        None => variable,
    };
    let Some(plan) = &packet.plan else {
        return Ok(None);
    };
    binding_property_group(name, property, plan, runtime)
}

fn binding_property_group(
    name: &str,
    property: &str,
    plan: &crate::query::plan::FactorizedPlan,
    runtime: &ProgramRuntime<'_, '_>,
) -> QueryResult<Option<ValueGroup>> {
    let Some(binding) = plan.bindings.iter().find(|binding| binding.name == name) else {
        return Ok(None);
    };
    let Some(schema) = runtime.snapshot.query_schema_state() else {
        return Ok(None);
    };
    for constraint in schema.constraints.values() {
        runtime.control.work(1)?;
        let ConstraintDefinitionKind::Type { rule } = &constraint.kind else {
            continue;
        };
        if rule.property_type != PropertyType::Integer
            || constraint.properties.as_slice() != [property]
        {
            continue;
        }
        let proven = match (&binding.kind, &constraint.target) {
            (FactorBindingKind::Node { labels }, SchemaTarget::Node { label }) => {
                let mut found = false;
                for id in labels {
                    runtime.control.work(1)?;
                    found |= crate::storage::with_label_name(runtime.connection, *id, |name| {
                        Ok::<_, QueryError>(name == Some(label.as_str()))
                    })?;
                }
                found
            }
            (
                FactorBindingKind::Relationship { type_id },
                SchemaTarget::Relationship { relationship_type },
            ) => {
                crate::storage::with_relationship_type_name(runtime.connection, *type_id, |name| {
                    Ok::<_, QueryError>(name == Some(relationship_type.as_str()))
                })?
            }
            _ => false,
        };
        if proven {
            return Ok(Some(match binding.group {
                FactorGroup::Shared => ValueGroup::Prefix,
                FactorGroup::Accumulate => ValueGroup::Arm(0),
                FactorGroup::Build => ValueGroup::Arm(1),
            }));
        }
    }
    Ok(None)
}
