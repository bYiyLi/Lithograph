//! Work for the current two-arm table and its last-arm-fastest Flatten reader.
//! Logical codec traffic, resident claims and final output are separate facts.

use super::{FactorArm, FactorBindingKind, FactorGroup, FactorWork, FactorizedPlan};
use crate::query::expression::{BindingRow, BindingValue};
use crate::query::plan::MatchStep;
use crate::query::resources::map_entry_bytes;
use crate::query::spill::{identity_binding_encoded_upper, serialization_bytes};
use crate::query::stats::PlannerStatistics;
use crate::query::stream::factor::compact::{
    compact_layout_shape_bytes, compact_member_heap_bytes,
};
use crate::query::stream::factor::{
    FACTOR_RESIDENT_BYTES, factor_member_slot_bytes, factor_private_metadata_bytes,
    factor_table_metadata_bytes,
};
use crate::query::{ExecutionControl, QueryResult};

// The existing Join cost uses one record operation plus bytes / 4096. This is
// a work scale, not a claim about the actual private SQLite page size.
const TRAFFIC_UNIT: u64 = 4096;
mod asp;

#[derive(Clone, Copy)]
struct Member {
    rows: u64,
    encoded: u64,
    row: u64,
    resident: u64,
}

struct BaseWork {
    output: u64,
    final_work: u64,
    old: u64,
    access: u64,
}

pub(super) fn beneficial(
    step: &MatchStep,
    plan: &FactorizedPlan,
    statistics: &PlannerStatistics,
    flat_work: Option<FactorWork>,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    control.check()?;
    let Some(work) = flat_work else {
        return Ok(false);
    };
    let [[a_rows, _, _], [b_rows, _, _]] = work.arms;
    if !plan.single_shared {
        return Ok(false);
    }
    let Some(a) = member(
        step,
        plan,
        &plan.accumulate,
        FactorGroup::Accumulate,
        a_rows,
        statistics,
        control,
    )?
    else {
        return Ok(false);
    };
    let Some(b) = member(
        step,
        plan,
        &plan.build,
        FactorGroup::Build,
        b_rows,
        statistics,
        control,
    )?
    else {
        return Ok(false);
    };
    let available = control
        .resources()
        .limit()
        .map(|limit| limit.saturating_sub(control.resources().used()));
    let result = if plan.asp.is_some() {
        asp::compare(step, plan, a, b, work, statistics, available)?
    } else {
        compare(plan, a, b, work, available)
    };
    control.check()?;
    Ok(result.unwrap_or(false))
}

fn member(
    step: &MatchStep,
    plan: &FactorizedPlan,
    arm: &FactorArm,
    group: FactorGroup,
    rows: u64,
    statistics: &PlannerStatistics,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<Member>> {
    let mut upper = 1_u64;
    for hop in &arm.hops {
        control.work(1)?;
        let Some(type_id) = step
            .parts
            .get(hop.part)
            .and_then(|part| part.relationship.as_ref())
            .and_then(|rel| rel.type_id)
        else {
            return Ok(None);
        };
        let Some(total) = statistics.type_counts.get(&type_id).copied() else {
            return Ok(None);
        };
        let Some(product) = upper
            .checked_mul(total)
            .and_then(|value| value.checked_mul(2))
        else {
            return Ok(None);
        };
        upper = product;
    }
    if rows == 0 || rows > upper {
        return Ok(None);
    }
    Ok(group_layout(plan, arm, group, rows))
}

fn group_layout(
    plan: &FactorizedPlan,
    arm: &FactorArm,
    group: FactorGroup,
    rows: u64,
) -> Option<Member> {
    let mut names = 0_u64;
    let mut count = 0_u64;
    let mut relationships = 0_u64;
    let mut encoded = 0_u64;
    for binding in plan
        .bindings
        .iter()
        .filter(|binding| binding.group == group)
    {
        count = count.checked_add(1)?;
        names = names.checked_add(binding.name.len() as u64)?;
        let relationship = matches!(binding.kind, FactorBindingKind::Relationship { .. });
        relationships += u64::from(relationship);
        let name = Some(binding.name.as_str());
        let fields = if relationship {
            [None, name, None]
        } else {
            [name, None, None]
        };
        // A sum of existing per-field codec bounds conservatively retains each
        // row header. It is transport advice, not serialization scratch.
        encoded = encoded
            .checked_add(identity_binding_encoded_upper(fields, usize::from(relationship)).ok()?)?;
    }
    if count == 0 || relationships != arm.hops.len() as u64 {
        return None;
    }
    finish_layout(
        rows,
        row_bytes(count, names, relationships)?,
        encoded.checked_add(8)?,
        compact_member_heap_bytes(count, relationships, 0).ok()?,
        compact_layout_shape_bytes(count, count, names).ok()?,
    )
}

fn row_bytes(count: u64, names: u64, relationships: u64) -> Option<u64> {
    let order = (count + 1).max(4).checked_next_power_of_two()?;
    (std::mem::size_of::<BindingRow>() as u64)
        .checked_add(map_entry_bytes::<BindingValue>().ok()?.checked_mul(count)?)?
        .checked_add(names.checked_mul(2)?)?
        .checked_add((std::mem::size_of::<String>() as u64).checked_mul(order)?)?
        .checked_add(map_entry_bytes::<i64>().ok()?.checked_mul(relationships)?)
}

fn finish_layout(
    rows: u64,
    row: u64,
    encoded: u64,
    packed_heap: u64,
    layout: u64,
) -> Option<Member> {
    let capacity = rows.max(4).checked_next_power_of_two()?;
    let resident = rows
        .checked_mul(packed_heap)?
        .checked_add(capacity.checked_mul(factor_member_slot_bytes())?)?
        .checked_add(layout)?;
    Some(Member {
        rows,
        encoded,
        row,
        resident,
    })
}

fn compare(
    plan: &FactorizedPlan,
    a: Member,
    b: Member,
    work: FactorWork,
    available: Option<u64>,
) -> Option<bool> {
    let base = base_work(a, b, work)?;
    let (spilled, peak) = table_work(plan, a, b)?;
    if available.is_some_and(|available| peak > available) {
        return Some(false);
    }
    let startup = a
        .rows
        .checked_add(b.rows)?
        .checked_add(peak.div_ceil(TRAFFIC_UNIT))?;
    let extra = if spilled {
        spill_work(a, b, base.output)?
    } else {
        // GroupRun caches the current A member, but B is loaded for every
        // product row. Each load clones the two identity bindings.
        a.rows.checked_add(base.output)?.checked_mul(2)?
    };
    let factor = base
        .access
        .checked_add(base.final_work)?
        .checked_add(startup)?
        .checked_add(extra)?;
    Some(factor < base.old)
}

fn base_work(a: Member, b: Member, work: FactorWork) -> Option<BaseWork> {
    let product = a.rows.checked_mul(b.rows)?;
    if work.output > product {
        return None;
    }
    // Both choices implement the same complete clause. Use its single joint
    // output estimate, rather than charging one plan A*B and the other J.
    let output = work.output;
    let final_work = output.checked_mul(2)?;
    Some(BaseWork {
        output,
        final_work,
        old: work.old.checked_add(work.output.checked_mul(2)?)?,
        access: work.arms[0][1]
            .checked_add(work.arms[0][2])?
            .checked_add(work.arms[1][1])?
            .checked_add(work.arms[1][2])?,
    })
}

fn table_work(plan: &FactorizedPlan, a: Member, b: Member) -> Option<(bool, u64)> {
    let metadata = factor_table_metadata_bytes(2).ok()?;
    let prefix = prefix_bytes(&plan.shared)?;
    let resident = metadata
        .checked_add(prefix)?
        .checked_add(a.resident)?
        .checked_add(b.resident)?;
    // A Vec growth reserves new capacity while the old buffer remains live.
    // The still-reserved ordinary producer row also precedes compact packing.
    let growth = member_growth(a.rows)?.max(member_growth(b.rows)?);
    let resident_peak = resident.checked_add(growth)?;
    let spilled = resident_peak.checked_add(a.row.max(b.row))? > FACTOR_RESIDENT_BYTES;
    let peak = peak_bytes(a, b, prefix, metadata, resident_peak, spilled)?;
    Some((spilled, peak))
}

fn member_growth(rows: u64) -> Option<u64> {
    let capacity = rows.max(4).checked_next_power_of_two()?;
    let old = if capacity <= 4 { 0 } else { capacity / 2 };
    old.checked_mul(factor_member_slot_bytes())
}

fn prefix_bytes(shared: &str) -> Option<u64> {
    map_entry_bytes::<BindingValue>()
        .ok()?
        .checked_add((shared.len() as u64).checked_mul(2)?)?
        .checked_add((std::mem::size_of::<String>() as u64).checked_mul(4)?)
}

fn peak_bytes(
    a: Member,
    b: Member,
    prefix: u64,
    metadata: u64,
    resident: u64,
    spilled: bool,
) -> Option<u64> {
    let rows = a.row.checked_add(b.row)?;
    // The loaded members, prefix copy and merge scratch coexist. The prefix
    // and both row descriptions also conservatively cover the flat output.
    let output = rows
        .checked_add(prefix)?
        .checked_add((std::mem::size_of::<String>() as u64).checked_mul(8)?)?;
    let live = output.checked_mul(2)?.checked_add(rows)?;
    if !spilled {
        return resident.checked_add(live);
    }
    let scratch = serialization_bytes(a.row.max(b.row)).ok()?.checked_mul(2)?;
    FACTOR_RESIDENT_BYTES
        .checked_add(metadata)?
        .checked_add(prefix)?
        .checked_add(factor_private_metadata_bytes(2).ok()?)?
        .checked_add(scratch)?
        .checked_add(live)
}

fn spill_work(a: Member, b: Member, output: u64) -> Option<u64> {
    let writes = a.rows.checked_add(b.rows)?;
    let reads = a.rows.checked_add(output)?;
    let records = writes.checked_add(reads)?;
    let write_bytes = a
        .rows
        .checked_mul(a.encoded)?
        .checked_add(b.rows.checked_mul(b.encoded)?)?;
    let read_bytes = a
        .rows
        .checked_mul(a.encoded)?
        .checked_add(output.checked_mul(b.encoded)?)?;
    let bytes = write_bytes.checked_add(read_bytes)?;
    // Same scale as the existing Grace cost: each INSERT/SELECT is an actual
    // record operation; encode/decode each pay two CPU units. Repeated B reads
    // cannot be collapsed into a fictitious two-pass whole-table drain.
    records
        .checked_mul(3)?
        .checked_add(bytes.div_ceil(TRAFFIC_UNIT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equivalent_plans_share_joint_output_instead_of_one_cartesian_estimate() {
        let member = Member {
            rows: 8,
            encoded: 200,
            row: 6000,
            resident: 2048,
        };
        let b = Member { rows: 33, ..member };
        let work = FactorWork {
            old: 1768,
            output: 11,
            arms: [[8, 320, 640], [33, 34, 67]],
            point_work: None,
        };
        let base = base_work(member, b, work).unwrap();
        assert_eq!(base.output, 11);
        assert_eq!(base.final_work, 22);
        assert_eq!(base.old, 1790);
        assert_ne!(base.output, member.rows * b.rows);
    }

    #[test]
    fn spill_reads_last_arm_for_each_product_not_once() {
        let a = Member {
            rows: 128,
            encoded: 200,
            row: 6000,
            resident: 768_000,
        };
        let b = Member {
            rows: 256,
            encoded: 220,
            row: 6000,
            resident: 1_536_000,
        };
        let output = a.rows * b.rows;
        let records = (a.rows + b.rows) + (a.rows + output);
        let bytes = a.rows * a.encoded * 2 + b.rows * b.encoded + output * b.encoded;
        assert_eq!(
            spill_work(a, b, output),
            Some(records * 3 + bytes.div_ceil(TRAFFIC_UNIT))
        );
        assert!(spill_work(a, b, output).unwrap() > (a.rows + b.rows) * 6);
    }

    #[test]
    fn decoded_row_scratch_is_separate_from_compact_resident() {
        let member = finish_layout(128, 6000, 200, 96, 128).unwrap();
        assert!(member.row > member.encoded);
        assert_ne!(member.resident, 128 * member.encoded);
        assert!(member.resident < 128 * member.row);
        assert!(member.resident < FACTOR_RESIDENT_BYTES);
    }

    #[test]
    fn overflow_returns_unknown_instead_of_wrapping_to_cheap_work() {
        assert!(finish_layout(u64::MAX, 6000, 200, 96, 128).is_none());
        let member = Member {
            rows: u64::MAX,
            encoded: 200,
            row: 6000,
            resident: 1,
        };
        assert!(spill_work(member, member, u64::MAX).is_none());
    }
}
