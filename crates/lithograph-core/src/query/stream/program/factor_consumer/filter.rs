use std::rc::Rc;

use super::*;
use super::proof::ValueGroup;
use super::scratch::GroupScratch;
use crate::query::stream::factor::FactorTableBuilder;

pub(super) fn apply(
    mut packet: GroupPacket,
    group: ValueGroup,
    predicate: &Expr,
    runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<Option<GroupPacket>> {
    let mut scratch = GroupScratch::new(&packet, runtime.control)?;
    if group == ValueGroup::Prefix {
        let slot = super::evaluation_slot(&packet, runtime.metrics);
        return Ok(scratch.with_row(&packet, None, 0, runtime, |row, runtime| {
            runtime.metrics.record_factorized_evaluations(slot, 1, 0);
            expression::predicate(runtime.evaluate(predicate, row)?)
        })?.unwrap_or(false).then_some(packet));
    }
    let ValueGroup::Arm(selected) = group else {
        return Err(QueryError::internal("factor filter lost its selected arm"));
    };
    let prefix = packet.table.prefix();
    let memory = runtime.control.reserve(expression::binding_row_bytes_controlled(prefix, Some(runtime.control))?)?;
    let copied = expression::clone_binding_row_unreserved_controlled(prefix, Some(runtime.control))?;
    let mut builder = FactorTableBuilder::new_reserved((copied, memory), packet.table.arm_count(), runtime.control)?;
    for arm in 0..packet.table.arm_count() {
        copy_arm(&mut builder, &packet, arm, selected, predicate, &mut scratch, runtime)?;
    }
    packet.table = Rc::new(builder.finish(runtime.control)?);
    Ok((!packet.is_empty()).then_some(packet))
}

fn copy_arm(
    builder: &mut FactorTableBuilder, packet: &GroupPacket, arm: usize, selected: usize,
    predicate: &Expr, scratch: &mut GroupScratch, runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<()> {
    for index in 0..packet.table.arm_len(arm) {
        runtime.control.work(1)?;
        let keep = arm != selected || keep_member(packet, arm, index, predicate, scratch, runtime)?;
        if keep {
            let row = packet.table.read_arm_row(arm, index, runtime.control)?
                .ok_or_else(|| QueryError::internal("factor filter input ended early"))?;
            builder.push_reserved(arm, row, runtime.control)?;
        }
    }
    Ok(())
}

fn keep_member(
    packet: &GroupPacket, arm: usize, index: u64, predicate: &Expr,
    scratch: &mut GroupScratch, runtime: &mut ProgramRuntime<'_, '_>,
) -> QueryResult<bool> {
    let baseline = runtime.expression_memory.bytes();
    let slot = super::evaluation_slot(packet, runtime.metrics);
    let keep = scratch.with_row(packet, Some(arm), index, runtime, |row, runtime| {
        runtime.metrics.record_factorized_evaluations(slot, 1, 0);
        expression::predicate(runtime.evaluate(predicate, row)?)
    })?.unwrap_or(false);
    runtime.expression_memory.resize(baseline)?;
    Ok(keep)
}
