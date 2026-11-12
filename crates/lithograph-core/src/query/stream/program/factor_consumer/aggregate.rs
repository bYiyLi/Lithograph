//! Global aggregates over the existing Flat/Group pull protocol.

use super::super::*;
use super::proof::ValueGroup;
use super::scratch::GroupScratch;
use crate::query::resources::{allocation_bytes, checked_add, checked_mul, map_entry_bytes};
use crate::query::stream::factor::{BindingPacket, FlattenContinuation, GroupPacket};
use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AggregateKind {
    Count,
    Minimum,
    Maximum,
}

#[derive(Debug)]
pub(in crate::query::stream::program) struct FactorAggregatePlan {
    projections: Vec<Projection>,
    _memory: MemoryReservation,
}

fn projection_body(clause: &AstNode) -> Option<&AstNode> {
    clause
        .descendants()
        .find(|node| node.kind == AstKind::ProjectionBody)
}

fn global_body(clause: &AstNode) -> Option<&AstNode> {
    let body = projection_body(clause)?;
    if body.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::StarProjection
                | AstKind::GroupBy
                | AstKind::GroupByAll
                | AstKind::GroupByEmpty
                | AstKind::SetQuantifier(_)
                | AstKind::OrderBy
                | AstKind::Skip
                | AstKind::Limit
        )
    }) {
        return None;
    }
    Some(body)
}

fn aggregate(expression: &Expr) -> Option<(AggregateKind, Option<&Expr>)> {
    let Expr::Function {
        name,
        args,
        distinct: false,
        star,
    } = expression
    else {
        return None;
    };
    let kind = if name.eq_ignore_ascii_case("count") {
        AggregateKind::Count
    } else if name.eq_ignore_ascii_case("min") {
        AggregateKind::Minimum
    } else if name.eq_ignore_ascii_case("max") {
        AggregateKind::Maximum
    } else {
        return None;
    };
    match (*star, args.as_slice()) {
        (_, []) if kind == AggregateKind::Count => Some((kind, None)),
        (false, [argument]) if value_shape(argument) => Some((kind, Some(argument))),
        _ => None,
    }
}

fn value_shape(expression: &Expr) -> bool {
    match expression {
        Expr::Literal(Value::Integer(_) | Value::Null) | Expr::Parameter(_) => true,
        Expr::Property(base, _) => matches!(base.as_ref(), Expr::Variable(_)),
        _ => false,
    }
}

/// Structural check only: no graph read or expression evaluation occurs here.
pub(in crate::query::stream::program) fn supports_aggregate(clause: &AstNode) -> bool {
    let Some(body) = global_body(clause) else {
        return false;
    };
    let mut found = false;
    for item in body
        .descendants()
        .filter(|node| node.kind == AstKind::ProjectionItem)
    {
        let Some(node) = expression::surface_expressions(item).into_iter().next() else {
            return false;
        };
        let Ok(expression) = expression::compile_expression(node) else {
            return false;
        };
        if aggregate(&expression).is_none() {
            return false;
        }
        found = true;
    }
    found
}

/// The builder calls this before moving/pulling its upstream source.
pub(in crate::query::stream::program) fn compile_aggregate(
    clause: &AstNode,
    source: &str,
    params: &std::collections::BTreeMap<String, Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<FactorAggregatePlan>> {
    if global_body(clause).is_none() {
        return Ok(None);
    }
    let nodes = clause.descendants().count();
    let text = source
        .get(clause.span.start..clause.span.end)
        .unwrap_or(source);
    let mut memory = control.reserve(checked_add(
        checked_mul(text.len() as u64, 32)?,
        checked_add(checked_mul(nodes as u64, 256)?, 4096)?,
    )?)?;
    let plan = crate::query::plan::lower_projection(clause, source, params)?;
    if !plain_aggregate_plan(&plan) {
        return Ok(None);
    }
    memory.grow(allocation_bytes::<AggregateValue>(plan.projections.len())?)?;
    Ok(Some(FactorAggregatePlan {
        projections: plan.projections,
        _memory: memory,
    }))
}

fn plain_aggregate_plan(plan: &ProjectionPlan) -> bool {
    !plan.projections.is_empty()
        && !plan.distinct
        && plan.order.is_empty()
        && plan.skip == 0
        && plan.limit.is_none()
        && plan
            .projections
            .iter()
            .all(|projection| aggregate(&projection.expression).is_some())
}

pub(in crate::query::stream::program) fn admit_aggregate(
    plan: &FactorAggregatePlan,
    factor: &crate::query::plan::FactorizedPlan,
    snapshot: &Snapshot<'_>,
    params: &std::collections::BTreeMap<String, Value>,
    view: &ResolvedGraphView,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let mut metrics = QueryMetrics::default();
    let runtime = ProgramRuntime {
        connection: snapshot.connection_for_query(),
        snapshot,
        graph_view: view,
        params,
        metrics: &mut metrics,
        is_interrupted: &|| false,
        control,
        expression_memory: control.reserve(0)?,
    };
    for projection in &plan.projections {
        control.work(1)?;
        let Some((_, argument)) = aggregate(&projection.expression) else {
            return Ok(false);
        };
        if let Some(argument) = argument
            && super::proof::plan_value_group(argument, factor, &runtime)?.is_none()
        {
            return Ok(false);
        }
    }
    Ok(!plan.projections.is_empty())
}

#[derive(Debug, Default)]
struct AggregateValue {
    count: u64,
    extreme: Option<Value>,
    memory: Option<MemoryReservation>,
}

impl AggregateValue {
    fn observe(
        &mut self,
        kind: AggregateKind,
        value: &Value,
        weight: u64,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if matches!(value, Value::Null) {
            return Ok(());
        }
        if kind == AggregateKind::Count {
            self.count = self.count.wrapping_add(weight);
        } else if replaces(kind, value, self.extreme.as_ref(), control)? {
            let memory =
                control.reserve(expression::value_bytes_with_control(value, Some(control))?)?;
            let value = expression::clone_value_unreserved_controlled(value, Some(control))?;
            self.extreme = Some(value);
            self.memory = Some(memory);
        }
        Ok(())
    }
}

fn replaces(
    kind: AggregateKind,
    candidate: &Value,
    current: Option<&Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let Some(current) = current else {
        return Ok(true);
    };
    let ordering = expression::cypher_order_compare_controlled(candidate, current, Some(control))?;
    Ok(
        (kind == AggregateKind::Minimum && ordering == Ordering::Less)
            || (kind == AggregateKind::Maximum && ordering == Ordering::Greater),
    )
}

#[derive(Debug)]
pub(in crate::query::stream::program) struct FactorAggregateSource {
    upstream: Option<Box<dyn BindingSource>>,
    plan: FactorAggregatePlan,
    flatten: Option<FlattenContinuation>,
    values: Vec<AggregateValue>,
    context_seen: bool,
    context: Option<expression::LoadCsvContext>,
    context_memory: Option<MemoryReservation>,
    emitted: bool,
}

impl FactorAggregateSource {
    pub(in crate::query::stream::program) fn new(
        upstream: Box<dyn BindingSource>,
        plan: FactorAggregatePlan,
    ) -> Self {
        let values = (0..plan.projections.len())
            .map(|_| AggregateValue::default())
            .collect();
        Self {
            upstream: Some(upstream),
            plan,
            flatten: None,
            values,
            context_seen: false,
            context: None,
            context_memory: None,
            emitted: false,
        }
    }

    fn observe_context(
        &mut self,
        row: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if !self.context_seen {
            self.context_seen = true;
            let bytes = row
                .load_csv_context
                .as_ref()
                .and_then(|context| context.file.as_ref())
                .map_or(0, |file| file.len()) as u64;
            let memory = control.reserve(bytes)?;
            self.context = expression::clone_load_csv_context_controlled(
                &row.load_csv_context,
                Some(control),
            )?;
            self.context_memory = Some(memory);
        } else if let Some(context) = self.context.as_mut() {
            let next = row.load_csv_context.as_ref();
            if context.file.as_ref() != next.and_then(|context| context.file.as_ref()) {
                context.file = None;
            }
            if context.line != next.and_then(|context| context.line) {
                context.line = None;
            }
        }
        if self
            .context
            .as_ref()
            .is_some_and(|context| context.file.is_none() && context.line.is_none())
        {
            self.context = None;
        }
        if self
            .context
            .as_ref()
            .is_none_or(|context| context.file.is_none())
        {
            self.context_memory = None;
        }
        Ok(())
    }

    fn flat(&mut self, row: &BindingRow, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<()> {
        self.observe_context(row, runtime.control)?;
        for (projection, state) in self.plan.projections.iter().zip(&mut self.values) {
            runtime.control.work(1)?;
            let (kind, argument) = aggregate(&projection.expression).ok_or_else(|| {
                QueryError::internal("factor aggregate lost its compiled expression")
            })?;
            if let Some(argument) = argument {
                let baseline = runtime.expression_memory.bytes();
                let value = runtime.evaluate(argument, row)?;
                state.observe(kind, &value, 1, runtime.control)?;
                drop(value);
                runtime.expression_memory.resize(baseline)?;
            } else {
                // Program COUNT uses group.len() as i64. Slow enumeration
                // preserves that cast's wrap semantics, never saturation.
                state.count = state.count.wrapping_add(1);
            }
        }
        Ok(())
    }

    fn group(
        &mut self,
        packet: &GroupPacket,
        runtime: &mut ProgramRuntime<'_, '_>,
    ) -> QueryResult<bool> {
        if !packet.cartesian_proven {
            return Ok(false);
        }
        let Some(total) = super::count::multiplicity(packet, runtime.control)? else {
            return Ok(false);
        };
        if total == 0 {
            return Ok(true);
        }
        let Some((groups, _proof_memory)) = self.preflight_groups(packet, total, runtime)? else {
            return Ok(false);
        };
        let (deltas, _delta_memory) = self.fold_deltas(packet, groups, runtime)?;
        self.observe_context(packet.table.prefix(), runtime.control)?;
        self.apply_deltas(deltas, runtime)?;
        Ok(true)
    }

    fn preflight_groups(
        &self,
        packet: &GroupPacket,
        total: u64,
        runtime: &ProgramRuntime<'_, '_>,
    ) -> QueryResult<Option<(Vec<Option<ValueGroup>>, MemoryReservation)>> {
        let memory = runtime
            .control
            .reserve(allocation_bytes::<Option<ValueGroup>>(self.values.len())?)?;
        let mut groups = Vec::with_capacity(self.values.len());
        // Preflight every expression and every possible COUNT increment before
        // evaluating or changing any aggregate. Failure flattens one packet.
        for (projection, state) in self.plan.projections.iter().zip(&self.values) {
            runtime.control.work(1)?;
            let (kind, argument) = aggregate(&projection.expression).ok_or_else(|| {
                QueryError::internal("factor aggregate lost its compiled expression")
            })?;
            if kind == AggregateKind::Count
                && state
                    .count
                    .checked_add(total)
                    .is_none_or(|count| count > i64::MAX as u64)
            {
                return Ok(None);
            }
            let group = match argument {
                Some(argument) => {
                    let Some(group) = aggregate_value_group(argument, packet, runtime)? else {
                        return Ok(None);
                    };
                    Some(group)
                }
                None => None,
            };
            groups.push(group);
        }
        Ok(Some((groups, memory)))
    }

    fn fold_deltas(
        &self,
        packet: &GroupPacket,
        groups: Vec<Option<ValueGroup>>,
        runtime: &mut ProgramRuntime<'_, '_>,
    ) -> QueryResult<(Vec<AggregateValue>, MemoryReservation)> {
        let memory = runtime
            .control
            .reserve(allocation_bytes::<AggregateValue>(self.values.len())?)?;
        let mut deltas = Vec::with_capacity(self.values.len());
        let mut scratch = GroupScratch::new(packet, runtime.control)?;
        for (projection, group) in self.plan.projections.iter().zip(groups) {
            runtime.control.work(1)?;
            let (kind, argument) = aggregate(&projection.expression).ok_or_else(|| {
                QueryError::internal("factor aggregate lost its compiled expression")
            })?;
            let delta = if kind == AggregateKind::Count {
                fold_count(packet, projection, runtime)?
            } else {
                let argument =
                    argument.ok_or_else(|| QueryError::internal("MIN/MAX argument is absent"))?;
                let group = group.ok_or_else(|| QueryError::internal("MIN/MAX group is absent"))?;
                fold_extreme(packet, argument, kind, group, &mut scratch, runtime)?
            };
            deltas.push(delta);
        }
        Ok((deltas, memory))
    }

    fn apply_deltas(
        &mut self,
        deltas: Vec<AggregateValue>,
        runtime: &ProgramRuntime<'_, '_>,
    ) -> QueryResult<()> {
        for ((projection, state), mut delta) in self
            .plan
            .projections
            .iter()
            .zip(&mut self.values)
            .zip(deltas)
        {
            let (kind, _) = aggregate(&projection.expression).ok_or_else(|| {
                QueryError::internal("factor aggregate lost its compiled expression")
            })?;
            if kind == AggregateKind::Count {
                state.count += delta.count;
            } else if let Some(value) = delta.extreme.as_ref()
                && replaces(kind, value, state.extreme.as_ref(), runtime.control)?
            {
                state.extreme = delta.extreme.take();
                state.memory = delta.memory.take();
            }
        }
        Ok(())
    }

    fn consume(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<()> {
        loop {
            runtime.control.work(1)?;
            let baseline = runtime.expression_memory.bytes();
            if let Some(row) = super::next_flat(&mut self.flatten, runtime)? {
                self.flat(&row, runtime)?;
                drop(row);
                runtime.expression_memory.resize(baseline)?;
                continue;
            }
            let Some(upstream) = self.upstream.as_mut() else {
                return Ok(());
            };
            let Some(packet) = upstream.next_packet(runtime)? else {
                self.upstream = None;
                self.flatten = None;
                runtime.expression_memory.resize(baseline)?;
                return Ok(());
            };
            match packet {
                BindingPacket::Flat(row) => {
                    self.flat(&row, runtime)?;
                    drop(row);
                }
                BindingPacket::Group(packet) => {
                    if !self.group(&packet, runtime)? {
                        // No aggregate state changed before this fallback.
                        // Flatten this exact packet once; never replay input.
                        super::install(
                            &mut self.flatten,
                            BindingPacket::Group(packet),
                            runtime.control,
                        )?;
                    }
                }
            }
            runtime.expression_memory.resize(baseline)?;
        }
    }

    fn output_bytes(&self, control: &ExecutionControl<'_>) -> QueryResult<u64> {
        let capacity = self
            .plan
            .projections
            .len()
            .max(4)
            .checked_next_power_of_two()
            .ok_or_else(|| {
                crate::query::resources::memory_error("factor aggregate output capacity overflow")
            })?;
        let mut bytes = allocation_bytes::<String>(capacity)?;
        for (projection, state) in self.plan.projections.iter().zip(&self.values) {
            control.work(1)?;
            bytes = checked_add(bytes, output_slot_bytes(projection, state, control)?)?;
        }
        checked_add(
            bytes,
            self.context
                .as_ref()
                .and_then(|context| context.file.as_ref())
                .map_or(0, |file| file.len()) as u64,
        )
    }

    fn output(&self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<BindingRow> {
        let memory = runtime
            .control
            .reserve(self.output_bytes(runtime.control)?)?;
        let mut row = BindingRow::default();
        for (projection, state) in self.plan.projections.iter().zip(&self.values) {
            let value = output_value(projection, state, runtime.control)?;
            row.insert(
                expression::clone_string_controlled(&projection.column, Some(runtime.control))?,
                expression::BindingValue::Scalar(value),
            );
        }
        row.load_csv_context =
            expression::clone_load_csv_context_controlled(&self.context, Some(runtime.control))?;
        runtime.expression_memory.merge(memory)?;
        Ok(row)
    }
}

fn output_slot_bytes(
    projection: &Projection,
    state: &AggregateValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let names = checked_mul(projection.column.len() as u64, 2)?;
    let slot = checked_add(
        map_entry_bytes::<expression::BindingValue>()?,
        std::mem::size_of::<Value>() as u64,
    )?;
    let value = state
        .extreme
        .as_ref()
        .map(|value| expression::value_bytes_with_control(value, Some(control)))
        .transpose()?
        .unwrap_or(0);
    checked_add(checked_add(names, slot)?, value)
}

fn output_value(
    projection: &Projection,
    state: &AggregateValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<Value> {
    let (kind, _) = aggregate(&projection.expression)
        .ok_or_else(|| QueryError::internal("factor aggregate lost its compiled expression"))?;
    if kind == AggregateKind::Count {
        return Ok(Value::Integer(state.count as i64));
    }
    state
        .extreme
        .as_ref()
        .map(|value| expression::clone_value_unreserved_controlled(value, Some(control)))
        .transpose()
        .map(|value| value.unwrap_or(Value::Null))
}

fn aggregate_value_group(
    argument: &Expr,
    packet: &GroupPacket,
    runtime: &ProgramRuntime<'_, '_>,
) -> QueryResult<Option<ValueGroup>> {
    let Some(group) = super::proof::value_group(argument, packet, runtime)? else {
        return Ok(None);
    };
    if let ValueGroup::Arm(arm) = group
        && arm >= packet.table.arm_count()
    {
        return Ok(None);
    }
    Ok(Some(group))
}

fn fold_count(
    packet: &GroupPacket,
    projection: &Projection,
    runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<AggregateValue> {
    let Some((counts, _memory)) = super::count::group_counts(
        packet,
        std::slice::from_ref(projection),
        runtime.snapshot,
        runtime.params,
        runtime.graph_view,
        runtime.control,
        runtime.metrics,
    )?
    else {
        return Err(QueryError::internal(
            "factor COUNT proof changed after preflight",
        ));
    };
    let count = *counts
        .first()
        .ok_or_else(|| QueryError::internal("factor COUNT returned no value"))?;
    Ok(AggregateValue {
        count,
        ..AggregateValue::default()
    })
}

fn fold_extreme(
    packet: &GroupPacket,
    argument: &Expr,
    kind: AggregateKind,
    group: ValueGroup,
    scratch: &mut GroupScratch,
    runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<AggregateValue> {
    let (arm, length) = match group {
        ValueGroup::Prefix => (None, 1),
        ValueGroup::Arm(arm) => (Some(arm), packet.table.arm_len(arm)),
    };
    let mut delta = AggregateValue::default();
    let slot = super::evaluation_slot(packet, runtime.metrics);
    for index in 0..length {
        runtime.control.work(1)?;
        let baseline = runtime.expression_memory.bytes();
        let present = scratch.with_row(packet, arm, index, runtime, |row, runtime| {
            runtime.metrics.record_factorized_evaluations(slot, 0, 1);
            let value = runtime.evaluate(argument, row)?;
            delta.observe(kind, &value, 1, runtime.control)
        })?;
        if present.is_none() {
            return Err(QueryError::internal("nonempty factor group lost a member"));
        }
        runtime.expression_memory.resize(baseline)?;
    }
    Ok(delta)
}

impl BindingSource for FactorAggregateSource {
    fn next(&mut self, runtime: &mut ProgramRuntime<'_, '_>) -> QueryResult<Option<BindingRow>> {
        if self.emitted {
            return Ok(None);
        }
        self.consume(runtime)?;
        let row = self.output(runtime)?;
        self.emitted = true;
        // The admitted program has one global aggregate boundary. Attribute
        // its actual output here, independently of the final Project row.
        let slot = runtime.metrics.operator_profile().and_then(|operators| {
            operators
                .iter()
                .position(|operator| operator.operator == "Aggregate")
        });
        runtime.metrics.record_profile_rows(slot, 1);
        Ok(Some(row))
    }
}
