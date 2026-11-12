//! Query-owned binding for repeated single-property equality probes.
//!
//! The owning MATCH retains this handle across outer rows, while each concrete
//! key and page belongs to one inner cursor. Owners never move a binding across
//! executions or connections. Snapshot identity and the actual cache source are
//! checked again before every page; a lost source is an error, not exhaustion.

use std::mem::size_of;

use rusqlite::{params, types::ValueRef};

use crate::cypher::Value;
use crate::query::expression::clone_value_unreserved_controlled;
use crate::query::mutation::property_from_value_controlled;
use crate::query::resources::{
    ExecutionControl, MemoryReservation, allocation_bytes, checked_add, checked_mul,
    value_bytes_controlled,
};
use crate::query::{QueryError, QueryResult};
use crate::storage::{
    self, HashId, IndexDefinition, IndexTarget, OwnerKind, Snapshot, StandardIndexKind,
};

use super::cache::{StandardIndexCursor, StandardIndexPage};
use super::persistent::{
    definition_workspace, ensure_index_cache, equality_key, index_definition_identity,
};

#[derive(Debug, PartialEq, Eq)]
enum BoundSource {
    Local,
    Persistent { generation_id: i64, anchor: HashId },
}

/// One lazy binding in one MATCH execution. Deliberately not `Clone`.
#[derive(Debug)]
pub(crate) struct BoundStandardIndexRead {
    snapshot_identity: HashId,
    index_name: String,
    owner_kind: OwnerKind,
    definition_hash: HashId,
    source: BoundSource,
    _memory: MemoryReservation,
}

impl BoundStandardIndexRead {
    pub(crate) fn owner_kind(&self) -> OwnerKind {
        self.owner_kind
    }
}

pub(crate) fn bind_standard_index_read(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    expected_definition_hash: HashId,
    control: &ExecutionControl<'_>,
) -> QueryResult<BoundStandardIndexRead> {
    let memory = control.reserve(checked_add(
        size_of::<BoundStandardIndexRead>() as u64,
        index_name.len() as u64,
    )?)?;
    let execution =
        crate::query::version::VersionExecution::new(snapshot.connection_for_query(), control);
    let loaded = if snapshot.query_schema_state().is_none() {
        Some(execution.load_schema(snapshot.commit())?)
    } else {
        None
    };
    let schema = snapshot
        .query_schema_state()
        .or_else(|| loaded.as_ref().map(|loaded| &loaded.schema))
        .ok_or_else(|| QueryError::internal("bound Index read is missing Schema"))?;
    let index = schema.indexes.get(index_name).ok_or_else(|| {
        QueryError::internal("bound Index definition is absent from the active Schema")
    })?;
    bind_index_definition(snapshot, index, expected_definition_hash, memory, control)
}

fn bind_index_definition(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    expected_definition_hash: HashId,
    mut memory: MemoryReservation,
    control: &ExecutionControl<'_>,
) -> QueryResult<BoundStandardIndexRead> {
    let owner_kind = equality_owner_kind(index)?;
    memory.resize(checked_add(
        size_of::<BoundStandardIndexRead>() as u64,
        index.name.len() as u64,
    )?)?;
    let _definition = control.reserve(definition_workspace(index, control)?)?;
    let (_, definition_hash) = index_definition_identity(index)?;
    if definition_hash != expected_definition_hash {
        return Err(QueryError::internal("bound Index definition changed"));
    }
    // Full definition/lineage/overlay work happens only at this lazy boundary.
    ensure_index_cache(snapshot, index, control)?;
    let bound = BoundStandardIndexRead {
        snapshot_identity: snapshot.cache_identity(),
        index_name: index.name.to_owned(),
        owner_kind,
        definition_hash,
        source: read_bound_source(snapshot, &index.name)?,
        _memory: memory,
    };
    validate_bound_source(snapshot, &bound, control)?;
    Ok(bound)
}

fn equality_owner_kind(index: &IndexDefinition) -> QueryResult<OwnerKind> {
    if !matches!(
        index.kind,
        StandardIndexKind::Range | StandardIndexKind::Text
    ) || !index.additional_properties.is_empty()
        || !index.labels_or_types.is_empty()
        || index.configuration.is_some()
    {
        return Err(QueryError::internal("unsupported bound equality Index"));
    }
    match &index.target {
        IndexTarget::NodeProperties { properties, .. } if properties.len() == 1 => {
            Ok(OwnerKind::Node)
        }
        IndexTarget::RelationshipProperties { properties, .. } if properties.len() == 1 => {
            Ok(OwnerKind::Relationship)
        }
        _ => Err(QueryError::internal(
            "bound equality requires one indexed property",
        )),
    }
}

fn read_bound_source(snapshot: &Snapshot<'_>, index_name: &str) -> QueryResult<BoundSource> {
    let mut statement = snapshot.connection_for_query().prepare(
        "SELECT generation_id, generation_anchor \
         FROM temp._lithograph_standard_index_cache_meta \
         WHERE snapshot_hash = ?1 AND index_name = ?2 AND complete = 1",
    )?;
    let mut rows = statement.query(params![
        snapshot.cache_identity().as_bytes().as_slice(),
        index_name,
    ])?;
    let Some(row) = rows.next()? else {
        return Err(lost_bound_source());
    };
    match (row.get::<_, Option<i64>>(0)?, row.get_ref(1)?) {
        (None, ValueRef::Null) => Ok(BoundSource::Local),
        (Some(generation_id), ValueRef::Blob(anchor)) if generation_id > 0 => {
            Ok(BoundSource::Persistent {
                generation_id,
                anchor: HashId::from_slice(anchor)?,
            })
        }
        _ => Err(lost_bound_source()),
    }
}

fn validate_bound_source(
    snapshot: &Snapshot<'_>,
    bound: &BoundStandardIndexRead,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    control.check()?;
    if snapshot.cache_identity() != bound.snapshot_identity {
        return Err(QueryError::internal("bound Index Snapshot changed"));
    }
    let valid = match bound.source {
        BoundSource::Local => snapshot.connection_for_query().query_row(
            "SELECT EXISTS(SELECT 1 FROM temp._lithograph_standard_index_cache_meta \
             WHERE snapshot_hash = ?1 AND index_name = ?2 AND complete = 1 \
             AND generation_id IS NULL AND generation_anchor IS NULL)",
            params![
                bound.snapshot_identity.as_bytes().as_slice(),
                bound.index_name
            ],
            |row| row.get::<_, bool>(0),
        )?,
        BoundSource::Persistent {
            generation_id,
            anchor,
        } => persistent_source_valid(snapshot, bound, generation_id, anchor)?,
    };
    control.check()?;
    if valid {
        Ok(())
    } else {
        Err(lost_bound_source())
    }
}

fn persistent_source_valid(
    snapshot: &Snapshot<'_>,
    bound: &BoundStandardIndexRead,
    generation_id: i64,
    anchor: HashId,
) -> QueryResult<bool> {
    Ok(snapshot.connection_for_query().query_row(
        "SELECT EXISTS(SELECT 1 FROM temp._lithograph_standard_index_cache_meta AS meta \
         JOIN main._lithograph_index_generations AS generation \
         ON generation.generation_id = meta.generation_id \
         WHERE meta.snapshot_hash = ?1 AND meta.index_name = ?2 AND meta.complete = 1 \
         AND meta.generation_id = ?3 AND meta.generation_anchor = ?4 \
         AND typeof(meta.complete) = 'integer' AND typeof(meta.generation_id) = 'integer' \
         AND typeof(meta.generation_anchor) = 'blob' \
         AND generation.anchor_commit = ?4 AND generation.complete = 1 \
         AND generation.encoding_version = ?5 AND generation.definition_hash = ?6 \
         AND typeof(generation.generation_id) = 'integer' \
         AND typeof(generation.anchor_commit) = 'blob' \
         AND typeof(generation.complete) = 'integer' \
         AND typeof(generation.encoding_version) = 'integer' \
         AND typeof(generation.definition_hash) = 'blob' \
         AND typeof(generation.definition_blob) = 'blob')",
        params![
            bound.snapshot_identity.as_bytes().as_slice(),
            bound.index_name,
            generation_id,
            anchor.as_bytes().as_slice(),
            storage::STANDARD_INDEX_ENCODING_VERSION,
            bound.definition_hash.as_bytes().as_slice(),
        ],
        |row| row.get(0),
    )?)
}

fn lost_bound_source() -> QueryError {
    QueryError::internal("bound Index read source changed during execution")
}

pub(crate) fn scan_bound_index_page(
    snapshot: &Snapshot<'_>,
    bound: &BoundStandardIndexRead,
    value: &Value,
    cursor: Option<&StandardIndexCursor>,
    limit: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<StandardIndexPage> {
    validate_bound_source(snapshot, bound, control)?;
    let after = equality_cursor_after(cursor)?;
    if limit == 0 {
        return Err(QueryError::internal(
            "bound Index page limit must be positive",
        ));
    }
    // The concrete key's caller keeps its input lease. Prepay conversion,
    // equality encoding and the bounded output before copying any of them.
    let _workspace = control.reserve(checked_add(
        checked_add(
            16_384,
            checked_mul(value_bytes_controlled(value, control)?, 16)?,
        )?,
        allocation_bytes::<i64>(limit)?,
    )?)?;
    let key = equality_probe_key(value, control)?;
    let Some(key) = key else {
        return Ok(StandardIndexPage {
            items: Vec::new(),
            next_cursor: None,
        });
    };
    scan_equality_page(snapshot, bound, &key, after, limit, control)
}

fn equality_cursor_after(cursor: Option<&StandardIndexCursor>) -> QueryResult<i64> {
    match cursor {
        None => Ok(0),
        Some(StandardIndexCursor::Owner(owner)) if *owner > 0 => Ok(*owner),
        _ => Err(QueryError::internal("invalid bound equality Index cursor")),
    }
}

fn equality_probe_key(
    value: &Value,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<Vec<u8>>> {
    if !matches!(
        value,
        Value::Null | Value::Boolean(_) | Value::Integer(_) | Value::Float(_) | Value::String(_)
    ) {
        return Err(QueryError::internal(
            "unsupported bound equality key family",
        ));
    }
    let owned = clone_value_unreserved_controlled(value, Some(control))?;
    let Some(property) = property_from_value_controlled(owned, control)? else {
        return Ok(None);
    };
    equality_key(&property, control)
}

fn scan_equality_page(
    snapshot: &Snapshot<'_>,
    bound: &BoundStandardIndexRead,
    key: &[u8],
    after: i64,
    limit: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<StandardIndexPage> {
    let mut statement = snapshot.connection_for_query().prepare(
        "SELECT owner_id FROM temp._lithograph_standard_index_cache \
         WHERE snapshot_hash = ?1 AND index_name = ?2 AND owner_kind = ?3 \
         AND property_ordinal = 0 AND owner_id > ?4 AND equality_blob = ?5 \
         ORDER BY owner_id LIMIT ?6",
    )?;
    let mut rows = statement.query(params![
        bound.snapshot_identity.as_bytes().as_slice(),
        bound.index_name,
        bound.owner_kind as i64,
        after,
        key,
        i64::try_from(limit)
            .map_err(|_| QueryError::internal("bound Index page limit overflow"))?,
    ])?;
    let mut items = Vec::with_capacity(limit);
    while let Some(row) = rows.next()? {
        control.work(1)?;
        items.push(row.get(0)?);
    }
    control.check()?;
    let next_cursor = (items.len() == limit)
        .then(|| items.last().copied().map(StandardIndexCursor::Owner))
        .flatten();
    Ok(StandardIndexPage { items, next_cursor })
}

#[cfg(test)]
mod tests;
