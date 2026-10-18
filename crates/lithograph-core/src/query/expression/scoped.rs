use std::collections::BTreeMap;

use crate::cypher::Value;
use crate::query::resources::{allocation_bytes, checked_add, map_entry_bytes};
use crate::query::{ExecutionControl, MemoryReservation, QueryError, QueryErrorKind, QueryResult};

use super::controlled::value_bytes_with_control;
use super::evaluate::EvaluatedValue;
use super::{
    Expr, ListPredicateKind, clone_string_controlled as clone_name,
    clone_value_unreserved_controlled,
};

struct ScopedValue {
    value: Value,
    memory: Option<MemoryReservation>,
}

type ScopedEvaluate<'a> =
    dyn FnMut(&Expr, &BTreeMap<String, Value>) -> QueryResult<ScopedValue> + 'a;

struct ScopedContext<'a, 'control> {
    aliases: &'a BTreeMap<String, Value>,
    evaluate: &'a mut ScopedEvaluate<'a>,
    control: Option<&'control ExecutionControl<'control>>,
}

impl ScopedContext<'_, '_> {
    fn evaluate(&mut self, expression: &Expr) -> QueryResult<ScopedValue> {
        (self.evaluate)(expression, self.aliases)
    }

    fn evaluate_with_aliases(
        &mut self,
        expression: &Expr,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<ScopedValue> {
        (self.evaluate)(expression, aliases)
    }

    fn work(&self) -> QueryResult<()> {
        self.control.map_or(Ok(()), |control| control.work(1))
    }

    fn reserve(&self, bytes: u64) -> QueryResult<Option<MemoryReservation>> {
        self.control
            .map(|control| control.reserve(bytes))
            .transpose()
    }

    fn reserve_value(&self, value: &Value) -> QueryResult<Option<MemoryReservation>> {
        self.control
            .map(|control| control.reserve(value_bytes_with_control(value, Some(control))?))
            .transpose()
    }

    /// The local bindings own copies of the enclosing aliases. Reserve all of
    /// those copies before constructing the map; the caller holds this lease
    /// until the iteration's local scope is dropped. Additional bindings are
    /// moved or separately reserved by the caller.
    fn local_aliases(
        &self,
        additional_names: &[&str],
    ) -> QueryResult<(BTreeMap<String, Value>, Option<MemoryReservation>)> {
        let memory = self.reserve_local_aliases(additional_names)?;
        let mut local = BTreeMap::new();
        for (name, value) in self.aliases {
            self.work()?;
            local.insert(
                clone_name(name, self.control)?,
                clone_value_unreserved_controlled(value, self.control)?,
            );
        }
        Ok((local, memory))
    }

    fn reserve_local_aliases(
        &self,
        additional_names: &[&str],
    ) -> QueryResult<Option<MemoryReservation>> {
        let Some(control) = self.control else {
            return Ok(None);
        };
        let mut bytes = 0;
        for (name, value) in self.aliases {
            control.work(1)?;
            bytes = checked_add(bytes, local_entry_bytes(name)?)?;
            bytes = checked_add(bytes, value_bytes_with_control(value, Some(control))?)?;
        }
        for name in additional_names {
            bytes = checked_add(bytes, local_entry_bytes(name)?)?;
        }
        Ok(Some(control.reserve(bytes)?))
    }

    fn local_value_aliases(
        &self,
        variable: &str,
        value: &Value,
    ) -> QueryResult<(BTreeMap<String, Value>, Option<MemoryReservation>)> {
        let (mut local, mut memory) = self.local_aliases(&[variable])?;
        let value_memory = self.reserve_value(value)?;
        if let Some(memory) = &mut memory
            && let Some(value_memory) = value_memory
        {
            memory.merge(value_memory)?;
        }
        local.insert(
            clone_name(variable, self.control)?,
            clone_value_unreserved_controlled(value, self.control)?,
        );
        Ok((local, memory))
    }
}

fn local_entry_bytes(name: &str) -> QueryResult<u64> {
    // Reserve a conservative BTreeMap node bound even for a single entry. The
    // value's heap allocation is accounted for separately.
    checked_add(
        map_entry_bytes::<Value>()?,
        allocation_bytes::<u8>(name.len())?,
    )
}

#[cfg(test)]
pub(crate) fn evaluate_scoped_expression_controlled(
    expression: &Expr,
    aliases: &BTreeMap<String, Value>,
    evaluate: &mut impl FnMut(&Expr, &BTreeMap<String, Value>) -> QueryResult<Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Value> {
    let mut result_memory = None;
    evaluate_scoped_expression_inner(
        expression,
        aliases,
        &mut |expression, aliases| {
            let value = evaluate(expression, aliases)?;
            let memory = super::controlled::reserve_value(&value, control)?;
            Ok(ScopedValue { value, memory })
        },
        control,
        &mut result_memory,
    )
}

pub(crate) fn evaluate_scoped_expression_with_memory(
    expression: &Expr,
    aliases: &BTreeMap<String, Value>,
    evaluate: &mut impl FnMut(&Expr, &BTreeMap<String, Value>) -> QueryResult<EvaluatedValue>,
    control: &ExecutionControl<'_>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    evaluate_scoped_expression_inner(
        expression,
        aliases,
        &mut |expression, aliases| {
            let EvaluatedValue { value, reservation } = evaluate(expression, aliases)?;
            Ok(ScopedValue {
                value,
                memory: Some(reservation),
            })
        },
        Some(control),
        result_memory,
    )
}

fn evaluate_scoped_expression_inner(
    expression: &Expr,
    aliases: &BTreeMap<String, Value>,
    evaluate: &mut impl FnMut(&Expr, &BTreeMap<String, Value>) -> QueryResult<ScopedValue>,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    if let Some(control) = control {
        control.check()?;
    }
    let context = &mut ScopedContext {
        aliases,
        evaluate,
        control,
    };
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
            result_memory,
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
            (accumulator, initial),
            variable,
            collection,
            reduction,
            None,
            context,
            result_memory,
        ),
        Expr::AllReduce {
            accumulator,
            initial,
            variable,
            collection,
            reduction,
            predicate,
        } => evaluate_reduction(
            (accumulator, initial),
            variable,
            collection,
            reduction,
            Some(predicate),
            context,
            result_memory,
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
    context: &mut ScopedContext<'_, '_>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    let Some((values, _input_memory)) =
        evaluate_list_input(collection, context, "list comprehension")?
    else {
        return Ok(Value::Null);
    };
    let mut output = Vec::new();
    let mut output_memory = context.reserve(allocation_bytes::<Value>(1)?)?;
    for value in values {
        context.work()?;
        let (local, _local_memory) = context.local_value_aliases(variable, &value)?;
        let keep = predicate_expression.map_or(Ok(true), |predicate| {
            evaluate_scoped_predicate(predicate, &local, context, "list comprehension WHERE")
                .map(|value| value.unwrap_or(false))
        })?;
        if keep {
            let projected = evaluate_list_projection(projection, value, &local, context)?;
            push_output(&mut output, projected, &mut output_memory, context)?;
        }
    }
    *result_memory = output_memory;
    Ok(Value::List(output))
}

fn evaluate_list_projection(
    projection: Option<&Expr>,
    value: Value,
    aliases: &BTreeMap<String, Value>,
    context: &mut ScopedContext<'_, '_>,
) -> QueryResult<ScopedValue> {
    match projection {
        Some(projection) => context.evaluate_with_aliases(projection, aliases),
        None => Ok(ScopedValue {
            memory: context.reserve_value(&value)?,
            value,
        }),
    }
}

fn evaluate_list_predicate(
    kind: ListPredicateKind,
    variable: &str,
    collection: &Expr,
    predicate_expression: Option<&Expr>,
    context: &mut ScopedContext<'_, '_>,
) -> QueryResult<Value> {
    let Some((values, _input_memory)) = evaluate_list_input(collection, context, "list predicate")?
    else {
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
    context: &mut ScopedContext<'_, '_>,
) -> QueryResult<PredicateCounts> {
    let mut counts = PredicateCounts::default();
    for value in values {
        context.work()?;
        let (local, _local_memory) = context.local_value_aliases(variable, &value)?;
        let tested = match predicate_expression {
            Some(predicate_expression) => {
                context.evaluate_with_aliases(predicate_expression, &local)?
            }
            None => ScopedValue {
                value,
                memory: None,
            },
        };
        match tested.value {
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
    initial_accumulator: (&str, &Expr),
    variable: &str,
    collection: &Expr,
    reduction: &Expr,
    predicate_expression: Option<&Expr>,
    context: &mut ScopedContext<'_, '_>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    let (accumulator_name, initial) = initial_accumulator;
    let ScopedValue {
        value: mut accumulator,
        memory: mut accumulator_memory,
    } = context.evaluate(initial)?;
    let Some((values, _input_memory)) = evaluate_list_input(collection, context, "reduction")?
    else {
        return Ok(Value::Null);
    };
    let mut has_null_predicate = false;
    for value in values {
        // Declare the clone's claim before `local` so error propagation also
        // drops the owning bindings before releasing their reservation.
        let mut predicate_memory = None;
        context.work()?;
        let (mut local, _alias_memory) = context.local_aliases(&[accumulator_name, variable])?;
        local.insert(clone_name(accumulator_name, context.control)?, accumulator);
        local.insert(clone_name(variable, context.control)?, value);
        let ScopedValue {
            value: next_accumulator,
            memory: next_memory,
        } = context.evaluate_with_aliases(reduction, &local)?;
        // The previous accumulator is still owned by `local` while reduction
        // evaluates its replacement. Keep both claims until that scope ends.
        if let Some(predicate_expression) = predicate_expression {
            match evaluate_reduction_predicate(
                accumulator_name,
                &next_accumulator,
                predicate_expression,
                &mut local,
                context,
                &mut predicate_memory,
            )? {
                Some(true) => {}
                Some(false) => {
                    drop(local);
                    drop(predicate_memory);
                    return Ok(Value::Boolean(false));
                }
                None => has_null_predicate = true,
            }
        }
        drop(local);
        drop(predicate_memory);
        accumulator = next_accumulator;
        accumulator_memory = next_memory;
    }
    reduction_result(
        ScopedValue {
            value: accumulator,
            memory: accumulator_memory,
        },
        predicate_expression.is_some(),
        has_null_predicate,
        result_memory,
    )
}

fn evaluate_reduction_predicate(
    accumulator_name: &str,
    accumulator: &Value,
    predicate: &Expr,
    aliases: &mut BTreeMap<String, Value>,
    context: &mut ScopedContext<'_, '_>,
    memory: &mut Option<MemoryReservation>,
) -> QueryResult<Option<bool>> {
    *memory = context.reserve_value(accumulator)?;
    aliases.insert(
        clone_name(accumulator_name, context.control)?,
        clone_value_unreserved_controlled(accumulator, context.control)?,
    );
    evaluate_scoped_predicate(predicate, aliases, context, "allReduce() predicate")
}

fn evaluate_scoped_predicate(
    expression: &Expr,
    aliases: &BTreeMap<String, Value>,
    context: &mut ScopedContext<'_, '_>,
    operation: &str,
) -> QueryResult<Option<bool>> {
    let value = context.evaluate_with_aliases(expression, aliases)?;
    match value.value {
        Value::Boolean(value) => Ok(Some(value)),
        Value::Null => Ok(None),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            format!("{operation} must be Boolean or null"),
        )),
    }
}

fn reduction_result(
    accumulator: ScopedValue,
    is_predicate: bool,
    has_null_predicate: bool,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    if is_predicate {
        Ok(if has_null_predicate {
            Value::Null
        } else {
            Value::Boolean(true)
        })
    } else {
        *result_memory = accumulator.memory;
        Ok(accumulator.value)
    }
}

fn evaluate_list_input(
    expression: &Expr,
    context: &mut ScopedContext<'_, '_>,
    operation: &str,
) -> QueryResult<Option<(Vec<Value>, Option<MemoryReservation>)>> {
    let ScopedValue { value, memory } = context.evaluate(expression)?;
    match value {
        Value::List(values) => Ok(Some((values, memory))),
        Value::Null => Ok(None),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            format!("{operation} IN input must be a List or null"),
        )),
    }
}

fn push_output(
    output: &mut Vec<Value>,
    value: ScopedValue,
    memory: &mut Option<MemoryReservation>,
    context: &ScopedContext<'_, '_>,
) -> QueryResult<()> {
    if let Some(control) = context.control {
        control.check()?;
    }
    ensure_output_capacity(output, memory)?;
    let ScopedValue {
        value,
        memory: value_memory,
    } = value;
    if let Some(memory) = memory
        && let Some(mut value_memory) = value_memory
    {
        // The destination Vec already owns the inline slot reservation. Move
        // the result's remaining claim without releasing its heap budget.
        value_memory.resize(
            value_memory
                .bytes()
                .saturating_sub(allocation_bytes::<Value>(1)?),
        )?;
        memory.merge(value_memory)?;
    }
    output.push(value);
    Ok(())
}

fn ensure_output_capacity(
    output: &mut Vec<Value>,
    memory: &mut Option<MemoryReservation>,
) -> QueryResult<()> {
    if output.len() == output.capacity() {
        let old_capacity = output.capacity();
        let capacity = old_capacity.saturating_mul(2).max(4);
        let additional = capacity - old_capacity;
        if let Some(memory) = memory {
            memory.grow(allocation_bytes::<Value>(additional)?)?;
        }
        output
            .try_reserve_exact(capacity - output.len())
            .map_err(|_| {
                crate::query::resources::memory_error("list comprehension allocation failed")
            })?;
        if let Some(memory) = memory
            && output.capacity() > capacity
        {
            memory.grow(allocation_bytes::<Value>(output.capacity() - capacity)?)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use crate::query::QueryResources;
    use crate::query::resources::value_bytes;

    use super::*;

    fn reduction() -> Expr {
        Expr::Reduce {
            accumulator: "acc".to_owned(),
            initial: Box::new(Expr::Literal(Value::Integer(0))),
            variable: "item".to_owned(),
            collection: Box::new(Expr::Variable("input".to_owned())),
            reduction: Box::new(Expr::Variable("acc".to_owned())),
        }
    }

    fn list_comprehension() -> Expr {
        Expr::ListComprehension {
            variable: "item".to_owned(),
            collection: Box::new(Expr::Variable("input".to_owned())),
            predicate: None,
            projection: None,
        }
    }

    fn evaluate_reduction_fixture(
        expression: &Expr,
        aliases: &BTreeMap<String, Value>,
        count: usize,
    ) -> QueryResult<Value> {
        Ok(match expression {
            Expr::Literal(value) => value.clone(),
            Expr::Variable(name) if name == "input" => Value::List(vec![Value::Integer(1); count]),
            Expr::Variable(name) => aliases[name].clone(),
            _ => unreachable!(),
        })
    }

    #[test]
    fn all_reduce_predicate_clone_stays_leased_until_its_local_binding_is_dropped() {
        for predicate in [
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Null,
            Value::Integer(1),
        ] {
            assert_predicate_clone_lease(predicate);
        }
    }

    fn assert_predicate_clone_lease(predicate: Value) {
        let resources = QueryResources::new(Some(64 * 1_024));
        let never = || false;
        let control = ExecutionControl::new(&resources, &never);
        let accumulator = Value::String("x".repeat(4_096));
        let original_memory = control.reserve(value_bytes(&accumulator).unwrap()).unwrap();
        let aliases = BTreeMap::new();
        let mut evaluate = |expression: &Expr, _: &BTreeMap<String, Value>| {
            let memory = Some(control.reserve(allocation_bytes::<Value>(1)?)?);
            let Expr::Literal(value) = expression else {
                unreachable!();
            };
            Ok(ScopedValue {
                value: value.clone(),
                memory,
            })
        };
        let mut context = ScopedContext {
            aliases: &aliases,
            evaluate: &mut evaluate,
            control: Some(&control),
        };
        let mut predicate_memory = None;
        let (mut local, local_memory) = context.local_aliases(&["acc"]).unwrap();
        let baseline = resources.used();
        let result = evaluate_reduction_predicate(
            "acc",
            &accumulator,
            &Expr::Literal(predicate.clone()),
            &mut local,
            &mut context,
            &mut predicate_memory,
        );
        match predicate {
            Value::Boolean(value) => assert_eq!(result.unwrap(), Some(value)),
            Value::Null => assert_eq!(result.unwrap(), None),
            _ => assert_eq!(result.unwrap_err().kind, QueryErrorKind::Type),
        }
        assert_eq!(local.get("acc"), Some(&accumulator));
        let expected = baseline + value_bytes(&accumulator).unwrap();
        assert_eq!(resources.used(), expected);
        drop(local);
        assert_eq!(resources.used(), expected);
        drop(predicate_memory);
        assert_eq!(resources.used(), baseline);
        drop(local_memory);
        drop(original_memory);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn tracked_list_output_transfers_its_existing_reservation_to_the_caller() {
        let expression = list_comprehension();
        let resources = QueryResources::new(Some(64 * 1_024));
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);
        let mut result_memory = None;
        let value = evaluate_scoped_expression_with_memory(
            &expression,
            &BTreeMap::new(),
            &mut |_, _| {
                let reservation = control.reserve(allocation_bytes::<Value>(3)? + 4_096)?;
                let value = Value::List(vec![Value::String("x".repeat(2_048)); 2]);
                Ok(EvaluatedValue { value, reservation })
            },
            &control,
            &mut result_memory,
        )
        .unwrap();
        let memory = result_memory.expect("returned list still owns its lease");
        assert_eq!(memory.bytes(), value_bytes(&value).unwrap());
        assert_eq!(resources.used(), memory.bytes());
        assert_eq!(
            value,
            Value::List(vec![Value::String("x".repeat(2_048)); 2])
        );
        drop(value);
        drop(memory);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn tracked_reduction_transfers_the_last_accumulator_claim() {
        let resources = QueryResources::new(Some(64 * 1_024));
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);
        let mut result_memory = None;
        let value = evaluate_scoped_expression_with_memory(
            &reduction(),
            &BTreeMap::new(),
            &mut |expression, aliases| match expression {
                Expr::Variable(name) if name == "input" => {
                    let reservation = control.reserve(allocation_bytes::<Value>(9)?)?;
                    Ok(EvaluatedValue {
                        value: Value::List(vec![Value::Integer(1); 8]),
                        reservation,
                    })
                }
                expression => {
                    let reservation = control.reserve(allocation_bytes::<Value>(1)?)?;
                    let value = match expression {
                        Expr::Literal(value) => value.clone(),
                        Expr::Variable(name) => aliases[name].clone(),
                        _ => unreachable!(),
                    };
                    Ok(EvaluatedValue { value, reservation })
                }
            },
            &control,
            &mut result_memory,
        )
        .unwrap();
        assert_eq!(value, Value::Integer(0));
        assert_eq!(resources.used(), value_bytes(&value).unwrap());
        drop(result_memory);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn reduction_releases_previous_accumulator_and_local_bindings() {
        let count = 10_000;
        let input_bytes = value_bytes(&Value::List(vec![Value::Integer(1); count])).unwrap();
        let resources = QueryResources::new(Some(input_bytes + 16_384));
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);
        let result = evaluate_scoped_expression_controlled(
            &reduction(),
            &BTreeMap::new(),
            &mut |expression, aliases| evaluate_reduction_fixture(expression, aliases, count),
            Some(&control),
        )
        .unwrap();
        assert_eq!(result, Value::Integer(0));
        assert!(resources.peak() <= input_bytes + 16_384);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn reduction_checks_cancellation_inside_the_expression() {
        let resources = QueryResources::default();
        let evaluations = Cell::new(0);
        let cancelled = Cell::new(false);
        let interrupted = || cancelled.get();
        let control = ExecutionControl::new(&resources, &interrupted);
        let error = evaluate_scoped_expression_controlled(
            &reduction(),
            &BTreeMap::new(),
            &mut |expression, aliases| {
                if matches!(expression, Expr::Variable(name) if name == "acc") {
                    evaluations.set(evaluations.get() + 1);
                    if evaluations.get() == 300 {
                        cancelled.set(true);
                    }
                }
                evaluate_reduction_fixture(expression, aliases, 10_000)
            },
            Some(&control),
        )
        .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert!((300..=556).contains(&evaluations.get()));
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn enclosing_alias_copies_are_rejected_before_scoped_evaluation() {
        let resources = QueryResources::new(Some(64 * 1024));
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);
        let aliases = BTreeMap::from([("large".to_owned(), Value::String("x".repeat(128 * 1024)))]);
        let reductions = Cell::new(0);
        let error = evaluate_scoped_expression_controlled(
            &reduction(),
            &aliases,
            &mut |expression, _| match expression {
                Expr::Literal(value) => Ok(value.clone()),
                Expr::Variable(name) if name == "input" => Ok(Value::List(vec![Value::Integer(1)])),
                _ => {
                    reductions.set(reductions.get() + 1);
                    Ok(Value::Null)
                }
            },
            Some(&control),
        )
        .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(reductions.get(), 0);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn list_output_grows_against_the_same_budget_as_its_input() {
        let expression = list_comprehension();
        let input = Value::List(vec![Value::String("x".repeat(4_096)); 4]);
        let input_bytes = value_bytes(&input).unwrap();
        let resources = QueryResources::new(Some(input_bytes + 12_000));
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);
        let error = evaluate_scoped_expression_controlled(
            &expression,
            &BTreeMap::new(),
            &mut |_, _| Ok(input.clone()),
            Some(&control),
        )
        .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert!(resources.peak() <= input_bytes + 12_000);
        assert_eq!(resources.used(), 0);
    }
}
