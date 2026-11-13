use super::*;
use crate::query::resources::{allocation_bytes, checked_add, checked_mul, map_entry_bytes};

pub(super) fn binding_names(part: &PatternPart) -> [Option<&str>; 4] {
    [
        part.path_variable.as_deref(),
        part.start.variable.as_deref(),
        part.relationship
            .as_ref()
            .and_then(|rel| rel.variable.as_deref()),
        part.end.as_ref().and_then(|node| node.variable.as_deref()),
    ]
}

/// Reserve both copies of a fresh binding name, order growth and the sparse
/// relationship used-set before any clone/bind. The incoming base stays live.
fn output_row_bytes(
    base: &BindingRow,
    part: &PatternPart,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let bytes = expression::binding_row_bytes_controlled(base, Some(control))?;
    let (fresh, additions) = fresh_bindings(base, part, control)?;
    output_growth_bytes(
        bytes,
        base.order.len(),
        fresh,
        additions,
        part.relationship.is_some(),
    )
}

fn output_growth_bytes(
    mut bytes: u64,
    order_len: usize,
    fresh: u64,
    additions: usize,
    relationship: bool,
) -> QueryResult<u64> {
    bytes = checked_add(bytes, fresh)?;
    let order = order_len
        .checked_add(additions)
        .and_then(usize::checked_next_power_of_two)
        .ok_or_else(|| crate::query::resources::memory_error("MATCH order capacity overflow"))?;
    bytes = checked_add(bytes, allocation_bytes::<String>(order.max(4))?)?;
    if relationship {
        bytes = checked_add(bytes, map_entry_bytes::<i64>()?)?;
    }
    // The fixed-hop path vectors and their small allocation headers.
    checked_add(bytes, 256)
}

/// The root StepCursor has one plain node Part and an empty incoming binding.
/// Price the same checked output reservation without constructing a fake row.
pub(super) fn node_output_row_bytes(spec: &crate::query::plan::NodeSpec) -> QueryResult<u64> {
    let (fresh, additions) = match spec.variable.as_ref() {
        Some(name) => (fresh_name_bytes(name)?, 1),
        None => (0, 0),
    };
    output_growth_bytes(
        allocation_bytes::<BindingRow>(1)?,
        0,
        fresh,
        additions,
        false,
    )
}

fn fresh_name_bytes(name: &str) -> QueryResult<u64> {
    checked_add(
        map_entry_bytes::<BindingValue>()?,
        checked_mul(name.len() as u64, 2)?,
    )
}

fn fresh_bindings(
    base: &BindingRow,
    part: &PatternPart,
    control: &ExecutionControl<'_>,
) -> QueryResult<(u64, usize)> {
    let mut bytes = 0;
    let names = binding_names(part);
    let mut additions = 0usize;
    for name in names.into_iter().flatten() {
        control.work(1)?;
        if !base.values.contains_key(name) {
            additions += 1;
            bytes = checked_add(bytes, fresh_name_bytes(name)?)?;
        }
    }
    Ok((bytes, additions))
}

impl PartCursor {
    pub(super) fn reserve_output(&mut self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        let bytes = output_row_bytes(&self.base, &self.part, control)?;
        let copies = if self
            .part
            .relationship
            .as_ref()
            .is_some_and(|rel| rel.index_seek.is_some() || rel.id_seek.is_some())
        {
            2
        } else {
            1
        };
        let bytes = checked_mul(bytes, copies)?;
        match &mut self.output_memory {
            Some(memory) => memory.resize(bytes)?,
            None => self.output_memory = Some(control.reserve(bytes)?),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
