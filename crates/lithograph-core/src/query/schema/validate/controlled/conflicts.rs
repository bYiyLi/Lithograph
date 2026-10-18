//! Keep merge's per-definition conflict order while owning every retained error.

use super::*;

pub(crate) fn validation_conflicts_controlled(
    connection: &Connection,
    schema: &SchemaState,
    snapshot: &Snapshot<'_>,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<(String, QueryError)>, MemoryReservation)> {
    let validation = Validation {
        connection,
        snapshot,
        control,
    };
    let mut output = Conflicts {
        values: Vec::new(),
        memory: control.reserve(0)?,
    };
    collect_nodes(&validation, schema, &mut output)?;
    collect_relationships(&validation, schema, &mut output)?;
    collect_constraints(&validation, schema, &mut output)?;
    control.check()?;
    Ok((output.values, output.memory))
}

fn collect_nodes(
    validation: &Validation<'_, '_, '_>,
    schema: &SchemaState,
    output: &mut Conflicts,
) -> QueryResult<()> {
    for (name, definition) in &schema.graph_nodes {
        let names = definition
            .properties
            .keys()
            .chain(definition.implied_labels.iter())
            .map(String::as_str)
            .chain([name.as_str(), definition.label.as_str()]);
        let bytes = error_workspace(names, validation.control)?;
        output.record("graph/node/", name, bytes, validation.control, || {
            validation.graph_node(definition)
        })?;
    }
    Ok(())
}

fn collect_relationships(
    validation: &Validation<'_, '_, '_>,
    schema: &SchemaState,
    output: &mut Conflicts,
) -> QueryResult<()> {
    for (name, definition) in &schema.graph_relationships {
        let names = definition
            .properties
            .keys()
            .map(String::as_str)
            .chain(definition.source_label.as_deref())
            .chain(definition.target_label.as_deref())
            .chain([name.as_str(), definition.relationship_type.as_str()]);
        let bytes = error_workspace(names, validation.control)?;
        output.record(
            "graph/relationship/",
            name,
            bytes,
            validation.control,
            || validation.graph_relationship(definition),
        )?;
    }
    Ok(())
}

fn collect_constraints(
    validation: &Validation<'_, '_, '_>,
    schema: &SchemaState,
    output: &mut Conflicts,
) -> QueryResult<()> {
    for (name, definition) in &schema.constraints {
        let target = match &definition.target {
            SchemaTarget::Node { label } => label,
            SchemaTarget::Relationship { relationship_type } => relationship_type,
        };
        let names = definition.properties.iter().map(String::as_str).chain([
            name.as_str(),
            definition.name.as_str(),
            target.as_str(),
        ]);
        let bytes = error_workspace(names, validation.control)?;
        output.record("constraint/", name, bytes, validation.control, || {
            validation.constraint(definition)
        })?;
    }
    Ok(())
}

fn error_workspace<'a>(
    names: impl Iterator<Item = &'a str>,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let mut maximum = 0;
    for name in names {
        control.work(1)?;
        maximum = maximum.max(name.len() as u64);
    }
    // A slot and diagnostic contain at most three source names and fixed
    // wording/ids. Include String growth and the simultaneous formatting copy.
    checked_mul(checked_add(checked_mul(maximum, 3)?, 512)?, 8)
}

struct Conflicts {
    values: Vec<(String, QueryError)>,
    memory: MemoryReservation,
}

impl Conflicts {
    fn record(
        &mut self,
        prefix: &str,
        name: &str,
        bytes: u64,
        control: &ExecutionControl<'_>,
        validate: impl FnOnce() -> QueryResult<()>,
    ) -> QueryResult<()> {
        let work = control.reserve(bytes)?;
        match validate() {
            Ok(()) => Ok(()),
            Err(error) if error.kind == QueryErrorKind::Constraint => {
                self.memory.merge(work)?;
                self.reserve_slot(control)?;
                let slot = slot_text(prefix, name, control)?;
                self.values.push((slot, error));
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn reserve_slot(&mut self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        control.check()?;
        if self.values.len() < self.values.capacity() {
            return Ok(());
        }
        let capacity = self.values.capacity().saturating_mul(2).max(4);
        let old = allocation_bytes::<(String, QueryError)>(self.values.capacity())?;
        self.memory
            .grow(allocation_bytes::<(String, QueryError)>(capacity)?)?;
        self.values
            .try_reserve_exact(capacity - self.values.len())
            .map_err(|_| memory_error("cannot allocate Schema conflict buffer"))?;
        self.memory.resize(self.memory.bytes() - old)
    }
}

fn slot_text(prefix: &str, name: &str, control: &ExecutionControl<'_>) -> QueryResult<String> {
    let bytes = prefix
        .len()
        .checked_add(name.len())
        .ok_or_else(|| memory_error("Schema conflict slot size overflow"))?;
    let mut output = String::with_capacity(bytes);
    output.push_str(prefix);
    let mut offset = 0;
    while offset < name.len() {
        let mut end = (offset + 64 * 1024).min(name.len());
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        control.check()?;
        output.push_str(&name[offset..end]);
        offset = end;
    }
    Ok(output)
}

#[cfg(test)]
mod tests;
