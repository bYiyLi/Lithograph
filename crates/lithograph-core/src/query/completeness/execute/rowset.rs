use super::*;

pub(crate) struct RowSet {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<BindingRow>,
    pub(crate) memory: Option<MemoryReservation>,
}

impl RowSet {
    pub(crate) fn track(&mut self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        let bytes = rowset_bytes(&self.columns, &self.rows, self.rows.capacity(), control)?;
        let bytes = crate::query::resources::checked_add(
            bytes,
            crate::query::resources::allocation_bytes::<BindingRow>(
                self.rows.capacity() - self.rows.len(),
            )?,
        )?;
        match self.memory.as_mut() {
            Some(memory) => memory.resize(bytes),
            None => {
                self.memory = Some(control.reserve(bytes)?);
                Ok(())
            }
        }
    }

    pub(crate) fn try_clone(&self, control: &ExecutionControl<'_>) -> QueryResult<Self> {
        Self::try_clone_rows(&self.columns, &self.rows, control)
    }

    pub(crate) fn try_clone_rows(
        columns: &[String],
        input_rows: &[BindingRow],
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let bytes = rowset_bytes(columns, input_rows, input_rows.len(), control)?;
        let memory = Some(control.reserve(bytes)?);
        let mut rows = Vec::with_capacity(input_rows.len());
        for row in input_rows {
            rows.push(expression::clone_binding_row_unreserved_controlled(
                row,
                Some(control),
            )?);
        }
        let columns = columns
            .iter()
            .map(|column| expression::clone_string_controlled(column, Some(control)))
            .collect::<QueryResult<Vec<_>>>()?;
        Ok(Self {
            columns,
            rows,
            memory,
        })
    }

    pub(crate) fn seed() -> Self {
        Self {
            columns: Vec::new(),
            rows: vec![BindingRow::default()],
            memory: None,
        }
    }
}

fn rowset_bytes(
    columns: &[String],
    rows: &[BindingRow],
    capacity: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    use crate::query::resources::{allocation_bytes, checked_add};
    let mut bytes = allocation_bytes::<BindingRow>(capacity.saturating_sub(rows.len()))?;
    bytes = checked_add(bytes, allocation_bytes::<String>(columns.len())?)?;
    for column in columns {
        control.work(1)?;
        bytes = checked_add(bytes, column.capacity() as u64)?;
    }
    for row in rows {
        bytes = checked_add(
            bytes,
            expression::binding_row_bytes_controlled(row, Some(control))?,
        )?;
    }
    Ok(bytes)
}
