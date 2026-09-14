use std::cmp::Ordering;
use std::collections::BTreeSet;

use rusqlite::{Connection, params, types::Value as SqlValue};

use crate::cypher::{CypherComparison, Value, cypher_compare, cypher_equals};
use crate::storage::{
    self, HashId, IndexDefinition, IndexTarget, OwnerKind, ScanPage, SchemaState, Snapshot,
    StandardIndexKind,
};

use super::super::super::mutation::property_from_value;
use super::super::super::{QueryError, QueryResult};
use super::super::equality::property_equality_key;
use super::{StandardIndexPredicate, StandardIndexSeek};

const NODE_OWNER_KIND: i64 = 0;
const RELATIONSHIP_OWNER_KIND: i64 = 1;
const RANGE_FAMILY_BOOLEAN: i64 = 1;
const RANGE_FAMILY_NUMBER: i64 = 2;
const RANGE_FAMILY_STRING: i64 = 3;
const RANGE_FAMILY_DATE: i64 = 4;
const RANGE_FAMILY_LOCAL_TIME: i64 = 5;
const RANGE_FAMILY_TIME: i64 = 6;
const RANGE_FAMILY_LOCAL_DATETIME: i64 = 7;
const RANGE_FAMILY_ZONED_DATETIME: i64 = 8;
const INDEX_BUILD_PAGE_SIZE: usize = 4_096;

#[derive(Debug, Clone)]
enum RangeOrderKey {
    Number(SqlValue),
    Text(String),
    Tuple {
        family: i64,
        a: i64,
        b: i64,
        c: i64,
        text: String,
    },
}

fn range_order_key(value: &Value) -> Option<RangeOrderKey> {
    match value {
        Value::Boolean(value) => Some(RangeOrderKey::Tuple {
            family: RANGE_FAMILY_BOOLEAN,
            a: i64::from(*value),
            b: 0,
            c: 0,
            text: String::new(),
        }),
        Value::Integer(value) => Some(RangeOrderKey::Number(SqlValue::Integer(*value))),
        Value::Float(value) if !value.is_nan() => {
            Some(RangeOrderKey::Number(SqlValue::Real(*value)))
        }
        Value::String(value) => Some(RangeOrderKey::Text(value.clone())),
        Value::Date(value) => Some(RangeOrderKey::Tuple {
            family: RANGE_FAMILY_DATE,
            a: value.days(),
            b: 0,
            c: 0,
            text: String::new(),
        }),
        Value::LocalTime(value) => Some(RangeOrderKey::Tuple {
            family: RANGE_FAMILY_LOCAL_TIME,
            a: i64::try_from(value.nanoseconds()).ok()?,
            b: 0,
            c: 0,
            text: String::new(),
        }),
        Value::Time(value) => {
            let (instant, offset) = value.comparison_key();
            Some(RangeOrderKey::Tuple {
                family: RANGE_FAMILY_TIME,
                a: i64::try_from(instant).ok()?,
                b: i64::from(offset),
                c: 0,
                text: String::new(),
            })
        }
        Value::LocalDateTime(value) => {
            let (days, nanoseconds) = value.comparison_key();
            Some(RangeOrderKey::Tuple {
                family: RANGE_FAMILY_LOCAL_DATETIME,
                a: days,
                b: i64::try_from(nanoseconds).ok()?,
                c: 0,
                text: String::new(),
            })
        }
        Value::ZonedDateTime(value) => {
            let (instant, offset, zone) = value.comparison_key();
            let seconds = i64::try_from(instant.div_euclid(1_000_000_000)).ok()?;
            let nanoseconds = i64::try_from(instant.rem_euclid(1_000_000_000)).ok()?;
            Some(RangeOrderKey::Tuple {
                family: RANGE_FAMILY_ZONED_DATETIME,
                a: seconds,
                b: nanoseconds,
                c: i64::from(offset),
                text: zone.to_owned(),
            })
        }
        _ => None,
    }
}

pub(crate) fn scan_node_index_after(
    snapshot: &Snapshot<'_>,
    seek: &StandardIndexSeek,
    after: i64,
    limit: usize,
) -> QueryResult<ScanPage<i64>> {
    if seek.kind == StandardIndexKind::Lookup {
        return Err(QueryError::internal(
            "node lookup Index seeks must use the canonical label access path",
        ));
    }
    scan_property_index_after(snapshot, seek, NODE_OWNER_KIND, after, limit)
}

pub(crate) fn scan_relationship_index_after(
    snapshot: &Snapshot<'_>,
    seek: &StandardIndexSeek,
    relationship_type_id: Option<i64>,
    after: i64,
    limit: usize,
) -> QueryResult<ScanPage<i64>> {
    if seek.kind == StandardIndexKind::Lookup {
        let Some(type_id) = relationship_type_id else {
            return Ok(ScanPage {
                items: Vec::new(),
                next_after: None,
            });
        };
        return scan_relationship_lookup_after(snapshot, seek, type_id, after, limit);
    }
    scan_property_index_after(snapshot, seek, RELATIONSHIP_OWNER_KIND, after, limit)
}

fn ensure_planned_index_cache(
    snapshot: &Snapshot<'_>,
    seek: &StandardIndexSeek,
) -> QueryResult<()> {
    let schema = snapshot.schema_state()?;
    let index = schema.indexes.get(&seek.index_name).ok_or_else(|| {
        QueryError::internal(format!(
            "planned Index {} is not present in the active Schema",
            seek.index_name
        ))
    })?;
    ensure_index_cache(snapshot, index)
}

fn scan_relationship_lookup_after(
    snapshot: &Snapshot<'_>,
    seek: &StandardIndexSeek,
    type_id: i64,
    after: i64,
    limit: usize,
) -> QueryResult<ScanPage<i64>> {
    let connection = snapshot.connection_for_query();
    ensure_planned_index_cache(snapshot, seek)?;
    let sql_limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut statement = connection.prepare(
        "SELECT owner_id FROM temp._lithograph_standard_index_cache \
         WHERE snapshot_hash = ?1 AND index_name = ?2 AND owner_kind = ?3 \
         AND token_id = ?4 AND owner_id > ?5 ORDER BY owner_id LIMIT ?6",
    )?;
    let rows = statement.query_map(
        params![
            snapshot.cache_identity().as_bytes().as_slice(),
            seek.index_name,
            RELATIONSHIP_OWNER_KIND,
            type_id,
            after,
            sql_limit
        ],
        |row| row.get(0),
    )?;
    let items = rows.collect::<Result<Vec<i64>, _>>()?;
    let next_after = (items.len() == limit)
        .then(|| items.last().copied())
        .flatten();
    Ok(ScanPage { items, next_after })
}

fn scan_property_index_after(
    snapshot: &Snapshot<'_>,
    seek: &StandardIndexSeek,
    owner_kind: i64,
    after: i64,
    limit: usize,
) -> QueryResult<ScanPage<i64>> {
    ensure_planned_index_cache(snapshot, seek)?;

    if seek.kind == StandardIndexKind::Range
        && seek.predicates.len() == 1
        && let (ordinal, StandardIndexPredicate::Bounds { lower, upper }) = &seek.predicates[0]
        && let Some(page) = scan_range_bounds_page(
            snapshot,
            &seek.index_name,
            owner_kind,
            *ordinal,
            after,
            limit,
            lower,
            upper,
        )?
    {
        return Ok(page);
    }

    let mut matched: Option<BTreeSet<i64>> = None;
    for (ordinal, predicate) in &seek.predicates {
        let candidates = scan_predicate_candidates(
            snapshot,
            &seek.index_name,
            owner_kind,
            *ordinal,
            seek.kind,
            predicate,
            after,
        )?;
        matched = Some(match matched {
            None => candidates,
            Some(previous) => previous.intersection(&candidates).copied().collect(),
        });
        if matched.as_ref().is_some_and(BTreeSet::is_empty) {
            break;
        }
    }

    let items = matched
        .unwrap_or_default()
        .into_iter()
        .take(limit)
        .collect::<Vec<_>>();
    let next_after = (items.len() == limit)
        .then(|| items.last().copied())
        .flatten();
    Ok(ScanPage { items, next_after })
}

fn scan_predicate_candidates(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    kind: StandardIndexKind,
    predicate: &StandardIndexPredicate,
    after: i64,
) -> QueryResult<BTreeSet<i64>> {
    if kind == StandardIndexKind::Range
        && let Some(candidates) = scan_range_predicate_candidates(
            snapshot, index_name, owner_kind, ordinal, predicate, after,
        )?
    {
        return Ok(candidates);
    }
    match predicate {
        StandardIndexPredicate::Lookup => Err(QueryError::internal(
            "lookup predicate reached property Index cache",
        )),
        StandardIndexPredicate::Equal(value) => {
            scan_exact_candidates(snapshot, index_name, owner_kind, ordinal, value, after)
        }
        StandardIndexPredicate::In(values) => {
            let mut result = BTreeSet::new();
            for value in values {
                result.extend(scan_exact_candidates(
                    snapshot, index_name, owner_kind, ordinal, value, after,
                )?);
            }
            Ok(result)
        }
        StandardIndexPredicate::StartsWith(value) => scan_text_candidates(
            snapshot,
            index_name,
            owner_kind,
            ordinal,
            after,
            "substr(text_value, 1, length(?6)) = ?6",
            value,
        ),
        StandardIndexPredicate::EndsWith(value) => scan_text_candidates(
            snapshot,
            index_name,
            owner_kind,
            ordinal,
            after,
            "substr(text_value, -length(?6)) = ?6",
            value,
        ),
        StandardIndexPredicate::Contains(value) => scan_text_candidates(
            snapshot,
            index_name,
            owner_kind,
            ordinal,
            after,
            "instr(text_value, ?6) > 0",
            value,
        ),
        StandardIndexPredicate::IsNotNull => {
            scan_all_property_candidates(snapshot, index_name, owner_kind, ordinal, after)
        }
        StandardIndexPredicate::WithinBBox { .. } if kind == StandardIndexKind::Point => {
            scan_point_bbox_candidates(snapshot, index_name, owner_kind, ordinal, after, predicate)
        }
        StandardIndexPredicate::Distance { .. } if kind == StandardIndexKind::Point => {
            scan_point_distance_candidates(
                snapshot, index_name, owner_kind, ordinal, after, predicate,
            )
        }
        StandardIndexPredicate::Less(_, _)
        | StandardIndexPredicate::Greater(_, _)
        | StandardIndexPredicate::WithinBBox { .. }
        | StandardIndexPredicate::Distance { .. } => {
            scan_filtered_candidates(snapshot, index_name, owner_kind, ordinal, predicate, after)
        }
        StandardIndexPredicate::Bounds { .. } => Err(QueryError::internal(
            "bounded Range predicate requires the single-property Range seek path",
        )),
    }
}

fn scan_range_predicate_candidates(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    predicate: &StandardIndexPredicate,
    after: i64,
) -> QueryResult<Option<BTreeSet<i64>>> {
    let candidates = match predicate {
        StandardIndexPredicate::Equal(value) => {
            scan_exact_candidates(snapshot, index_name, owner_kind, ordinal, value, after)?
        }
        StandardIndexPredicate::In(values) => {
            let mut result = BTreeSet::new();
            for value in values {
                result.extend(scan_exact_candidates(
                    snapshot, index_name, owner_kind, ordinal, value, after,
                )?);
            }
            result
        }
        StandardIndexPredicate::StartsWith(value) => {
            scan_range_prefix_candidates(snapshot, index_name, owner_kind, ordinal, after, value)?
        }
        StandardIndexPredicate::Less(value, inclusive)
        | StandardIndexPredicate::Greater(value, inclusive) => {
            let Some(key) = range_order_key(value) else {
                return Ok(Some(BTreeSet::new()));
            };
            let operator = match predicate {
                StandardIndexPredicate::Less(_, false) => "<",
                StandardIndexPredicate::Less(_, true) => "<=",
                StandardIndexPredicate::Greater(_, false) => ">",
                StandardIndexPredicate::Greater(_, true) => ">=",
                _ => unreachable!(),
            };
            let _ = inclusive;
            scan_range_order_candidates(
                snapshot, index_name, owner_kind, ordinal, after, operator, &key,
            )?
        }
        _ => return Ok(None),
    };
    Ok(Some(candidates))
}

#[allow(
    clippy::too_many_arguments,
    reason = "the bounded Range cache helper keeps the complete indexed bound/page contract explicit"
)]
fn scan_range_bounds_page(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    after: i64,
    limit: usize,
    lower: &Option<(Value, bool)>,
    upper: &Option<(Value, bool)>,
) -> QueryResult<Option<ScanPage<i64>>> {
    let lower = match lower {
        Some((value, inclusive)) => match numeric_range_value(value) {
            Some(value) => Some((value, *inclusive)),
            None => return Ok(None),
        },
        None => None,
    };
    let upper = match upper {
        Some((value, inclusive)) => match numeric_range_value(value) {
            Some(value) => Some((value, *inclusive)),
            None => return Ok(None),
        },
        None => None,
    };
    if lower.is_none() && upper.is_none() {
        return Ok(None);
    }

    let mut sql = "SELECT owner_id FROM temp._lithograph_standard_index_cache \
         WHERE snapshot_hash = ? AND index_name = ? AND owner_kind = ? \
         AND property_ordinal = ? AND owner_id > ? AND sort_family = ?"
        .to_owned();
    let mut parameters = vec![
        SqlValue::Blob(snapshot.cache_identity().as_bytes().to_vec()),
        SqlValue::Text(index_name.to_owned()),
        SqlValue::Integer(owner_kind),
        SqlValue::Integer(i64::try_from(ordinal).unwrap_or(i64::MAX)),
        SqlValue::Integer(after),
        SqlValue::Integer(RANGE_FAMILY_NUMBER),
    ];
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

fn numeric_range_value(value: &Value) -> Option<SqlValue> {
    match value {
        Value::Integer(value) => Some(SqlValue::Integer(*value)),
        Value::Float(value) if !value.is_nan() => Some(SqlValue::Real(*value)),
        _ => None,
    }
}

fn scan_owner_id_candidates(
    snapshot: &Snapshot<'_>,
    sql: &str,
    parameters: &[&dyn rusqlite::ToSql],
) -> QueryResult<BTreeSet<i64>> {
    let mut statement = snapshot.connection_for_query().prepare(sql)?;
    let rows = statement.query_map(parameters, |row| row.get(0))?;
    rows.collect::<Result<BTreeSet<i64>, _>>()
        .map_err(Into::into)
}

fn scan_scalar_range_candidates(
    base: (&Snapshot<'_>, &str, i64, i64, i64),
    family: i64,
    column: &str,
    operator: &str,
    value: &dyn rusqlite::ToSql,
) -> QueryResult<BTreeSet<i64>> {
    let (snapshot, index_name, owner_kind, ordinal, after) = base;
    let sql = format!(
        "SELECT owner_id FROM temp._lithograph_standard_index_cache \
         WHERE snapshot_hash = ?1 AND index_name = ?2 AND owner_kind = ?3 \
         AND property_ordinal = ?4 AND owner_id > ?5 AND sort_family = ?6 \
         AND {column} {operator} ?7 ORDER BY owner_id"
    );
    scan_owner_id_candidates(
        snapshot,
        &sql,
        params![
            snapshot.cache_identity().as_bytes().as_slice(),
            index_name,
            owner_kind,
            ordinal,
            after,
            family,
            value,
        ],
    )
}

fn scan_range_order_candidates(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    after: i64,
    operator: &str,
    key: &RangeOrderKey,
) -> QueryResult<BTreeSet<i64>> {
    let ordinal = i64::try_from(ordinal).unwrap_or(i64::MAX);
    match key {
        RangeOrderKey::Number(number) => scan_scalar_range_candidates(
            (snapshot, index_name, owner_kind, ordinal, after),
            RANGE_FAMILY_NUMBER,
            "sort_number",
            operator,
            number,
        ),
        RangeOrderKey::Text(text) => scan_scalar_range_candidates(
            (snapshot, index_name, owner_kind, ordinal, after),
            RANGE_FAMILY_STRING,
            "sort_text",
            operator,
            text,
        ),
        RangeOrderKey::Tuple {
            family,
            a,
            b,
            c,
            text,
        } => {
            let sql = format!(
                "SELECT owner_id FROM temp._lithograph_standard_index_cache \
                 WHERE snapshot_hash = ?1 AND index_name = ?2 AND owner_kind = ?3 \
                 AND property_ordinal = ?4 AND owner_id > ?5 AND sort_family = ?6 \
                 AND (sort_a, sort_b, sort_c, sort_text) {operator} (?7, ?8, ?9, ?10) \
                 ORDER BY owner_id"
            );
            scan_owner_id_candidates(
                snapshot,
                &sql,
                params![
                    snapshot.cache_identity().as_bytes().as_slice(),
                    index_name,
                    owner_kind,
                    ordinal,
                    after,
                    family,
                    a,
                    b,
                    c,
                    text,
                ],
            )
        }
    }
}

fn scan_range_prefix_candidates(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    after: i64,
    prefix: &str,
) -> QueryResult<BTreeSet<i64>> {
    let upper = prefix_upper_bound(prefix);
    scan_owner_id_candidates(
        snapshot,
        "SELECT owner_id FROM temp._lithograph_standard_index_cache \
         WHERE snapshot_hash = ?1 AND index_name = ?2 AND owner_kind = ?3 \
         AND property_ordinal = ?4 AND owner_id > ?5 AND sort_family = ?6 \
         AND sort_text >= ?7 AND (?8 IS NULL OR sort_text < ?8) \
         AND substr(sort_text, 1, length(?7)) = ?7 ORDER BY owner_id",
        params![
            snapshot.cache_identity().as_bytes().as_slice(),
            index_name,
            owner_kind,
            i64::try_from(ordinal).unwrap_or(i64::MAX),
            after,
            RANGE_FAMILY_STRING,
            prefix,
            upper,
        ],
    )
}

fn prefix_upper_bound(prefix: &str) -> Option<String> {
    let mut characters = prefix.chars().collect::<Vec<_>>();
    for index in (0..characters.len()).rev() {
        let mut scalar = u32::from(characters[index]);
        while scalar < 0x10_FFFF {
            scalar = scalar.checked_add(1)?;
            if let Some(next) = char::from_u32(scalar) {
                characters[index] = next;
                characters.truncate(index + 1);
                return Some(characters.into_iter().collect());
            }
        }
    }
    None
}

fn scan_all_property_candidates(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    after: i64,
) -> QueryResult<BTreeSet<i64>> {
    scan_owner_id_candidates(
        snapshot,
        "SELECT owner_id FROM temp._lithograph_standard_index_cache \
         WHERE snapshot_hash = ?1 AND index_name = ?2 AND owner_kind = ?3 \
         AND property_ordinal = ?4 AND owner_id > ?5 ORDER BY owner_id",
        params![
            snapshot.cache_identity().as_bytes().as_slice(),
            index_name,
            owner_kind,
            i64::try_from(ordinal).unwrap_or(i64::MAX),
            after,
        ],
    )
}

fn scan_exact_candidates(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    value: &Value,
    after: i64,
) -> QueryResult<BTreeSet<i64>> {
    let Some(property) = property_from_value(value.clone())? else {
        return Ok(BTreeSet::new());
    };
    let Some(key) = property_equality_key(&property)? else {
        return Ok(BTreeSet::new());
    };
    scan_owner_id_candidates(
        snapshot,
        "SELECT owner_id FROM temp._lithograph_standard_index_cache \
         WHERE snapshot_hash = ?1 AND index_name = ?2 AND owner_kind = ?3 \
         AND property_ordinal = ?4 AND owner_id > ?5 AND equality_blob = ?6 ORDER BY owner_id",
        params![
            snapshot.cache_identity().as_bytes().as_slice(),
            index_name,
            owner_kind,
            i64::try_from(ordinal).unwrap_or(i64::MAX),
            after,
            key,
        ],
    )
}

fn scan_text_candidates(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    after: i64,
    predicate_sql: &str,
    value: &str,
) -> QueryResult<BTreeSet<i64>> {
    let sql = format!(
        "SELECT owner_id FROM temp._lithograph_standard_index_cache \
         WHERE snapshot_hash = ?1 AND index_name = ?2 AND owner_kind = ?3 \
         AND property_ordinal = ?4 AND owner_id > ?5 AND text_value IS NOT NULL \
         AND {predicate_sql} ORDER BY owner_id"
    );
    scan_owner_id_candidates(
        snapshot,
        &sql,
        params![
            snapshot.cache_identity().as_bytes().as_slice(),
            index_name,
            owner_kind,
            i64::try_from(ordinal).unwrap_or(i64::MAX),
            after,
            value,
        ],
    )
}

fn scan_point_bbox_candidates(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    after: i64,
    predicate: &StandardIndexPredicate,
) -> QueryResult<BTreeSet<i64>> {
    let StandardIndexPredicate::WithinBBox { lower, upper } = predicate else {
        return Err(QueryError::internal(
            "point bbox scanner received a non-bbox predicate",
        ));
    };
    let (Value::Point(lower), Value::Point(upper)) = (lower, upper) else {
        return Ok(BTreeSet::new());
    };
    if lower.srid() != upper.srid() || lower.coordinates().len() != upper.coordinates().len() {
        return Ok(BTreeSet::new());
    }
    let geographic = lower.crs().starts_with("wgs-84");
    let longitude_wrap = geographic && lower.coordinates()[0] > upper.coordinates()[0];
    let x_predicate = if longitude_wrap {
        "(point_x >= ?7 OR point_x <= ?8)"
    } else {
        "point_x >= ?7 AND point_x <= ?8"
    };
    let sql = format!(
        "SELECT owner_id, value_blob FROM temp._lithograph_standard_index_cache \
         WHERE snapshot_hash = ?1 AND index_name = ?2 AND owner_kind = ?3 \
         AND property_ordinal = ?4 AND owner_id > ?5 AND point_crs = ?6 \
         AND {x_predicate} AND point_y >= ?9 AND point_y <= ?10 \
         AND (?11 IS NULL OR (point_z >= ?11 AND point_z <= ?12)) \
         ORDER BY owner_id"
    );
    let z_lower = lower.coordinates().get(2).copied();
    let z_upper = upper.coordinates().get(2).copied();
    scan_value_predicate_candidates(
        snapshot,
        &sql,
        params![
            snapshot.cache_identity().as_bytes().as_slice(),
            index_name,
            owner_kind,
            i64::try_from(ordinal).unwrap_or(i64::MAX),
            after,
            i64::from(lower.srid()),
            lower.coordinates()[0],
            upper.coordinates()[0],
            lower.coordinates()[1],
            upper.coordinates()[1],
            z_lower,
            z_upper,
        ],
        predicate,
    )
}

fn scan_point_distance_candidates(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    after: i64,
    predicate: &StandardIndexPredicate,
) -> QueryResult<BTreeSet<i64>> {
    let StandardIndexPredicate::Distance { center, radius, .. } = predicate else {
        return Err(QueryError::internal(
            "point distance scanner received a non-distance predicate",
        ));
    };
    let Value::Point(center) = center else {
        return Ok(BTreeSet::new());
    };
    let radius = match radius {
        Value::Integer(value) => *value as f64,
        Value::Float(value) => *value,
        _ => return Ok(BTreeSet::new()),
    };
    if !radius.is_finite() || radius < 0.0 {
        return Ok(BTreeSet::new());
    }
    let coordinates = center.coordinates();
    let z_lower = coordinates.get(2).map(|value| *value - radius);
    let z_upper = coordinates.get(2).map(|value| *value + radius);
    let mut parameters = vec![
        SqlValue::Blob(snapshot.cache_identity().as_bytes().to_vec()),
        SqlValue::Text(index_name.to_owned()),
        SqlValue::Integer(owner_kind),
        SqlValue::Integer(i64::try_from(ordinal).unwrap_or(i64::MAX)),
        SqlValue::Integer(after),
        SqlValue::Integer(i64::from(center.srid())),
    ];
    let sql = if center.crs().starts_with("wgs-84") {
        const EARTH_RADIUS_METERS: f64 = 6_378_140.0;
        let minimum_surface_radius = if coordinates.len() == 3 {
            EARTH_RADIUS_METERS + coordinates[2] - radius / 2.0
        } else {
            EARTH_RADIUS_METERS
        };
        let latitude_delta = if minimum_surface_radius <= 0.0 {
            180.0
        } else {
            (radius / minimum_surface_radius).to_degrees().min(180.0)
        };
        parameters.extend([
            SqlValue::Real((coordinates[1] - latitude_delta).max(-90.0)),
            SqlValue::Real((coordinates[1] + latitude_delta).min(90.0)),
            optional_real(z_lower),
            optional_real(z_upper),
        ]);
        "SELECT owner_id, value_blob FROM temp._lithograph_standard_index_cache \
         WHERE snapshot_hash = ?1 AND index_name = ?2 AND owner_kind = ?3 \
         AND property_ordinal = ?4 AND owner_id > ?5 AND point_crs = ?6 \
         AND point_y >= ?7 AND point_y <= ?8 \
         AND (?9 IS NULL OR (point_z >= ?9 AND point_z <= ?10)) ORDER BY owner_id"
    } else {
        parameters.extend([
            SqlValue::Real(coordinates[0] - radius),
            SqlValue::Real(coordinates[0] + radius),
            SqlValue::Real(coordinates[1] - radius),
            SqlValue::Real(coordinates[1] + radius),
            optional_real(z_lower),
            optional_real(z_upper),
        ]);
        "SELECT owner_id, value_blob FROM temp._lithograph_standard_index_cache \
         WHERE snapshot_hash = ?1 AND index_name = ?2 AND owner_kind = ?3 \
         AND property_ordinal = ?4 AND owner_id > ?5 AND point_crs = ?6 \
         AND point_x >= ?7 AND point_x <= ?8 AND point_y >= ?9 AND point_y <= ?10 \
         AND (?11 IS NULL OR (point_z >= ?11 AND point_z <= ?12)) ORDER BY owner_id"
    };
    scan_value_predicate_candidates(
        snapshot,
        sql,
        rusqlite::params_from_iter(parameters.iter()),
        predicate,
    )
}

fn optional_real(value: Option<f64>) -> SqlValue {
    value.map_or(SqlValue::Null, SqlValue::Real)
}

fn scan_value_predicate_candidates(
    snapshot: &Snapshot<'_>,
    sql: &str,
    parameters: impl rusqlite::Params,
    predicate: &StandardIndexPredicate,
) -> QueryResult<BTreeSet<i64>> {
    let mut statement = snapshot.connection_for_query().prepare(sql)?;
    let rows = statement.query_map(parameters, |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut result = BTreeSet::new();
    for row in rows {
        let (owner_id, bytes) = row?;
        let value = storage::PropertyValue::from_canonical_bytes(&bytes)?;
        let value = super::super::super::graph::property_value(value)?;
        if predicate_matches(&value, predicate)? {
            result.insert(owner_id);
        }
    }
    Ok(result)
}

fn scan_filtered_candidates(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    predicate: &StandardIndexPredicate,
    after: i64,
) -> QueryResult<BTreeSet<i64>> {
    let sql = "SELECT owner_id, value_blob FROM temp._lithograph_standard_index_cache \
         WHERE snapshot_hash = ?1 AND index_name = ?2 AND owner_kind = ?3 \
         AND property_ordinal = ?4 AND owner_id > ?5 ORDER BY owner_id";
    scan_value_predicate_candidates(
        snapshot,
        sql,
        params![
            snapshot.cache_identity().as_bytes().as_slice(),
            index_name,
            owner_kind,
            i64::try_from(ordinal).unwrap_or(i64::MAX),
            after,
        ],
        predicate,
    )
}

fn predicate_matches(value: &Value, predicate: &StandardIndexPredicate) -> QueryResult<bool> {
    match predicate {
        StandardIndexPredicate::Lookup => Ok(true),
        StandardIndexPredicate::Equal(_) | StandardIndexPredicate::In(_) => {
            equality_predicate_matches(value, predicate)
        }
        StandardIndexPredicate::Less(_, _) | StandardIndexPredicate::Greater(_, _) => {
            order_predicate_matches(value, predicate)
        }
        StandardIndexPredicate::Bounds { lower, upper } => {
            let lower_matches = match lower {
                Some((expected, inclusive)) => order_predicate_matches(
                    value,
                    &StandardIndexPredicate::Greater(expected.clone(), *inclusive),
                )?,
                None => true,
            };
            let upper_matches = match upper {
                Some((expected, inclusive)) => order_predicate_matches(
                    value,
                    &StandardIndexPredicate::Less(expected.clone(), *inclusive),
                )?,
                None => true,
            };
            Ok(lower_matches && upper_matches)
        }
        StandardIndexPredicate::IsNotNull => Ok(!matches!(value, Value::Null)),
        StandardIndexPredicate::StartsWith(_)
        | StandardIndexPredicate::EndsWith(_)
        | StandardIndexPredicate::Contains(_) => text_predicate_matches(value, predicate),
        StandardIndexPredicate::WithinBBox { .. } | StandardIndexPredicate::Distance { .. } => {
            spatial_predicate_matches(value, predicate)
        }
    }
}

fn equality_predicate_matches(
    value: &Value,
    predicate: &StandardIndexPredicate,
) -> QueryResult<bool> {
    match predicate {
        StandardIndexPredicate::Equal(expected) => {
            Ok(cypher_equals(value, expected)?.unwrap_or(false))
        }
        StandardIndexPredicate::In(expected) => {
            for candidate in expected {
                if cypher_equals(value, candidate)? == Some(true) {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => Err(QueryError::internal(
            "equality predicate helper received a non-equality predicate",
        )),
    }
}

fn order_predicate_matches(value: &Value, predicate: &StandardIndexPredicate) -> QueryResult<bool> {
    let (expected, desired, inclusive) = match predicate {
        StandardIndexPredicate::Less(expected, inclusive) => (expected, Ordering::Less, *inclusive),
        StandardIndexPredicate::Greater(expected, inclusive) => {
            (expected, Ordering::Greater, *inclusive)
        }
        _ => {
            return Err(QueryError::internal(
                "order predicate helper received a non-order predicate",
            ));
        }
    };
    ordering_matches(cypher_compare(value, expected)?, desired, inclusive)
}

fn text_predicate_matches(value: &Value, predicate: &StandardIndexPredicate) -> QueryResult<bool> {
    let Value::String(actual) = value else {
        return Ok(false);
    };
    Ok(match predicate {
        StandardIndexPredicate::StartsWith(expected) => actual.starts_with(expected),
        StandardIndexPredicate::EndsWith(expected) => actual.ends_with(expected),
        StandardIndexPredicate::Contains(expected) => actual.contains(expected),
        _ => {
            return Err(QueryError::internal(
                "text predicate helper received a non-text predicate",
            ));
        }
    })
}

fn spatial_predicate_matches(
    value: &Value,
    predicate: &StandardIndexPredicate,
) -> QueryResult<bool> {
    match predicate {
        StandardIndexPredicate::WithinBBox { lower, upper } => {
            let result = super::super::super::functions::evaluate(
                "point.withinbbox",
                &[value.clone(), lower.clone(), upper.clone()],
            )
            .ok_or_else(|| {
                QueryError::internal("point.withinBBox is missing from the function registry")
            })??;
            Ok(matches!(result, Value::Boolean(true)))
        }
        StandardIndexPredicate::Distance {
            center,
            radius,
            inclusive,
        } => {
            let distance = super::super::super::functions::evaluate(
                "point.distance",
                &[value.clone(), center.clone()],
            )
            .ok_or_else(|| {
                QueryError::internal("point.distance is missing from the function registry")
            })??;
            ordering_matches(
                cypher_compare(&distance, radius)?,
                Ordering::Less,
                *inclusive,
            )
        }
        _ => Err(QueryError::internal(
            "spatial predicate helper received a non-spatial predicate",
        )),
    }
}

fn ordering_matches(
    comparison: Option<CypherComparison>,
    desired: Ordering,
    inclusive: bool,
) -> QueryResult<bool> {
    Ok(match comparison {
        Some(CypherComparison::Less) => desired == Ordering::Less,
        Some(CypherComparison::Equal) => inclusive,
        Some(CypherComparison::Greater) => desired == Ordering::Greater,
        Some(CypherComparison::Unordered) | None => false,
    })
}

pub(crate) fn ensure_standard_indexes_for_commit(
    connection: &Connection,
    commit: HashId,
    previous: &SchemaState,
    schema: &SchemaState,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let snapshot = Snapshot::resolve(connection, commit)?;
    for (name, index) in &schema.indexes {
        if previous.indexes.get(name) == Some(index) {
            continue;
        }
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        if !matches!(
            index.kind,
            StandardIndexKind::FullText | StandardIndexKind::Vector
        ) && !matches!(index.target, IndexTarget::NodeLookup)
        {
            ensure_index_cache(&snapshot, index)?;
        }
    }
    Ok(())
}

fn ensure_index_cache(snapshot: &Snapshot<'_>, index: &IndexDefinition) -> QueryResult<()> {
    let connection = snapshot.connection_for_query();
    ensure_cache_tables(connection)?;
    let exists: i64 = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM temp._lithograph_standard_index_cache_meta \
         WHERE snapshot_hash = ?1 AND index_name = ?2 AND complete = 1)",
        params![snapshot.cache_identity().as_bytes().as_slice(), index.name],
        |row| row.get(0),
    )?;
    if exists == 1 {
        return Ok(());
    }
    connection.execute(
        "DELETE FROM temp._lithograph_standard_index_cache \
         WHERE snapshot_hash = ?1 AND index_name = ?2",
        params![snapshot.cache_identity().as_bytes().as_slice(), index.name],
    )?;
    build_index_cache(snapshot, index)?;
    connection.execute(
        "INSERT OR REPLACE INTO temp._lithograph_standard_index_cache_meta \
         (snapshot_hash, index_name, complete) VALUES(?1, ?2, 1)",
        params![snapshot.cache_identity().as_bytes().as_slice(), index.name],
    )?;
    Ok(())
}

fn ensure_cache_tables(connection: &Connection) -> QueryResult<()> {
    connection.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS _lithograph_standard_index_cache_meta(\
             snapshot_hash BLOB NOT NULL, index_name TEXT NOT NULL, complete INTEGER NOT NULL,\
             PRIMARY KEY(snapshot_hash, index_name)) WITHOUT ROWID;\
         CREATE TEMP TABLE IF NOT EXISTS _lithograph_standard_index_cache(\
             snapshot_hash BLOB NOT NULL, index_name TEXT NOT NULL, owner_kind INTEGER NOT NULL,\
             owner_id INTEGER NOT NULL, property_ordinal INTEGER NOT NULL, token_id INTEGER,\
             value_blob BLOB, equality_blob BLOB, text_value TEXT, sort_family INTEGER, sort_number NUMERIC,\
             sort_a INTEGER, sort_b INTEGER, sort_c INTEGER, sort_text TEXT,\
             point_crs INTEGER, point_x REAL, point_y REAL, point_z REAL,\
             PRIMARY KEY(snapshot_hash, index_name, owner_kind, owner_id, property_ordinal)) WITHOUT ROWID;\
         CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_value \
             ON _lithograph_standard_index_cache(snapshot_hash, index_name, owner_kind, property_ordinal, value_blob, owner_id);\
         CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_equality \
             ON _lithograph_standard_index_cache(snapshot_hash, index_name, owner_kind, property_ordinal, equality_blob, owner_id);\
         CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_text \
             ON _lithograph_standard_index_cache(snapshot_hash, index_name, owner_kind, property_ordinal, text_value, owner_id);\
         CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_range_number \
             ON _lithograph_standard_index_cache(snapshot_hash, index_name, owner_kind, property_ordinal, sort_family, sort_number, owner_id);\
         CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_range_text \
             ON _lithograph_standard_index_cache(snapshot_hash, index_name, owner_kind, property_ordinal, sort_family, sort_text, owner_id);\
         CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_range_tuple \
             ON _lithograph_standard_index_cache(snapshot_hash, index_name, owner_kind, property_ordinal, sort_family, sort_a, sort_b, sort_c, sort_text, owner_id);\
         CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_point_x \
             ON _lithograph_standard_index_cache(snapshot_hash, index_name, owner_kind, property_ordinal, point_crs, point_x, point_y, point_z, owner_id);\
         CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_point_y \
             ON _lithograph_standard_index_cache(snapshot_hash, index_name, owner_kind, property_ordinal, point_crs, point_y, point_x, point_z, owner_id);\
         CREATE INDEX IF NOT EXISTS temp._lithograph_standard_index_cache_token \
             ON _lithograph_standard_index_cache(snapshot_hash, index_name, owner_kind, token_id, owner_id);",
    )?;
    Ok(())
}

fn build_index_cache(snapshot: &Snapshot<'_>, index: &IndexDefinition) -> QueryResult<()> {
    match &index.target {
        IndexTarget::NodeLookup => Ok(()),
        IndexTarget::RelationshipLookup => build_relationship_lookup_cache(snapshot, index),
        IndexTarget::NodeProperties { label, properties } => {
            build_node_property_cache(snapshot, index, label, properties)
        }
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => build_relationship_property_cache(snapshot, index, relationship_type, properties),
    }
}

fn build_relationship_lookup_cache(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
) -> QueryResult<()> {
    let mut after = 0_i64;
    loop {
        let page = snapshot.scan_relationships_after(after, INDEX_BUILD_PAGE_SIZE)?;
        for relationship in page.items {
            snapshot.connection_for_query().execute(
            "INSERT OR REPLACE INTO temp._lithograph_standard_index_cache\
             (snapshot_hash, index_name, owner_kind, owner_id, property_ordinal, token_id, value_blob, text_value)\
             VALUES(?1, ?2, ?3, ?4, 0, ?5, NULL, NULL)",
            params![
                snapshot.cache_identity().as_bytes().as_slice(),
                index.name,
                RELATIONSHIP_OWNER_KIND,
                relationship.id,
                relationship.type_id,
            ],
            )?;
        }
        let Some(next_after) = page.next_after else {
            break;
        };
        after = next_after;
    }
    Ok(())
}

fn build_node_property_cache(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    label: &str,
    properties: &[String],
) -> QueryResult<()> {
    let Some(label_id) = storage::find_label(snapshot.connection_for_query(), label)? else {
        return Ok(());
    };
    let Some(keys) = property_key_ids(snapshot, properties)? else {
        return Ok(());
    };
    let mut after = 0_i64;
    loop {
        let page = snapshot.scan_label_after(label_id, after, INDEX_BUILD_PAGE_SIZE)?;
        for node in page.items {
            insert_owner_values(snapshot, index, NODE_OWNER_KIND, node, &keys)?;
        }
        let Some(next_after) = page.next_after else {
            break;
        };
        after = next_after;
    }
    Ok(())
}

fn build_relationship_property_cache(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    relationship_type: &str,
    properties: &[String],
) -> QueryResult<()> {
    let Some(type_id) =
        storage::find_relationship_type(snapshot.connection_for_query(), relationship_type)?
    else {
        return Ok(());
    };
    let Some(keys) = property_key_ids(snapshot, properties)? else {
        return Ok(());
    };
    let mut after = 0_i64;
    loop {
        let page = snapshot.scan_relationships_after(after, INDEX_BUILD_PAGE_SIZE)?;
        for relationship in page.items {
            if relationship.type_id == type_id {
                insert_owner_values(
                    snapshot,
                    index,
                    RELATIONSHIP_OWNER_KIND,
                    relationship.id,
                    &keys,
                )?;
            }
        }
        let Some(next_after) = page.next_after else {
            break;
        };
        after = next_after;
    }
    Ok(())
}

fn property_key_ids(
    snapshot: &Snapshot<'_>,
    properties: &[String],
) -> QueryResult<Option<Vec<i64>>> {
    let mut keys = Vec::with_capacity(properties.len());
    for property in properties {
        let Some(key) = storage::find_property_key(snapshot.connection_for_query(), property)?
        else {
            return Ok(None);
        };
        keys.push(key);
    }
    Ok(Some(keys))
}

fn insert_owner_values(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    owner_kind: i64,
    owner_id: i64,
    keys: &[i64],
) -> QueryResult<()> {
    let storage_owner = if owner_kind == NODE_OWNER_KIND {
        OwnerKind::Node
    } else {
        OwnerKind::Relationship
    };
    let mut values = Vec::with_capacity(keys.len());
    for key in keys {
        let Some(value) = snapshot.property(storage_owner, owner_id, *key)? else {
            return Ok(());
        };
        if !cache_value_supported(index.kind, &value) {
            return Ok(());
        }
        values.push(value);
    }
    for (ordinal, value) in values.into_iter().enumerate() {
        insert_cache_value(snapshot, index, owner_kind, owner_id, ordinal, value)?;
    }
    Ok(())
}

fn insert_cache_value(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    owner_kind: i64,
    owner_id: i64,
    ordinal: usize,
    value: storage::PropertyValue,
) -> QueryResult<()> {
    let value_blob = value.canonical_bytes()?;
    let equality_blob = property_equality_key(&value)?;
    let text_value = match &value {
        storage::PropertyValue::String(value) => Some(value.as_str()),
        _ => None,
    };
    let order_key = if index.kind == StandardIndexKind::Range {
        Some(super::super::super::graph::property_value(value.clone())?)
            .as_ref()
            .and_then(range_order_key)
    } else {
        None
    };
    let (point_crs, point_x, point_y, point_z) = match &value {
        storage::PropertyValue::Point(point) => (
            Some(point.crs),
            point.coordinates.first().copied(),
            point.coordinates.get(1).copied(),
            point.coordinates.get(2).copied(),
        ),
        _ => (None, None, None, None),
    };
    let (sort_family, sort_number, sort_a, sort_b, sort_c, sort_text) = match order_key {
        Some(RangeOrderKey::Number(number)) => (
            Some(RANGE_FAMILY_NUMBER),
            Some(number),
            None,
            None,
            None,
            None,
        ),
        Some(RangeOrderKey::Text(text)) => (
            Some(RANGE_FAMILY_STRING),
            None,
            None,
            None,
            None,
            Some(text),
        ),
        Some(RangeOrderKey::Tuple {
            family,
            a,
            b,
            c,
            text,
        }) => (Some(family), None, Some(a), Some(b), Some(c), Some(text)),
        None => (None, None, None, None, None, None),
    };
    snapshot.connection_for_query().execute(
        "INSERT OR REPLACE INTO temp._lithograph_standard_index_cache\
         (snapshot_hash, index_name, owner_kind, owner_id, property_ordinal, token_id, value_blob, equality_blob, text_value,\
          sort_family, sort_number, sort_a, sort_b, sort_c, sort_text, point_crs, point_x, point_y, point_z)\
         VALUES(?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            snapshot.cache_identity().as_bytes().as_slice(),
            index.name,
            owner_kind,
            owner_id,
            i64::try_from(ordinal).unwrap_or(i64::MAX),
            value_blob,
            equality_blob,
            text_value,
            sort_family,
            sort_number,
            sort_a,
            sort_b,
            sort_c,
            sort_text,
            point_crs,
            point_x,
            point_y,
            point_z,
        ],
    )?;
    Ok(())
}

fn cache_value_supported(kind: StandardIndexKind, value: &storage::PropertyValue) -> bool {
    match kind {
        StandardIndexKind::Lookup | StandardIndexKind::FullText | StandardIndexKind::Vector => {
            false
        }
        StandardIndexKind::Text => matches!(value, storage::PropertyValue::String(_)),
        StandardIndexKind::Point => matches!(value, storage::PropertyValue::Point(_)),
        StandardIndexKind::Range => true,
    }
}
