//! One reusable prefix plus one arm member, never a product of arms.

use super::*;
use crate::query::resources::{allocation_bytes, checked_add};

pub(super) struct GroupScratch {
    row: BindingRow,
    memory: MemoryReservation,
}

impl GroupScratch {
    pub(super) fn new(packet: &GroupPacket, control: &ExecutionControl<'_>) -> QueryResult<Self> {
        let prefix = packet.table.prefix();
        let memory = control.reserve(expression::binding_row_bytes_controlled(
            prefix,
            Some(control),
        )?)?;
        let row = expression::clone_binding_row_unreserved_controlled(prefix, Some(control))?;
        Ok(Self { row, memory })
    }

    pub(super) fn with_row<T>(
        &mut self,
        packet: &GroupPacket,
        arm: Option<usize>,
        index: u64,
        runtime: &mut ProgramRuntime<'_, '_>,
        visit: impl FnOnce(&BindingRow, &mut ProgramRuntime<'_, '_>) -> QueryResult<T>,
    ) -> QueryResult<Option<T>> {
        runtime.control.work(1)?;
        if packet.is_empty() {
            return Ok(None);
        }
        self.reset(packet.table.prefix(), runtime.control)?;
        let member = match arm {
            Some(arm) => {
                let Some(member) = packet.table.read_arm_row(arm, index, runtime.control)? else {
                    return Err(QueryError::internal(
                        "factor member missing during group evaluation",
                    ));
                };
                Some(member)
            }
            None => None,
        };
        if let Some((delta, _memory)) = &member {
            self.merge(delta, runtime.control)?;
        }
        let value = match &packet.projection {
            Some(projections) => {
                let (row, _memory) = factor::consume::project_binding(
                    projections,
                    &self.row,
                    runtime.snapshot,
                    runtime.params,
                    &runtime.graph_view.query_access,
                    runtime.control,
                )?;
                visit(&row, runtime)?
            }
            None => visit(&self.row, runtime)?,
        };
        Ok(Some(value))
    }

    fn reset(&mut self, prefix: &BindingRow, control: &ExecutionControl<'_>) -> QueryResult<()> {
        control.work(self.row.values.len() as u64)?;
        self.row
            .values
            .retain(|name, _| prefix.values.contains_key(name));
        self.row.order.truncate(prefix.order.len());
        self.row
            .used_relationships
            .retain(|id| prefix.used_relationships.contains(id));
        self.memory.resize(expression::binding_row_bytes_controlled(
            &self.row,
            Some(control),
        )?)
    }

    fn merge(&mut self, delta: &BindingRow, control: &ExecutionControl<'_>) -> QueryResult<()> {
        self.reserve_delta(delta, control)?;
        self.merge_relationships(delta, control)?;
        self.merge_values(delta, control)
    }

    fn reserve_delta(
        &mut self,
        delta: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let bytes = checked_add(
            self.memory.bytes(),
            expression::binding_row_bytes_controlled(delta, Some(control))?,
        )?;
        let names = self
            .row
            .order
            .len()
            .checked_add(delta.order.len())
            .and_then(|names| names.max(4).checked_next_power_of_two())
            .ok_or_else(|| {
                crate::query::resources::memory_error("factor scratch column capacity overflow")
            })?;
        self.memory
            .resize(checked_add(bytes, allocation_bytes::<String>(names)?)?)?;
        Ok(())
    }

    fn merge_relationships(
        &mut self,
        delta: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for id in &delta.used_relationships {
            control.work(1)?;
            if !self.row.used_relationships.insert(*id) {
                return Err(QueryError::internal(
                    "factor proof allowed a reused relationship in one arm",
                ));
            }
        }
        Ok(())
    }

    fn merge_values(
        &mut self,
        delta: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for name in &delta.order {
            control.work(1)?;
            if self.row.values.contains_key(name) {
                return Err(QueryError::internal(
                    "factor delta overwrote its prefix binding",
                ));
            }
            let value = delta
                .values
                .get(name)
                .ok_or_else(|| QueryError::internal("factor delta column is absent"))?;
            self.row.insert(
                expression::clone_string_controlled(name, Some(control))?,
                expression::clone_binding_value_unreserved_controlled(value, Some(control))?,
            );
        }
        Ok(())
    }
}
