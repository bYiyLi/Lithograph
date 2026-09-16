use rusqlite::{OptionalExtension, params, types::Value as SqlValue};

use crate::cypher::Value;
use crate::query::{QueryError, QueryResult};
use crate::storage::{ScanPage, Snapshot};

use super::{
    RANGE_FAMILY_NUMBER, StandardIndexCursor, StandardIndexPage, index_scan_parameters,
    numeric_range_value,
};

#[allow(
    clippy::too_many_arguments,
    reason = "the bounded Range cache helper keeps the complete indexed bound/page contract explicit"
)]
pub(super) fn scan_range_bounds_page(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    after: i64,
    limit: usize,
    lower: &Option<(Value, bool)>,
    upper: &Option<(Value, bool)>,
) -> QueryResult<Option<ScanPage<i64>>> {
    let Some(bounds) = numeric_range_bounds(lower, upper) else {
        return Ok(None);
    };
    let NumericRangeBounds { lower, upper } = bounds;

    let mut sql = "SELECT owner_id FROM temp._lithograph_standard_index_cache \
         WHERE snapshot_hash = ? AND index_name = ? AND owner_kind = ? \
         AND property_ordinal = ? AND owner_id > ? AND sort_family = ?"
        .to_owned();
    let mut parameters = index_scan_parameters(snapshot, index_name, owner_kind, ordinal, after);
    parameters.push(SqlValue::Integer(RANGE_FAMILY_NUMBER));
    if let Some((value, inclusive)) = lower {
        sql.push_str(if inclusive {
            " AND sort_number >= ?"
        } else {
            " AND sort_number > ?"
        });
        parameters.push(value);
    }
    if let Some((value, inclusive)) = upper {
        sql.push_str(if inclusive {
            " AND sort_number <= ?"
        } else {
            " AND sort_number < ?"
        });
        parameters.push(value);
    }
    sql.push_str(" ORDER BY owner_id LIMIT ?");
    parameters.push(SqlValue::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));

    let mut statement = snapshot.connection_for_query().prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(parameters), |row| row.get(0))?;
    let items = rows.collect::<Result<Vec<i64>, _>>()?;
    let next_after = (items.len() == limit)
        .then(|| items.last().copied())
        .flatten();
    Ok(Some(ScanPage { items, next_after }))
}

pub(super) struct NumericRangeBounds {
    lower: Option<(SqlValue, bool)>,
    upper: Option<(SqlValue, bool)>,
}

pub(super) fn numeric_range_bounds(
    lower: &Option<(Value, bool)>,
    upper: &Option<(Value, bool)>,
) -> Option<NumericRangeBounds> {
    let lower = numeric_range_bound(lower)?;
    let upper = numeric_range_bound(upper)?;
    (lower.is_some() || upper.is_some()).then_some(NumericRangeBounds { lower, upper })
}

fn numeric_range_bound(bound: &Option<(Value, bool)>) -> Option<Option<(SqlValue, bool)>> {
    match bound {
        Some((value, inclusive)) => {
            numeric_range_value(value).map(|value| Some((value, *inclusive)))
        }
        None => Some(None),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the Range cursor page binds one immutable generation, query-local overlay and both numeric bounds"
)]
pub(super) fn scan_persistent_numeric_range_page(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    cursor: Option<&StandardIndexCursor>,
    limit: usize,
    bounds: &NumericRangeBounds,
) -> QueryResult<Option<StandardIndexPage>> {
    let Some(generation_id) = bound_generation_id(snapshot, index_name)? else {
        return Ok(None);
    };
    let range_cursor = numeric_range_cursor(cursor, generation_id)?;
    let spec = NumericRangePageSpec {
        index_name,
        generation_id,
        owner_kind,
        ordinal,
        cursor: range_cursor,
        limit,
        bounds,
    };

    #[cfg(feature = "test-support")]
    let started = std::time::Instant::now();
    let (sql, parameters) = numeric_range_page_query(snapshot, &spec);
    let mut statement = snapshot.connection_for_query().prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(parameters), |row| {
        Ok((row.get::<_, SqlValue>(0)?, row.get::<_, i64>(1)?))
    })?;
    let keyed = rows.collect::<Result<Vec<_>, _>>()?;
    let next_cursor = numeric_range_next_cursor(generation_id, &keyed, limit);
    let items = keyed.into_iter().map(|(_, owner_id)| owner_id).collect();
    #[cfg(feature = "test-support")]
    crate::performance::record_standard_index_scan(started.elapsed().as_micros());
    Ok(Some(StandardIndexPage { items, next_cursor }))
}

struct NumericRangePageSpec<'a> {
    index_name: &'a str,
    generation_id: i64,
    owner_kind: i64,
    ordinal: usize,
    cursor: Option<(&'a SqlValue, i64)>,
    limit: usize,
    bounds: &'a NumericRangeBounds,
}

fn bound_generation_id(snapshot: &Snapshot<'_>, index_name: &str) -> QueryResult<Option<i64>> {
    snapshot
        .connection_for_query()
        .query_row(
            "SELECT generation_id FROM temp._lithograph_standard_index_cache_meta \
             WHERE snapshot_hash = ?1 AND index_name = ?2 AND complete = 1",
            params![snapshot.cache_identity().as_bytes().as_slice(), index_name],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
        .map(|value| value.flatten())
        .map_err(Into::into)
}

fn numeric_range_cursor(
    cursor: Option<&StandardIndexCursor>,
    generation_id: i64,
) -> QueryResult<Option<(&SqlValue, i64)>> {
    match cursor {
        None => Ok(None),
        Some(StandardIndexCursor::NumericRange {
            generation_id: cursor_generation,
            sort_number,
            owner_id,
        }) if *cursor_generation == generation_id => Ok(Some((sort_number, *owner_id))),
        Some(StandardIndexCursor::NumericRange { .. }) => Err(QueryError::internal(
            "persistent Range Index generation changed during paginated scan",
        )),
        Some(StandardIndexCursor::Owner(_)) => Err(QueryError::internal(
            "owner-ordered cursor reached a numeric Range scan",
        )),
    }
}

fn numeric_range_page_query(
    snapshot: &Snapshot<'_>,
    spec: &NumericRangePageSpec<'_>,
) -> (String, Vec<SqlValue>) {
    let mut sql = "SELECT sort_number, owner_id FROM (\
         SELECT entries.sort_number AS sort_number, entries.owner_id AS owner_id \
         FROM main._lithograph_index_entries AS entries \
         INDEXED BY _lithograph_index_entries_range_number \
         WHERE entries.generation_id = ? AND entries.owner_kind = ? \
           AND entries.property_ordinal = ? AND entries.sort_family = ? \
           AND NOT EXISTS (\
             SELECT 1 FROM temp._lithograph_standard_index_changed_owners AS changed \
             WHERE changed.snapshot_hash = ? AND changed.index_name = ? \
               AND changed.owner_kind = entries.owner_kind AND changed.owner_id = entries.owner_id\
           )"
    .to_owned();
    let mut parameters = vec![
        SqlValue::Integer(spec.generation_id),
        SqlValue::Integer(spec.owner_kind),
        SqlValue::Integer(i64::try_from(spec.ordinal).unwrap_or(i64::MAX)),
        SqlValue::Integer(RANGE_FAMILY_NUMBER),
        SqlValue::Blob(snapshot.cache_identity().as_bytes().to_vec()),
        SqlValue::Text(spec.index_name.to_owned()),
    ];
    append_numeric_range_branch(
        &mut sql,
        &mut parameters,
        "entries.sort_number",
        "entries.owner_id",
        spec,
    );
    sql.push_str(
        " UNION ALL SELECT local.sort_number AS sort_number, local.owner_id AS owner_id \
         FROM temp._lithograph_standard_index_cache_local AS local \
         INDEXED BY _lithograph_standard_index_cache_range_number \
         WHERE local.snapshot_hash = ? AND local.index_name = ? AND local.owner_kind = ? \
           AND local.property_ordinal = ? AND local.sort_family = ?",
    );
    parameters.extend([
        SqlValue::Blob(snapshot.cache_identity().as_bytes().to_vec()),
        SqlValue::Text(spec.index_name.to_owned()),
        SqlValue::Integer(spec.owner_kind),
        SqlValue::Integer(i64::try_from(spec.ordinal).unwrap_or(i64::MAX)),
        SqlValue::Integer(RANGE_FAMILY_NUMBER),
    ]);
    append_numeric_range_branch(
        &mut sql,
        &mut parameters,
        "local.sort_number",
        "local.owner_id",
        spec,
    );
    sql.push_str(") ORDER BY sort_number, owner_id LIMIT ?");
    parameters.push(SqlValue::Integer(
        i64::try_from(spec.limit).unwrap_or(i64::MAX),
    ));
    (sql, parameters)
}

fn append_numeric_range_branch(
    sql: &mut String,
    parameters: &mut Vec<SqlValue>,
    sort_column: &str,
    owner_column: &str,
    spec: &NumericRangePageSpec<'_>,
) {
    append_numeric_bounds_for_column(
        sql,
        parameters,
        sort_column,
        spec.bounds.lower.as_ref(),
        spec.bounds.upper.as_ref(),
    );
    append_numeric_range_cursor(sql, parameters, sort_column, owner_column, spec.cursor);
}

fn numeric_range_next_cursor(
    generation_id: i64,
    keyed: &[(SqlValue, i64)],
    limit: usize,
) -> Option<StandardIndexCursor> {
    if keyed.len() != limit {
        return None;
    }
    keyed.last().map(
        |(sort_number, owner_id)| StandardIndexCursor::NumericRange {
            generation_id,
            sort_number: sort_number.clone(),
            owner_id: *owner_id,
        },
    )
}

fn append_numeric_bounds_for_column(
    sql: &mut String,
    parameters: &mut Vec<SqlValue>,
    column: &str,
    lower: Option<&(SqlValue, bool)>,
    upper: Option<&(SqlValue, bool)>,
) {
    if let Some((value, inclusive)) = lower {
        sql.push_str(&format!(
            " AND {column} {} ?",
            if *inclusive { ">=" } else { ">" }
        ));
        parameters.push(value.clone());
    }
    if let Some((value, inclusive)) = upper {
        sql.push_str(&format!(
            " AND {column} {} ?",
            if *inclusive { "<=" } else { "<" }
        ));
        parameters.push(value.clone());
    }
}

fn append_numeric_range_cursor(
    sql: &mut String,
    parameters: &mut Vec<SqlValue>,
    sort_column: &str,
    owner_column: &str,
    cursor: Option<(&SqlValue, i64)>,
) {
    if let Some((sort_number, owner_id)) = cursor {
        sql.push_str(&format!(" AND ({sort_column}, {owner_column}) > (?, ?)"));
        parameters.push(sort_number.clone());
        parameters.push(SqlValue::Integer(owner_id));
    }
}
