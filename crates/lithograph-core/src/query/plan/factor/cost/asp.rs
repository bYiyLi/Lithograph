//! ASP pays both factor tables, the complete ID set, filtered Hash and Probe.
use super::*;
use crate::query::plan::join::{HashFamily, HashOperandSide};
use crate::query::stats::CardinalityEstimate;
use crate::query::stream::{hash_layout, hash_table_peak_bytes};

pub(super) fn compare(
    step: &MatchStep,
    plan: &FactorizedPlan,
    a: Member,
    b: Member,
    work: FactorWork,
    statistics: &PlannerStatistics,
    available: Option<u64>,
) -> QueryResult<Option<u64>> {
    let Some(asp) = &plan.asp else {
        return Ok(None);
    };
    let probe = match asp.probe_group {
        FactorGroup::Accumulate => a,
        FactorGroup::Build => b,
        FactorGroup::Shared => return Ok(None),
    };
    let Some(keys) = key_upper(plan, &asp.probe_variable, probe.rows, statistics) else {
        return Ok(None);
    };
    let Some(build) = step.parts.get(asp.part) else {
        return Ok(None);
    };
    let Some(total) = build
        .start
        .labels
        .iter()
        .filter_map(|label| statistics.label_count(*label))
        .min()
    else {
        return Ok(None);
    };
    if total == 0 || keys.upper == 0 {
        return Ok(None);
    }
    let Some(join) = step.join.as_ref() else {
        return Ok(None);
    };
    let Some((equality, side)) = join.hashes.iter().find_map(|equality| {
        if equality.family != HashFamily::NodeIdentity {
            return None;
        }
        [HashOperandSide::Left, HashOperandSide::Right]
            .into_iter()
            .find_map(|side| {
                let (inner, probe) = equality.keys(side);
                (inner.variable == asp.build_variable && probe.variable == asp.probe_variable)
                    .then_some((equality, side))
            })
    }) else {
        return Ok(None);
    };
    let layout = hash_layout(build, equality, side)?;
    let Some(hash_peak) = hash_table_peak_bytes(keys.upper.min(total), &layout) else {
        return Ok(None);
    };
    // This first identity slice requires a bounded resident Hash. Larger or
    // unknown layouts keep the existing Grace/INL choice, rather than pricing
    // a resident table that the runtime cannot retain.
    if hash_peak > layout.soft_cap {
        return Ok(None);
    }
    Ok(score(
        plan,
        [a, b],
        work,
        keys,
        hash_peak,
        layout.headroom_bytes,
        available,
    ))
}

fn key_upper(
    plan: &FactorizedPlan,
    variable: &str,
    rows: u64,
    statistics: &PlannerStatistics,
) -> Option<CardinalityEstimate> {
    let binding = plan
        .bindings
        .iter()
        .find(|binding| binding.name == variable)?;
    if binding.group == FactorGroup::Shared {
        return None;
    }
    let FactorBindingKind::Node { labels } = &binding.kind else {
        return None;
    };
    // A complete Label count is only a safe distinct-ID upper bound, not an
    // asserted NDV or proof that every label owner occurs in the accumulated arm.
    labels
        .iter()
        .filter_map(|label| statistics.label_count(*label))
        .min()
        .map(|upper| CardinalityEstimate {
            expected: upper.min(rows),
            lower: 0,
            upper,
        })
}

fn score(
    plan: &FactorizedPlan,
    [a, b]: [Member; 2],
    work: FactorWork,
    keys: CardinalityEstimate,
    hash_peak: u64,
    hash_headroom: u64,
    available: Option<u64>,
) -> Option<u64> {
    let (spilled, table_peak) = table_work(plan, a, b)?;
    let peak = resident_peak(table_peak, keys.upper, hash_peak, hash_headroom)?;
    if available.is_some_and(|available| peak > available) {
        return Some(0);
    }
    let probe = match plan.asp.as_ref()?.probe_group {
        FactorGroup::Accumulate => a,
        FactorGroup::Build => b,
        FactorGroup::Shared => return None,
    };
    let other = if plan.asp.as_ref()?.probe_group == FactorGroup::Accumulate {
        b
    } else {
        a
    };
    let candidate = candidate_work(a.rows, probe, other, work, keys.expected, spilled)?;
    Some(
        work.old
            .checked_add(work.output.checked_mul(2)?)?
            .saturating_sub(candidate),
    )
}

fn resident_peak(table_peak: u64, keys: u64, hash_peak: u64, hash_headroom: u64) -> Option<u64> {
    let ids = keys
        .checked_mul(map_entry_bytes::<i64>().ok()?)?
        .checked_add(4096)?;
    table_peak
        .checked_mul(2)?
        .checked_add(hash_peak)?
        .checked_add(hash_headroom)?
        .checked_add(ids)
}

fn candidate_work(
    first_rows: u64,
    probe: Member,
    other: Member,
    work: FactorWork,
    keys: u64,
    spilled: bool,
) -> Option<u64> {
    let access = work.arms.iter().try_fold(0_u64, |sum, arm| {
        sum.checked_add(arm[1])?.checked_add(arm[2])
    })?;
    let accumulation = probe.rows.checked_add(other.rows)?;
    let id_work = probe
        .rows
        .checked_mul(u64::from(64 - keys.max(1).leading_zeros()))?;
    let hash_work = hash_probe_work(probe.rows, keys, work.point_work?)?;
    let transport = transport_work(first_rows, probe, other, work.output, spilled)?;
    access
        .checked_add(accumulation)?
        .checked_add(id_work)?
        .checked_add(hash_work)?
        .checked_add(transport)?
        .checked_add(work.output.checked_mul(2)?)
}

fn hash_probe_work(rows: u64, keys: u64, point_work: u64) -> Option<u64> {
    let point_and_insert = point_work.checked_add(3)?;
    keys.checked_mul(point_and_insert)?
        .checked_add(rows.checked_mul(5)?)
}

fn transport_work(
    first_rows: u64,
    probe: Member,
    other: Member,
    output: u64,
    spilled: bool,
) -> Option<u64> {
    if spilled {
        return spill_transport(first_rows, probe, other, output);
    }
    probe
        .rows
        .checked_mul(4)?
        .checked_add(other.rows.checked_mul(2)?)?
        .checked_add(first_rows.checked_mul(2)?)?
        .checked_add(output.checked_mul(2)?)
}

fn spill_transport(first_rows: u64, probe: Member, other: Member, output: u64) -> Option<u64> {
    let writes = probe.rows.checked_add(other.rows)?.checked_mul(2)?;
    let reads = probe
        .rows
        .checked_mul(2)?
        .checked_add(other.rows)?
        .checked_add(first_rows)?
        .checked_add(output)?;
    let records = writes.checked_add(reads)?;
    let bytes = records.checked_mul(probe.encoded.max(other.encoded))?;
    records
        .checked_mul(3)?
        .checked_add(bytes.div_ceil(TRAFFIC_UNIT))
}
