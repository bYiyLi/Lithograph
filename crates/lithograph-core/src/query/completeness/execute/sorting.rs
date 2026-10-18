use super::*;

struct SortKeys<'a> {
    values: &'a [Vec<Value>],
    directions: &'a [(usize, crate::cypher::OrderDirectionKind)],
    order: &'a AstNode,
    control: &'a ExecutionControl<'a>,
}

impl ReadExecutor<'_, '_> {
    pub(super) fn sort_projected(
        &mut self,
        rows: &mut [ProjectedRow],
        order: &AstNode,
    ) -> QueryResult<()> {
        let expressions = surface_expressions(order)
            .into_iter()
            .map(compile_expression)
            .collect::<QueryResult<Vec<_>>>()?;
        let directions = order
            .descendants()
            .filter_map(|node| match node.kind {
                AstKind::OrderDirection(direction) => Some((node.span.start, direction)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let (keys, _memory) = self.projection_sort_keys(rows, &expressions, &directions, order)?;
        let context = SortKeys {
            values: &keys,
            directions: &directions,
            order,
            control: self.control,
        };
        let (indices, _indices_memory) = sorted_indices(rows.len(), &context)?;
        apply_permutation(rows, indices, self.control)
    }

    fn projection_sort_keys(
        &mut self,
        rows: &[ProjectedRow],
        expressions: &[expression::Expr],
        directions: &[(usize, crate::cypher::OrderDirectionKind)],
        order: &AstNode,
    ) -> QueryResult<(Vec<Vec<Value>>, MemoryReservation)> {
        let mut memory = self
            .control
            .reserve(crate::query::resources::allocation_bytes::<Vec<Value>>(
                rows.len(),
            )?)?;
        let mut keys = Vec::with_capacity(rows.len());
        for row in rows {
            self.control.work(1)?;
            memory.grow(crate::query::resources::allocation_bytes::<Value>(
                expressions.len(),
            )?)?;
            let mut row_keys = Vec::with_capacity(expressions.len());
            for expression in expressions {
                row_keys.push(self.evaluate_projected(expression, row)?);
            }
            compare_keys(&row_keys, &row_keys, directions, order, self.control)?;
            keys.push(row_keys);
        }
        Ok((keys, memory))
    }
}

fn sorted_indices(
    length: usize,
    context: &SortKeys<'_>,
) -> QueryResult<(Vec<usize>, MemoryReservation)> {
    let mut memory = context
        .control
        .reserve(crate::query::resources::checked_mul(
            crate::query::resources::allocation_bytes::<usize>(length)?,
            2,
        )?)?;
    let mut indices = (0..length).collect::<Vec<_>>();
    let mut scratch = vec![0; length];
    let mut width = 1_usize;
    while width < length {
        let mut start = 0_usize;
        while start < length {
            let middle = start.saturating_add(width).min(length);
            let end = middle.saturating_add(width).min(length);
            merge_indices(
                &indices[start..middle],
                &indices[middle..end],
                &mut scratch[start..end],
                context,
            )?;
            start = end;
        }
        std::mem::swap(&mut indices, &mut scratch);
        width = width.saturating_mul(2);
    }
    memory.resize(crate::query::resources::allocation_bytes::<usize>(length)?)?;
    Ok((indices, memory))
}

fn merge_indices(
    left: &[usize],
    right: &[usize],
    output: &mut [usize],
    context: &SortKeys<'_>,
) -> QueryResult<()> {
    let (mut left_index, mut right_index) = (0, 0);
    for target in output {
        context.control.work(1)?;
        let take_left = right_index == right.len()
            || left_index < left.len()
                && compare_keys(
                    &context.values[left[left_index]],
                    &context.values[right[right_index]],
                    context.directions,
                    context.order,
                    context.control,
                )? != Ordering::Greater;
        *target = if take_left {
            let index = left[left_index];
            left_index += 1;
            index
        } else {
            let index = right[right_index];
            right_index += 1;
            index
        };
    }
    Ok(())
}

fn apply_permutation(
    rows: &mut [ProjectedRow],
    indices: Vec<usize>,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let _memory = control.reserve(crate::query::resources::allocation_bytes::<usize>(
        indices.len(),
    )?)?;
    let mut positions = vec![0; indices.len()];
    for (target, source) in indices.into_iter().enumerate() {
        positions[source] = target;
    }
    for source in 0..rows.len() {
        while positions[source] != source {
            control.work(1)?;
            let target = positions[source];
            rows.swap(source, target);
            positions.swap(source, target);
        }
    }
    Ok(())
}
