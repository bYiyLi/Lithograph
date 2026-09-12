use crate::query::QueryResult;

use super::{Expr, InterpolatedPart};

pub(crate) fn transform_expression<F>(expression: &Expr, transform: &mut F) -> QueryResult<Expr>
where
    F: FnMut(&Expr) -> QueryResult<Option<Expr>>,
{
    if let Some(replacement) = transform(expression)? {
        return Ok(replacement);
    }
    match expression {
        Expr::List(_)
        | Expr::Map(_)
        | Expr::Function { .. }
        | Expr::Property(_, _)
        | Expr::Unary(_, _)
        | Expr::Binary(_, _, _) => transform_container(expression, transform),
        Expr::Case { .. } | Expr::Interpolated(_) => transform_branching(expression, transform),
        Expr::ListComprehension { .. }
        | Expr::ListPredicate { .. }
        | Expr::Reduce { .. }
        | Expr::AllReduce { .. }
        | Expr::MapProjection { .. } => transform_collection(expression, transform),
        Expr::Subscript { .. }
        | Expr::IsNull { .. }
        | Expr::NormalizedPredicate { .. }
        | Expr::TypePredicate { .. }
        | Expr::LabelPredicate { .. } => transform_postfix(expression, transform),
        Expr::Literal(_)
        | Expr::Variable(_)
        | Expr::Parameter(_)
        | Expr::PatternPredicate(_)
        | Expr::PatternComprehension(_)
        | Expr::Subquery { .. } => Ok(expression.clone()),
    }
}

fn transform_container<F>(expression: &Expr, transform: &mut F) -> QueryResult<Expr>
where
    F: FnMut(&Expr) -> QueryResult<Option<Expr>>,
{
    Ok(match expression {
        Expr::List(values) => Expr::List(
            values
                .iter()
                .map(|value| transform_expression(value, transform))
                .collect::<QueryResult<_>>()?,
        ),
        Expr::Map(values) => Expr::Map(
            values
                .iter()
                .map(|(key, value)| Ok((key.clone(), transform_expression(value, transform)?)))
                .collect::<QueryResult<_>>()?,
        ),
        Expr::Function {
            name,
            args,
            distinct,
            star,
        } => Expr::Function {
            name: name.clone(),
            args: args
                .iter()
                .map(|value| transform_expression(value, transform))
                .collect::<QueryResult<_>>()?,
            distinct: *distinct,
            star: *star,
        },
        Expr::Property(base, key) => Expr::Property(
            Box::new(transform_expression(base, transform)?),
            key.clone(),
        ),
        Expr::Unary(operator, value) => {
            Expr::Unary(*operator, Box::new(transform_expression(value, transform)?))
        }
        Expr::Binary(operator, left, right) => Expr::Binary(
            *operator,
            Box::new(transform_expression(left, transform)?),
            Box::new(transform_expression(right, transform)?),
        ),
        _ => unreachable!("container expression dispatch is exhaustive"),
    })
}

fn transform_branching<F>(expression: &Expr, transform: &mut F) -> QueryResult<Expr>
where
    F: FnMut(&Expr) -> QueryResult<Option<Expr>>,
{
    Ok(match expression {
        Expr::Case {
            operand,
            alternatives,
            fallback,
        } => Expr::Case {
            operand: transform_optional(operand.as_deref(), transform)?,
            alternatives: alternatives
                .iter()
                .map(|(condition, value)| {
                    Ok((
                        transform_expression(condition, transform)?,
                        transform_expression(value, transform)?,
                    ))
                })
                .collect::<QueryResult<_>>()?,
            fallback: transform_optional(fallback.as_deref(), transform)?,
        },
        Expr::Interpolated(parts) => Expr::Interpolated(
            parts
                .iter()
                .map(|part| match part {
                    InterpolatedPart::Text(value) => Ok(InterpolatedPart::Text(value.clone())),
                    InterpolatedPart::Expression(value) => Ok(InterpolatedPart::Expression(
                        transform_expression(value, transform)?,
                    )),
                })
                .collect::<QueryResult<_>>()?,
        ),
        _ => unreachable!("branching expression dispatch is exhaustive"),
    })
}

fn transform_collection<F>(expression: &Expr, transform: &mut F) -> QueryResult<Expr>
where
    F: FnMut(&Expr) -> QueryResult<Option<Expr>>,
{
    match expression {
        Expr::ListComprehension { .. } | Expr::ListPredicate { .. } => {
            transform_list_collection(expression, transform)
        }
        Expr::Reduce { .. } | Expr::AllReduce { .. } => transform_reduction(expression, transform),
        Expr::MapProjection {
            base,
            include_all,
            entries,
        } => Ok(Expr::MapProjection {
            base: base.clone(),
            include_all: *include_all,
            entries: entries
                .iter()
                .map(|(key, value)| Ok((key.clone(), transform_expression(value, transform)?)))
                .collect::<QueryResult<_>>()?,
        }),
        _ => unreachable!("collection expression dispatch is exhaustive"),
    }
}

fn transform_list_collection<F>(expression: &Expr, transform: &mut F) -> QueryResult<Expr>
where
    F: FnMut(&Expr) -> QueryResult<Option<Expr>>,
{
    match expression {
        Expr::ListComprehension {
            variable,
            collection,
            predicate,
            projection,
        } => Ok(Expr::ListComprehension {
            variable: variable.clone(),
            collection: Box::new(transform_expression(collection, transform)?),
            predicate: transform_optional(predicate.as_deref(), transform)?,
            projection: transform_optional(projection.as_deref(), transform)?,
        }),
        Expr::ListPredicate {
            kind,
            variable,
            collection,
            predicate,
        } => Ok(Expr::ListPredicate {
            kind: *kind,
            variable: variable.clone(),
            collection: Box::new(transform_expression(collection, transform)?),
            predicate: transform_optional(predicate.as_deref(), transform)?,
        }),
        _ => unreachable!("list collection dispatch is exhaustive"),
    }
}

fn transform_reduction<F>(expression: &Expr, transform: &mut F) -> QueryResult<Expr>
where
    F: FnMut(&Expr) -> QueryResult<Option<Expr>>,
{
    match expression {
        Expr::Reduce {
            accumulator,
            initial,
            variable,
            collection,
            reduction,
        } => Ok(Expr::Reduce {
            accumulator: accumulator.clone(),
            initial: Box::new(transform_expression(initial, transform)?),
            variable: variable.clone(),
            collection: Box::new(transform_expression(collection, transform)?),
            reduction: Box::new(transform_expression(reduction, transform)?),
        }),
        Expr::AllReduce {
            accumulator,
            initial,
            variable,
            collection,
            reduction,
            predicate,
        } => Ok(Expr::AllReduce {
            accumulator: accumulator.clone(),
            initial: Box::new(transform_expression(initial, transform)?),
            variable: variable.clone(),
            collection: Box::new(transform_expression(collection, transform)?),
            reduction: Box::new(transform_expression(reduction, transform)?),
            predicate: Box::new(transform_expression(predicate, transform)?),
        }),
        _ => unreachable!("reduction dispatch is exhaustive"),
    }
}

fn transform_postfix<F>(expression: &Expr, transform: &mut F) -> QueryResult<Expr>
where
    F: FnMut(&Expr) -> QueryResult<Option<Expr>>,
{
    Ok(match expression {
        Expr::Subscript {
            base,
            start,
            end,
            slice,
        } => Expr::Subscript {
            base: Box::new(transform_expression(base, transform)?),
            start: transform_optional(start.as_deref(), transform)?,
            end: transform_optional(end.as_deref(), transform)?,
            slice: *slice,
        },
        Expr::IsNull { value, negated } => Expr::IsNull {
            value: Box::new(transform_expression(value, transform)?),
            negated: *negated,
        },
        Expr::NormalizedPredicate {
            value,
            form,
            negated,
        } => Expr::NormalizedPredicate {
            value: Box::new(transform_expression(value, transform)?),
            form: *form,
            negated: *negated,
        },
        Expr::TypePredicate {
            value,
            type_spec,
            negated,
        } => Expr::TypePredicate {
            value: Box::new(transform_expression(value, transform)?),
            type_spec: type_spec.clone(),
            negated: *negated,
        },
        Expr::LabelPredicate {
            value,
            name_expression,
            negated,
        } => Expr::LabelPredicate {
            value: Box::new(transform_expression(value, transform)?),
            name_expression: name_expression.clone(),
            negated: *negated,
        },
        _ => unreachable!("postfix expression dispatch is exhaustive"),
    })
}

fn transform_optional<F>(value: Option<&Expr>, transform: &mut F) -> QueryResult<Option<Box<Expr>>>
where
    F: FnMut(&Expr) -> QueryResult<Option<Expr>>,
{
    value
        .map(|value| transform_expression(value, transform))
        .transpose()
        .map(|value| value.map(Box::new))
}
