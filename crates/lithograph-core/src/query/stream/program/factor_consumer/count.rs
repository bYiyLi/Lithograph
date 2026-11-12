//! The basic and program aggregators share the same exact weighted COUNT.

use super::proof::ValueGroup;
use super::scratch::GroupScratch;
use super::*;
use crate::query::resources::allocation_bytes;

pub(crate) fn group_counts(
    packet: &GroupPacket,
    projections: &[Projection],
    snapshot: &Snapshot<'_>,
    params: &std::collections::BTreeMap<String, Value>,
    view: &ResolvedGraphView,
    control: &ExecutionControl<'_>,
    metrics: &mut QueryMetrics,
) -> QueryResult<Option<(Vec<u64>, MemoryReservation)>> {
    if !packet.cartesian_proven {
        return Ok(None);
    }
    let Some(total) = multiplicity(packet, control)? else {
        return Ok(None);
    };
    let mut runtime = ProgramRuntime {
        connection: snapshot.connection_for_query(),
        snapshot,
        graph_view: view,
        params,
        metrics,
        is_interrupted: &|| false,
        control,
        expression_memory: control.reserve(0)?,
    };
    let Some((groups, _groups_memory)) = groups(packet, projections, &runtime)? else {
        return Ok(None);
    };
    fold_counts(packet, projections, groups, total, &mut runtime).map(Some)
}

fn groups(
    packet: &GroupPacket,
    projections: &[Projection],
    runtime: &ProgramRuntime<'_, '_>,
) -> QueryResult<Option<(Vec<Option<ValueGroup>>, MemoryReservation)>> {
    let control = runtime.control;
    let memory = control.reserve(allocation_bytes::<Option<ValueGroup>>(projections.len())?)?;
    let mut groups = Vec::new();
    groups.try_reserve_exact(projections.len()).map_err(|_| {
        crate::query::resources::memory_error("unable to allocate factor count proof")
    })?;
    for projection in projections {
        control.work(1)?;
        let Some(argument) = count_argument(&projection.expression) else {
            return Ok(None);
        };
        let group = match argument {
            Some(argument) => {
                let Some(group) = proof::value_group(argument, packet, runtime)? else {
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

fn fold_counts(
    packet: &GroupPacket,
    projections: &[Projection],
    groups: Vec<Option<ValueGroup>>,
    total: u64,
    runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<(Vec<u64>, MemoryReservation)> {
    let control = runtime.control;
    let memory = control.reserve(allocation_bytes::<u64>(projections.len())?)?;
    let mut counts = Vec::new();
    counts
        .try_reserve_exact(projections.len())
        .map_err(|_| crate::query::resources::memory_error("unable to allocate factor counts"))?;
    let mut scratch = GroupScratch::new(packet, control)?;
    for (projection, group) in projections.iter().zip(groups) {
        control.work(1)?;
        let value = match (total, group) {
            (0, _) => 0,
            (_, None) => total,
            (_, Some(group)) => count_values(
                packet,
                &projection.expression,
                group,
                total,
                &mut scratch,
                runtime,
            )?,
        };
        counts.push(value);
    }
    Ok((counts, memory))
}

fn count_argument(expression: &Expr) -> Option<Option<&Expr>> {
    let Expr::Function {
        name,
        args,
        distinct: false,
        star,
    } = expression
    else {
        return None;
    };
    if !name.eq_ignore_ascii_case("count") {
        return None;
    }
    match (*star, args.as_slice()) {
        (_, []) => Some(None),
        (false, [argument]) => Some(Some(argument)),
        _ => None,
    }
}

pub(super) fn multiplicity(
    packet: &GroupPacket,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<u64>> {
    if packet.table.arm_count() == 0 {
        return Ok(Some(0));
    }
    let mut total = 1_u64;
    for arm in 0..packet.table.arm_count() {
        control.work(1)?;
        let Some(next) = total.checked_mul(packet.table.arm_len(arm)) else {
            return Ok(None);
        };
        if next > i64::MAX as u64 {
            return Ok(None);
        }
        total = next;
    }
    Ok(Some(total))
}

fn count_values(
    packet: &GroupPacket,
    expression: &Expr,
    group: ValueGroup,
    total: u64,
    scratch: &mut GroupScratch,
    runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<u64> {
    let argument = count_argument(expression)
        .flatten()
        .ok_or_else(|| QueryError::internal("factor count argument disappeared"))?;
    let (arm, entries, weight) = match group {
        ValueGroup::Prefix => (None, 1, total),
        ValueGroup::Arm(arm) => {
            let entries = packet.table.arm_len(arm);
            (Some(arm), entries, total / entries)
        }
    };
    let mut count = 0_u64;
    let slot = super::evaluation_slot(packet, runtime.metrics);
    for index in 0..entries {
        runtime.control.work(1)?;
        let baseline = runtime.expression_memory.bytes();
        let present = scratch
            .with_row(packet, arm, index, runtime, |row, runtime| {
                runtime.metrics.record_factorized_evaluations(slot, 0, 1);
                Ok(!matches!(runtime.evaluate(argument, row)?, Value::Null))
            })?
            .unwrap_or(false);
        runtime.expression_memory.resize(baseline)?;
        if present {
            count = count
                .checked_add(weight)
                .ok_or_else(|| QueryError::internal("proved factor count overflowed"))?;
        }
    }
    Ok(count)
}
