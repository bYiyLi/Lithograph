//! Optional, exact-anchor binding for the existing numeric Range producer.

use super::*;
use crate::cypher::Value;
use crate::query::{QueryErrorKind, schema::StandardIndexSeek};

#[cfg(test)]
mod tests;

/// Execution-local ordering evidence. It owns no values or cache allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OrderedNumericIndexRead {
    snapshot_identity: HashId,
    definition_hash: HashId,
    generation_id: i64,
}

impl OrderedNumericIndexRead {
    pub(in crate::query::schema::index) fn generation_id(&self) -> i64 {
        self.generation_id
    }
}

pub(crate) fn try_bind_topk_numeric_index(
    snapshot: &Snapshot<'_>,
    seek: &StandardIndexSeek,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<OrderedNumericIndexRead>> {
    let Some(index) = topk_index(snapshot, seek, control, budget)? else {
        return Ok(None);
    };
    let _memory = match control.reserve(definition_workspace(index, control)?) {
        Ok(memory) => memory,
        Err(error) if error.kind == QueryErrorKind::Resource => return Ok(None),
        Err(error) => return Err(error),
    };
    let (definition, definition_hash) = index_definition_identity(index)?;
    if !persistent_index_storage_available(snapshot.connection_for_query())? {
        return Ok(None);
    }
    let Some(generation_id) = exact_generation(snapshot, &definition, definition_hash, control)?
    else {
        return Ok(None);
    };
    if !super::super::standard_index_cache_allowed(snapshot, control, budget)?
        || !initialize_binding(snapshot, index, generation_id, control, budget)?
    {
        return Ok(None);
    }
    Ok(Some(OrderedNumericIndexRead {
        snapshot_identity: snapshot.cache_identity(),
        definition_hash,
        generation_id,
    }))
}

fn topk_index<'a>(
    snapshot: &'a Snapshot<'_>,
    seek: &StandardIndexSeek,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<&'a IndexDefinition>> {
    control.check()?;
    if snapshot.query_local_layer().is_some()
        || snapshot.cache_identity() != snapshot.commit()
        || !numeric_topk_seek(seek)
        || !budget.work(1, 0)
    {
        return Ok(None);
    }
    let Some(index) = snapshot
        .query_schema_state()
        .and_then(|s| s.indexes.get(&seek.index_name))
    else {
        return Ok(None);
    };
    if !simple_node_range(index) || !definition_budget(index, control, budget)? {
        return Ok(None);
    }
    Ok(Some(index))
}

pub(in crate::query::schema::index) fn numeric_topk_seek(seek: &StandardIndexSeek) -> bool {
    seek.kind == StandardIndexKind::Range
        && matches!(seek.predicates.as_slice(), [(0, super::super::StandardIndexPredicate::Bounds { lower, upper })]
            if (lower.is_some() || upper.is_some())
                && lower.as_ref().is_none_or(|(v, _)| matches!(v, Value::Integer(_)))
                && upper.as_ref().is_none_or(|(v, _)| matches!(v, Value::Integer(_))))
}

fn simple_node_range(index: &IndexDefinition) -> bool {
    index.kind == StandardIndexKind::Range
        && index.configuration.is_none()
        && index.labels_or_types.is_empty()
        && index.additional_properties.is_empty()
        && matches!(&index.target, IndexTarget::NodeProperties { properties, .. } if properties.len() == 1)
}

fn definition_budget(
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<bool> {
    let IndexTarget::NodeProperties { label, properties } = &index.target else {
        return Ok(false);
    };
    let bytes = index
        .name
        .len()
        .saturating_add(label.len())
        .saturating_add(properties[0].len())
        .saturating_add(index.owning_constraint.as_ref().map_or(0, String::len));
    let work = bytes.saturating_mul(6).saturating_add(512).div_ceil(64);
    control.work(work as u64)?;
    Ok(budget.work(
        work.saturating_add(16),
        bytes.saturating_mul(64).saturating_add(16_384),
    ))
}

fn exact_generation(
    snapshot: &Snapshot<'_>,
    definition: &[u8],
    definition_hash: HashId,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<i64>> {
    control.check()?;
    let mut statement = snapshot.connection_for_query().prepare(
        "SELECT generation_id FROM main._lithograph_index_generations \
         WHERE anchor_commit=?1 AND definition_hash=?2 AND encoding_version=?3 AND complete=1 \
         AND typeof(generation_id)='integer' AND generation_id>0 \
         AND typeof(anchor_commit)='blob' AND typeof(definition_hash)='blob' \
         AND typeof(encoding_version)='integer' AND typeof(complete)='integer' \
         AND CASE WHEN typeof(definition_blob)='blob' AND length(definition_blob)=?5 \
             THEN definition_blob END=?4",
    )?;
    statement
        .query_row(
            params![
                snapshot.commit().as_bytes().as_slice(),
                definition_hash.as_bytes().as_slice(),
                INDEX_ENCODING_VERSION,
                definition,
                definition.len() as i64
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

fn initialize_binding(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    generation_id: i64,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<bool> {
    let connection = snapshot.connection_for_query();
    if !optional_temp_ddl(connection, CACHE_TABLES_SQL, control, budget)? {
        return Ok(false);
    }
    // Never clear or index a prior full local materialization to obtain TopK.
    if !optional_temp(empty_overlay(snapshot, &index.name))?.unwrap_or(false)
        || !optional_temp(connection.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM temp._lithograph_standard_index_cache_local)",
            [],
            |row| row.get::<_, bool>(0),
        ))?
        .unwrap_or(false)
        || !optional_temp_ddl(connection, CACHE_RANGE_INDEXES_SQL, control, budget)?
    {
        return Ok(false);
    }
    control.check()?;
    let Some(mut statement) = optional_temp(connection.prepare(
        "INSERT OR REPLACE INTO temp._lithograph_standard_index_cache_meta \
         (snapshot_hash,index_name,complete,generation_id,generation_anchor) VALUES(?1,?2,1,?3,?4)",
    ))?
    else {
        return Ok(false);
    };
    if statement.readonly() {
        return Ok(false);
    }
    Ok(optional_temp(statement.execute(params![
        snapshot.cache_identity().as_bytes().as_slice(),
        index.name,
        generation_id,
        snapshot.commit().as_bytes().as_slice()
    ]))? == Some(1))
}

fn empty_overlay(snapshot: &Snapshot<'_>, index_name: &str) -> rusqlite::Result<bool> {
    snapshot.connection_for_query().query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM temp._lithograph_standard_index_cache_local \
         WHERE snapshot_hash=?1 AND index_name=?2) \
         AND NOT EXISTS(SELECT 1 FROM temp._lithograph_standard_index_changed_owners \
         WHERE snapshot_hash=?1 AND index_name=?2)",
        params![snapshot.cache_identity().as_bytes().as_slice(), index_name],
        |row| row.get(0),
    )
}

fn optional_temp<T>(result: rusqlite::Result<T>) -> QueryResult<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::SqliteFailure(code, _))
            if matches!(
                code.extended_code & 0xff,
                rusqlite::ffi::SQLITE_AUTH
                    | rusqlite::ffi::SQLITE_PERM
                    | rusqlite::ffi::SQLITE_READONLY
                    | rusqlite::ffi::SQLITE_CANTOPEN
                    | rusqlite::ffi::SQLITE_FULL
                    | rusqlite::ffi::SQLITE_NOMEM
                    | rusqlite::ffi::SQLITE_TOOBIG
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

fn optional_temp_ddl(
    connection: &Connection,
    sql: &str,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<bool> {
    for sql in sql.split(';').filter(|s| !s.trim().is_empty()) {
        control.check()?;
        if !budget.work(1, 0) {
            return Ok(false);
        }
        let Some(mut statement) = optional_temp(connection.prepare(sql))? else {
            return Ok(false);
        };
        if statement.readonly() || optional_temp(statement.execute([]))?.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(in crate::query::schema::index) fn validate_topk_numeric_binding(
    snapshot: &Snapshot<'_>,
    proof: &OrderedNumericIndexRead,
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    control.check()?;
    if snapshot.query_local_layer().is_some()
        || snapshot.cache_identity() != proof.snapshot_identity
        || snapshot.commit() != proof.snapshot_identity
        || !simple_node_range(index)
    {
        return Err(lost_source());
    }
    let _memory = control.reserve(definition_workspace(index, control)?)?;
    let (definition, hash) = index_definition_identity(index)?;
    if hash != proof.definition_hash {
        return Err(lost_source());
    }
    let valid = snapshot.connection_for_query().query_row(
        "SELECT EXISTS(SELECT 1 FROM temp._lithograph_standard_index_cache_meta AS meta \
         JOIN main._lithograph_index_generations AS g ON g.generation_id=meta.generation_id \
         WHERE meta.snapshot_hash=?1 AND meta.index_name=?2 AND meta.complete=1 \
         AND meta.generation_id=?3 AND meta.generation_anchor=?1 \
         AND typeof(meta.complete)='integer' AND typeof(meta.generation_id)='integer' \
         AND typeof(meta.generation_anchor)='blob' AND g.anchor_commit=?1 \
         AND g.definition_hash=?4 AND g.encoding_version=?6 AND g.complete=1 \
         AND CASE WHEN typeof(g.definition_blob)='blob' AND length(g.definition_blob)=?7 \
             THEN g.definition_blob END=?5 \
         AND typeof(g.complete)='integer' AND typeof(g.encoding_version)='integer' \
         AND typeof(g.anchor_commit)='blob' AND typeof(g.definition_hash)='blob' \
         AND typeof(g.definition_blob)='blob')",
        params![
            proof.snapshot_identity.as_bytes().as_slice(),
            index.name,
            proof.generation_id,
            hash.as_bytes().as_slice(),
            definition,
            INDEX_ENCODING_VERSION,
            definition.len() as i64
        ],
        |row| row.get::<_, bool>(0),
    )?;
    if !valid || !empty_overlay(snapshot, &index.name)? {
        return Err(lost_source());
    }
    control.check()
}

fn lost_source() -> QueryError {
    QueryError::internal("ordered numeric Index binding changed during TopK execution")
}
