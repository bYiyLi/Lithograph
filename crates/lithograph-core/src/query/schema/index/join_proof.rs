//! Planning-only capabilities for an already typed dynamic equality probe.

use super::*;

pub(crate) fn join_index_cache_allowed(
    snapshot: &Snapshot<'_>,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<bool> {
    super::standard_index_cache_allowed(snapshot, control, budget)
}

/// These templates admit only the small, ordinary single-property definition.
/// Prepay its canonical serialization before allocating any owned JSON/blob.
pub(crate) fn join_index_definition_hash(
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<crate::storage::HashId>> {
    control.work(1)?;
    if index.configuration.is_some()
        || !index.additional_properties.is_empty()
        || !index.labels_or_types.is_empty()
    {
        return Ok(None);
    }
    let (scope, properties) = match &index.target {
        IndexTarget::NodeProperties { label, properties } => (label, properties),
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => (relationship_type, properties),
        _ => return Ok(None),
    };
    let [property] = properties.as_slice() else {
        return Ok(None);
    };
    let bytes = index
        .name
        .len()
        .saturating_add(scope.len())
        .saturating_add(property.len())
        .saturating_add(index.owning_constraint.as_ref().map_or(0, String::len));
    // JSON escaping is at most six bytes per source byte; the admitted fixed
    // fields fit the constant allowance. Hash/serialization work is prepaid
    // alongside the simultaneously live canonicalization workspace.
    let units = bytes.saturating_mul(6).saturating_add(512).div_ceil(64);
    control.work(units as u64)?;
    if !budget.work(units, bytes.saturating_mul(64).saturating_add(4096)) {
        return Ok(None);
    }
    let blob = index.canonical_identity_blob()?;
    control.work(blob.len().div_ceil(64) as u64)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"LITHOGRAPH_INDEX_DEFINITION_V1");
    hasher.update(&blob);
    Ok(Some(crate::storage::HashId::from_bytes(
        *hasher.finalize().as_bytes(),
    )))
}
