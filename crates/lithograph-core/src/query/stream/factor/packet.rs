use std::rc::Rc;
use std::sync::Arc;

use super::FactorTable;
use crate::query::expression::BindingRow;
use crate::query::resources::{allocation_bytes, checked_add};
use crate::query::{ExecutionControl, MemoryReservation, QueryResult};

#[derive(Debug)]
pub(crate) enum BindingPacket {
    Flat(BindingRow),
    Group(GroupPacket),
}

/// These two proven projections can remain descriptions until the SQL boundary.
#[derive(Clone, Debug)]
pub(crate) enum GroupProjection {
    Slot { name: String, output: String },
    ElementId { name: String, output: String },
}

impl GroupProjection {
    pub(crate) fn names(&self) -> (&str, &str) {
        match self {
            Self::Slot { name, output } | Self::ElementId { name, output } => (name, output),
        }
    }
}

#[derive(Debug)]
pub(crate) struct GroupPacket {
    pub(crate) table: Rc<FactorTable>,
    pub(crate) plan: Option<Arc<crate::query::plan::FactorizedPlan>>,
    pub(crate) projection: Option<Vec<GroupProjection>>,
    /// Only a plan proof that all cross-arm combinations are valid permits folds.
    pub(crate) cartesian_proven: bool,
    _memory: MemoryReservation,
    projection_memory: MemoryReservation,
}

impl GroupPacket {
    pub(crate) fn memory_bytes(&self) -> QueryResult<u64> {
        checked_add(self._memory.bytes(), self.projection_memory.bytes())
    }

    pub(crate) fn new(table: Rc<FactorTable>, control: &ExecutionControl<'_>) -> QueryResult<Self> {
        Ok(Self {
            table,
            plan: None,
            projection: None,
            cartesian_proven: false,
            _memory: control.reserve(std::mem::size_of::<Self>() as u64)?,
            projection_memory: control.reserve(0)?,
        })
    }

    pub(crate) fn set_projections(
        &mut self,
        projections: &[GroupProjection],
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let mut bytes = allocation_bytes::<GroupProjection>(projections.len())?;
        for projection in projections {
            control.work(1)?;
            let (name, output) = projection.names();
            bytes = checked_add(bytes, checked_add(name.len() as u64, output.len() as u64)?)?;
        }
        let memory = control.reserve(bytes)?;
        let mut copied = Vec::new();
        copied.try_reserve_exact(projections.len()).map_err(|_| {
            crate::query::resources::memory_error("unable to allocate factorized projections")
        })?;
        for projection in projections {
            control.work(1)?;
            let (name, output) = projection.names();
            let name = crate::query::expression::clone_string_controlled(name, Some(control))?;
            let output = crate::query::expression::clone_string_controlled(output, Some(control))?;
            copied.push(match projection {
                GroupProjection::Slot { .. } => GroupProjection::Slot { name, output },
                GroupProjection::ElementId { .. } => GroupProjection::ElementId { name, output },
            });
        }
        self.projection = Some(copied);
        self.projection_memory = memory;
        Ok(())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.table.arm_count() == 0
            || (0..self.table.arm_count()).any(|arm| self.table.arm_len(arm) == 0)
    }
}
