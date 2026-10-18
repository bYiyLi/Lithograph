use std::collections::BTreeMap;

use crate::cypher::Value;
use crate::query::{QueryError, QueryErrorKind, QueryResult};

use super::{Expr, ListPredicateKind};

type ScopedEvaluate<'a> = dyn FnMut(&Expr, &BTreeMap<String, Value>) -> QueryResult<Value> + 'a;

struct ScopedContext<'a> {
    aliases: &'a BTreeMap<String, Value>,
    evaluate: &'a mut ScopedEvaluate<'a>,
}

impl ScopedContext<'_> {
    fn evaluate(&mut self, expression: &Expr) -> QueryResult<Value> {
        (self.evaluate)(expression, self.aliases)
    }

    fn evaluate_with_aliases(
        &mut self,
        expression: &Expr,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<Value> {
        (self.evaluate)(expression, aliases)
    }
}

pub(crate) fn evaluate_scoped_expression(
    expression: &Expr,
    aliases: &BTreeMap<String, Value>,
    evaluate: &mut impl FnMut(&Expr, &BTreeMap<String, Value>) -> QueryResult<Value>,
) -> QueryResult<Value> {
    let context = &mut ScopedContext { aliases, evaluate };
    match expression {
        Expr::ListComprehension {
            variable,
            collection,
            predicate,
            projection,
        } => evaluate_list_comprehension(
            variable,
            collection,
            predicate.as_deref(),
            projection.as_deref(),
            context,
        ),
        Expr::ListPredicate {
            kind,
            variable,
            collection,
            predicate,
        } => evaluate_list_predicate(*kind, variable, collection, predicate.as_deref(), context),
        Expr::Reduce {
            accumulator,
            initial,
            variable,
            collection,
            reduction,
        } => evaluate_reduction(
            accumulator,
            initial,
            variable,
            collection,
            reduction,
            None,
            context,
        ),
        Expr::AllReduce {
            accumulator,
            initial,
            variable,
            collection,
            reduction,
            predicate,
        } => evaluate_reduction(
            accumulator,
            initial,
            variable,
            collection,
            reduction,
            Some(predicate),
            context,
        ),
        _ => Err(QueryError::internal(
            "non-scoped expression reached the scoped evaluator",
        )),
    }
}

fn evaluate_list_comprehension(
    variable: &str,
    collection: &Expr,
    predicate_expression: Option<&Expr>,
    projection: Option<&Expr>,
    context: &mut ScopedContext<'_>,
) -> QueryResult<Value> {
    let Some(values) = evaluate_list_input(collection, context, "list comprehension")? else {
        return Ok(Value::Null);
    };
    let mut output = Vec::new();
    for value in values {
        let mut local = context.aliases.clone();
        local.insert(variable.to_owned(), value.clone());
        let keep = predicate_expression.map_or(Ok(true), |predicate_expression| {
            let value = context.evaluate_with_aliases(predicate_expression, &local)?;
            match value {
                Value::Boolean(value) => Ok(value),
                Value::Null => Ok(false),
                _ => Err(QueryError::new(
                    QueryErrorKind::Type,
                    "list comprehension WHERE must be Boolean or null",
                )),
            }
        })?;
        if keep {
            output.push(projection.map_or(Ok(value), |projection| {
                context.evaluate_with_aliases(projection, &local)
            })?);
        }
    }
    Ok(Value::List(output))
}

fn evaluate_list_predicate(
    kind: ListPredicateKind,
    variable: &str,
    collection: &Expr,
    predicate_expression: Option<&Expr>,
    context: &mut ScopedContext<'_>,
) -> QueryResult<Value> {
    let Some(values) = evaluate_list_input(collection, context, "list predicate")? else {
        return Ok(Value::Null);
    };
    let counts = evaluate_list_predicate_values(variable, values, predicate_expression, context)?;
    Ok(list_predicate_result(kind, counts))
}

#[derive(Default)]
struct PredicateCounts {
    true_count: usize,
    false_count: usize,
    has_null: bool,
}

fn evaluate_list_predicate_values(
    variable: &str,
    values: Vec<Value>,
    predicate_expression: Option<&Expr>,
    context: &mut ScopedContext<'_>,
) -> QueryResult<PredicateCounts> {
    let mut counts = PredicateCounts::default();
    for value in values {
        let mut local = context.aliases.clone();
        local.insert(variable.to_owned(), value.clone());
        let tested = match predicate_expression {
            Some(predicate_expression) => {
                context.evaluate_with_aliases(predicate_expression, &local)?
            }
            None => value,
        };
        match tested {
            Value::Boolean(true) => counts.true_count += 1,
            Value::Boolean(false) => counts.false_count += 1,
            Value::Null => counts.has_null = true,
            _ => {
                return Err(QueryError::new(
                    QueryErrorKind::Type,
                    "list predicate WHERE must be Boolean or null",
                ));
            }
        }
    }
    Ok(counts)
}

fn list_predicate_result(kind: ListPredicateKind, counts: PredicateCounts) -> Value {
    let result = match kind {
        ListPredicateKind::All if counts.false_count > 0 => Some(false),
        ListPredicateKind::All if counts.has_null => None,
        ListPredicateKind::All => Some(true),
        ListPredicateKind::Any if counts.true_count > 0 => Some(true),
        ListPredicateKind::Any if counts.has_null => None,
        ListPredicateKind::Any => Some(false),
        ListPredicateKind::None if counts.true_count > 0 => Some(false),
        ListPredicateKind::None if counts.has_null => None,
        ListPredicateKind::None => Some(true),
        ListPredicateKind::Single if counts.true_count > 1 => Some(false),
        ListPredicateKind::Single if counts.has_null => None,
        ListPredicateKind::Single => Some(counts.true_count == 1),
    };
    result.map(Value::Boolean).unwrap_or(Value::Null)
}

fn evaluate_reduction(
    accumulator_name: &str,
    initial: &Expr,
    variable: &str,
    collection: &Expr,
    reduction: &Expr,
    predicate_expression: Option<&Expr>,
    context: &mut ScopedContext<'_>,
) -> QueryResult<Value> {
    let mut accumulator = context.evaluate(initial)?;
    let Some(values) = evaluate_list_input(collection, context, "reduction")? else {
        return Ok(Value::Null);
    };
    let mut has_null_predicate = false;
    for value in values {
        let mut local = context.aliases.clone();
        local.insert(accumulator_name.to_owned(), accumulator);
        local.insert(variable.to_owned(), value);
        accumulator = context.evaluate_with_aliases(reduction, &local)?;
        if let Some(predicate_expression) = predicate_expression {
            local.insert(accumulator_name.to_owned(), accumulator.clone());
            match context.evaluate_with_aliases(predicate_expression, &local)? {
                Value::Boolean(true) => {}
                Value::Boolean(false) => return Ok(Value::Boolean(false)),
                Value::Null => has_null_predicate = true,
                _ => {
                    return Err(QueryError::new(
                        QueryErrorKind::Type,
                        "allReduce() predicate must be Boolean or null",
                    ));
                }
            }
        }
    }
    if predicate_expression.is_some() {
        Ok(if has_null_predicate {
            Value::Null
        } else {
            Value::Boolean(true)
        })
    } else {
        Ok(accumulator)
    }
}

fn evaluate_list_input(
    expression: &Expr,
    context: &mut ScopedContext<'_>,
    operation: &str,
) -> QueryResult<Option<Vec<Value>>> {
    match context.evaluate(expression)? {
        Value::List(values) => Ok(Some(values)),
        Value::Null => Ok(None),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            format!("{operation} IN input must be a List or null"),
        )),
    }
}
