//! Group-aware consumers in the existing program pull pipeline.

use super::*;
use crate::query::stream::factor::{self, BindingPacket, FlattenContinuation, GroupPacket};

mod aggregate;
mod count;
mod filter;
mod proof;
mod scratch;

pub(super) use aggregate::{
    FactorAggregateSource, admit_aggregate, compile_aggregate, supports_aggregate,
};

fn evaluation_slot(packet: &GroupPacket, metrics: &QueryMetrics) -> Option<usize> {
    packet
        .plan
        .as_ref()
        .and_then(|plan| metrics.factorized_slot(plan.clause_start, "FactorizedAccumulate"))
}

pub(super) fn filter(
    source: &mut FilterSource,
    runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<Option<BindingRow>> {
    next_output(source, |source| &mut source.factor_output, runtime)
}

// Both consumers override next_packet; pending Group rows must be drained
// before advancing that packet source. This preserves the same pull boundary.
fn next_output<S: BindingSource>(
    source: &mut S,
    output: fn(&mut S) -> &mut Option<FlattenContinuation>,
    runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<Option<BindingRow>> {
    loop {
        if let Some(row) = next_flat(output(source), runtime)? {
            return Ok(Some(row));
        }
        let Some(packet) = source.next_packet(runtime)? else {
            return Ok(None);
        };
        match packet {
            BindingPacket::Flat(row) => return Ok(Some(row)),
            packet @ BindingPacket::Group(_) => install(output(source), packet, runtime.control)?,
        }
    }
}

fn next_input(
    upstream: &mut dyn BindingSource,
    flatten: &mut Option<FlattenContinuation>,
    runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<Option<BindingPacket>> {
    match next_flat(flatten, runtime)? {
        Some(row) => Ok(Some(BindingPacket::Flat(row))),
        None => upstream.next_packet(runtime),
    }
}

pub(super) fn filter_packet(
    source: &mut FilterSource,
    runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<Option<BindingPacket>> {
    loop {
        let baseline = runtime.expression_memory.bytes();
        check_interrupted(runtime.is_interrupted)?;
        let Some(packet) = next_input(
            source.upstream.as_mut(),
            &mut source.factor_flatten,
            runtime,
        )?
        else {
            return Ok(None);
        };
        match packet {
            BindingPacket::Flat(row) => {
                if expression::predicate(runtime.evaluate(&source.predicate, &row)?)? {
                    return Ok(Some(BindingPacket::Flat(row)));
                }
            }
            BindingPacket::Group(packet) => {
                if packet.is_empty() {
                    continue;
                }
                let Some(group) = proof::predicate_group(&source.predicate, &packet, runtime)?
                else {
                    install(
                        &mut source.factor_flatten,
                        BindingPacket::Group(packet),
                        runtime.control,
                    )?;
                    continue;
                };
                if let Some(packet) = filter::apply(packet, group, &source.predicate, runtime)? {
                    return Ok(Some(BindingPacket::Group(packet)));
                }
            }
        }
        runtime.expression_memory.resize(baseline)?;
    }
}

pub(super) fn projection(
    source: &mut ProjectionSource,
    runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<Option<BindingRow>> {
    next_output(source, |source| &mut source.factor_output, runtime)
}

pub(super) fn projection_packet(
    source: &mut ProjectionSource,
    runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<Option<BindingPacket>> {
    if source.limit.is_some_and(|limit| source.emitted >= limit) {
        return Ok(None);
    }
    loop {
        let baseline = runtime.expression_memory.bytes();
        check_interrupted(runtime.is_interrupted)?;
        let Some(packet) = next_input(
            source.upstream.as_mut(),
            &mut source.factor_flatten,
            runtime,
        )?
        else {
            return Ok(None);
        };
        let input = match packet {
            BindingPacket::Group(packet) => {
                if let Some(packet) = project_group(source, packet, runtime)? {
                    return Ok(Some(packet));
                }
                continue;
            }
            BindingPacket::Flat(input) => input,
        };
        let mut output = BindingRow::default();
        output.load_csv_context.clone_from(&input.load_csv_context);
        for projection in &source.projections {
            let value = runtime.evaluate(&projection.expression, &input)?;
            output.insert(
                projection.column.clone(),
                expression::binding_from_value_controlled(
                    runtime.snapshot,
                    value,
                    runtime.control,
                )?,
            );
        }
        if source.seen < source.skip {
            source.seen = source.seen.saturating_add(1);
            drop(input);
            drop(output);
            runtime.expression_memory.resize(baseline)?;
            continue;
        }
        source.seen = source.seen.saturating_add(1);
        source.emitted = source.emitted.saturating_add(1);
        return Ok(Some(BindingPacket::Flat(output)));
    }
}

fn project_group(
    source: &mut ProjectionSource,
    mut packet: GroupPacket,
    runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<Option<BindingPacket>> {
    if packet.is_empty() {
        return Ok(None);
    }
    if source.skip == 0
        && source.limit.is_none()
        && let Some((projections, _memory)) =
            factor::consume::compile_projection(&packet, &source.projections, runtime.control)?
    {
        packet.set_projections(&projections, runtime.control)?;
        return Ok(Some(BindingPacket::Group(packet)));
    }
    install(
        &mut source.factor_flatten,
        BindingPacket::Group(packet),
        runtime.control,
    )?;
    Ok(None)
}

fn install(
    flatten: &mut Option<FlattenContinuation>,
    packet: BindingPacket,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    if flatten.is_none() {
        *flatten = Some(FlattenContinuation::new(control)?);
    }
    flatten
        .as_mut()
        .ok_or_else(|| QueryError::internal("missing factor continuation"))?
        .install(packet, control)
}

fn next_flat(
    flatten: &mut Option<FlattenContinuation>,
    runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<Option<BindingRow>> {
    let Some(flatten) = flatten.as_mut() else {
        return Ok(None);
    };
    let clause = flatten.clause_start();
    let row = flatten.with_next_row(runtime.control, |row, projection| {
        if let Some(clause) = clause {
            runtime.metrics.record_factorized_flatten(clause, 1);
        }
        let (row, memory) = match projection {
            Some(projections) => factor::consume::project_binding(
                projections,
                row,
                runtime.snapshot,
                runtime.params,
                &runtime.graph_view.query_access,
                runtime.control,
            )?,
            None => {
                let memory = runtime
                    .control
                    .reserve(expression::binding_row_bytes_controlled(
                        row,
                        Some(runtime.control),
                    )?)?;
                (
                    expression::clone_binding_row_unreserved_controlled(
                        row,
                        Some(runtime.control),
                    )?,
                    memory,
                )
            }
        };
        runtime.expression_memory.merge(memory)?;
        Ok(row)
    })?;
    if let Some((clause, delta)) = flatten.take_storage_metrics()? {
        runtime
            .metrics
            .record_factorized_storage(clause, "Flatten", delta);
    }
    Ok(row)
}
