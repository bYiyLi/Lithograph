use crate::storage::{SchemaState, StandardIndexKind};

use super::super::{AccessPolicy, QueryResult};

/// Newly introduced host-backed index definitions call a Provider or tokenizer
/// during validation. Unchanged definitions and removals have no such effect.
pub(crate) fn validate_external_transition(
    previous: &SchemaState,
    next: &SchemaState,
    access: &AccessPolicy,
) -> QueryResult<()> {
    if access.read.allow_external_io {
        return Ok(());
    }
    for (name, index) in &next.indexes {
        if !previous
            .indexes
            .get(name)
            .map(|previous| previous.canonical_eq(index))
            .transpose()?
            .unwrap_or(false)
            && matches!(
                index.kind,
                StandardIndexKind::FullText | StandardIndexKind::Semantic
            )
        {
            access.check_external_io()?;
        }
    }
    Ok(())
}
