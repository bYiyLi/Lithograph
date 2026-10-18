use crate::query::resources::{
    ExecutionControl, MemoryReservation, checked_add, checked_mul, map_entry_bytes,
};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BTreeSet;

use rusqlite::{params, types::Value as SqlValue};

use crate::cypher::{CypherComparison, Value, cypher_compare};
use crate::storage::{self, ScanPage, Snapshot, StandardIndexKind};

use super::super::super::mutation::property_from_value_controlled;
use super::super::super::{QueryError, QueryResult};
use super::{StandardIndexPredicate, StandardIndexSeek};

mod range;

use range::{numeric_range_bounds, scan_persistent_numeric_range_page, scan_range_bounds_page};

pub(super) const NODE_OWNER_KIND: i64 = 1;
pub(super) const RELATIONSHIP_OWNER_KIND: i64 = 2;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StandardIndexCursor {
    Owner(i64),
    NumericRange {
        generation_id: i64,
        sort_number: SqlValue,
        owner_id: i64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StandardIndexPage {
    pub(crate) items: Vec<i64>,
    pub(crate) next_cursor: Option<StandardIndexCursor>,
}

const RANGE_FAMILY_BOOLEAN: i64 = 1;
pub(super) const RANGE_FAMILY_NUMBER: i64 = 2;
pub(super) const RANGE_FAMILY_STRING: i64 = 3;
const RANGE_FAMILY_DATE: i64 = 4;
const RANGE_FAMILY_LOCAL_TIME: i64 = 5;
const RANGE_FAMILY_TIME: i64 = 6;
const RANGE_FAMILY_LOCAL_DATETIME: i64 = 7;
const RANGE_FAMILY_ZONED_DATETIME: i64 = 8;
#[derive(Debug, Clone)]
pub(super) enum RangeOrderKey {
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

pub(super) fn range_order_key(value: &Value) -> Option<RangeOrderKey> {
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

struct IndexScan<'a, 'connection, 'control> {
    snapshot: &'a Snapshot<'connection>,
    control: &'a ExecutionControl<'control>,
    memory: RefCell<MemoryReservation>,
}

impl<'a, 'connection, 'control> IndexScan<'a, 'connection, 'control> {
    fn new(
        snapshot: &'a Snapshot<'connection>,
        seek: &StandardIndexSeek,
        control: &'a ExecutionControl<'control>,
    ) -> QueryResult<Self> {
        control.check()?;
        let mut bytes = checked_add(16_384, checked_mul(seek.index_name.len() as u64, 16)?)?;
        for (_, predicate) in &seek.predicates {
            bytes = checked_add(
                bytes,
                checked_mul(super::constant::predicate_bytes(predicate, control)?, 16)?,
            )?;
        }
        Ok(Self {
            snapshot,
            control,
            memory: RefCell::new(control.reserve(bytes)?),
        })
    }

    fn insert(&self, values: &mut BTreeSet<i64>, value: i64) -> QueryResult<()> {
        self.control.work(1)?;
        if !values.contains(&value) {
            self.memory.borrow_mut().grow(map_entry_bytes::<i64>()?)?;
            values.insert(value);
        }
        Ok(())
    }
}

impl<'connection> std::ops::Deref for IndexScan<'_, 'connection, '_> {
    type Target = Snapshot<'connection>;
    fn deref(&self) -> &Self::Target {
        self.snapshot
    }
}

pub(crate) fn scan_node_index_page(
    snapshot: &Snapshot<'_>,
    seek: &StandardIndexSeek,
    cursor: Option<&StandardIndexCursor>,
    limit: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<StandardIndexPage> {
    let scan = IndexScan::new(snapshot, seek, control)?;
    let snapshot = &scan;
    if seek.kind == StandardIndexKind::Lookup {
        return Err(QueryError::internal(
            "node lookup Index seeks must use the canonical label access path",
        ));
    }
    scan_property_index_page(snapshot, seek, NODE_OWNER_KIND, cursor, limit)
}

pub(crate) fn scan_relationship_index_page(
    snapshot: &Snapshot<'_>,
    seek: &StandardIndexSeek,
    relationship_type_id: Option<i64>,
    cursor: Option<&StandardIndexCursor>,
    limit: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<StandardIndexPage> {
    let scan = IndexScan::new(snapshot, seek, control)?;
    let snapshot = &scan;
    if seek.kind == StandardIndexKind::Lookup {
        let Some(type_id) = relationship_type_id else {
            return Ok(StandardIndexPage {
                items: Vec::new(),
                next_cursor: None,
            });
        };
        let page = scan_relationship_lookup_after(
            snapshot,
            seek,
            type_id,
            owner_cursor_after(cursor)?,
            limit,
        )?;
        return Ok(owner_scan_page(page));
    }
    scan_property_index_page(snapshot, seek, RELATIONSHIP_OWNER_KIND, cursor, limit)
}

fn scan_property_index_page(
    snapshot: &IndexScan<'_, '_, '_>,
    seek: &StandardIndexSeek,
    owner_kind: i64,
    cursor: Option<&StandardIndexCursor>,
    limit: usize,
) -> QueryResult<StandardIndexPage> {
    let index = ensure_planned_index_cache(snapshot, seek)?;
    if seek.kind == StandardIndexKind::Range
        && seek.predicates.len() == 1
        && let (ordinal, StandardIndexPredicate::Bounds { lower, upper }) = &seek.predicates[0]
        && let Some(bounds) = numeric_range_bounds(lower, upper)
    {
        if let Some(page) = scan_persistent_numeric_range_page(
            snapshot,
            &seek.index_name,
            owner_kind,
            *ordinal,
            cursor,
            limit,
            &bounds,
        )? {
            prefetch_index_properties(snapshot, &index, owner_kind, &page.items)?;
            return Ok(page);
        }
        if matches!(cursor, Some(StandardIndexCursor::NumericRange { .. })) {
            return Err(QueryError::internal(
                "persistent Range Index generation changed during paginated scan",
            ));
        }
    }
    let page = scan_property_index_after(
        snapshot,
        seek,
        owner_kind,
        owner_cursor_after(cursor)?,
        limit,
    )?;
    Ok(owner_scan_page(page))
}

fn owner_cursor_after(cursor: Option<&StandardIndexCursor>) -> QueryResult<i64> {
    match cursor {
        None => Ok(0),
        Some(StandardIndexCursor::Owner(owner_id)) => Ok(*owner_id),
        Some(StandardIndexCursor::NumericRange { .. }) => Err(QueryError::internal(
            "numeric Range cursor reached an owner-ordered Index scan",
        )),
    }
}

fn owner_scan_page(page: ScanPage<i64>) -> StandardIndexPage {
    StandardIndexPage {
        items: page.items,
        next_cursor: page.next_after.map(StandardIndexCursor::Owner),
    }
}

fn ensure_planned_index_cache(
    snapshot: &IndexScan<'_, '_, '_>,
    seek: &StandardIndexSeek,
) -> QueryResult<storage::IndexDefinition> {
    let execution = crate::query::version::VersionExecution::new(
        snapshot.connection_for_query(),
        snapshot.control,
    );
    let loaded = if snapshot.query_schema_state().is_none() {
        Some(execution.load_schema(snapshot.commit())?)
    } else {
        None
    };
    let schema = snapshot
        .query_schema_state()
        .or_else(|| loaded.as_ref().map(|loaded| &loaded.schema))
        .ok_or_else(|| QueryError::internal("Index scan is missing Schema"))?;
    let index = schema.indexes.get(&seek.index_name).ok_or_else(|| {
        QueryError::internal(format!(
            "planned Index {} is not present in the active Schema",
            seek.index_name
        ))
    })?;
    snapshot
        .memory
        .borrow_mut()
        .grow(super::persistent::definition_workspace(
            index,
            snapshot.control,
        )?)?;
    ensure_index_cache(snapshot, index, snapshot.control)?;
    snapshot.control.check()?;
    Ok(index.clone())
}

fn scan_relationship_lookup_after(
    snapshot: &IndexScan<'_, '_, '_>,
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
    snapshot: &IndexScan<'_, '_, '_>,
    seek: &StandardIndexSeek,
    owner_kind: i64,
    after: i64,
    limit: usize,
) -> QueryResult<ScanPage<i64>> {
    let index = ensure_planned_index_cache(snapshot, seek)?;

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
        prefetch_index_properties(snapshot, &index, owner_kind, &page.items)?;
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
            Some(previous) => intersect_candidates(snapshot, &previous, &candidates)?,
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
    prefetch_index_properties(snapshot, &index, owner_kind, &items)?;
    Ok(ScanPage { items, next_after })
}

fn intersect_candidates(
    snapshot: &IndexScan<'_, '_, '_>,
    left: &BTreeSet<i64>,
    right: &BTreeSet<i64>,
) -> QueryResult<BTreeSet<i64>> {
    let mut output = BTreeSet::new();
    for owner in left.intersection(right) {
        snapshot.insert(&mut output, *owner)?;
    }
    Ok(output)
}

fn prefetch_index_properties(
    snapshot: &IndexScan<'_, '_, '_>,
    index: &storage::IndexDefinition,
    owner_kind: i64,
    owner_ids: &[i64],
) -> QueryResult<()> {
    let properties = match &index.target {
        storage::IndexTarget::NodeProperties { properties, .. }
        | storage::IndexTarget::RelationshipProperties { properties, .. } => properties,
        storage::IndexTarget::NodeLookup | storage::IndexTarget::RelationshipLookup => {
            return Ok(());
        }
    };
    let storage_owner = if owner_kind == NODE_OWNER_KIND {
        storage::OwnerKind::Node
    } else {
        storage::OwnerKind::Relationship
    };
    let mut key_ids = Vec::with_capacity(properties.len());
    for property in properties {
        let Some(key_id) = storage::find_property_key(snapshot.connection_for_query(), property)?
        else {
            return Ok(());
        };
        key_ids.push(key_id);
    }
    snapshot.prefetch_properties(storage_owner, owner_ids, &key_ids)?;
    Ok(())
}

fn scan_predicate_candidates(
    snapshot: &IndexScan<'_, '_, '_>,
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
                for owner in
                    scan_exact_candidates(snapshot, index_name, owner_kind, ordinal, value, after)?
                {
                    snapshot.insert(&mut result, owner)?;
                }
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
    snapshot: &IndexScan<'_, '_, '_>,
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
                for owner in
                    scan_exact_candidates(snapshot, index_name, owner_kind, ordinal, value, after)?
                {
                    snapshot.insert(&mut result, owner)?;
                }
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

fn index_scan_parameters(
    snapshot: &Snapshot<'_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    after: i64,
) -> Vec<SqlValue> {
    vec![
        SqlValue::Blob(snapshot.cache_identity().as_bytes().to_vec()),
        SqlValue::Text(index_name.to_owned()),
        SqlValue::Integer(owner_kind),
        SqlValue::Integer(i64::try_from(ordinal).unwrap_or(i64::MAX)),
        SqlValue::Integer(after),
    ]
}

fn numeric_range_value(value: &Value) -> Option<SqlValue> {
    match value {
        Value::Integer(value) => Some(SqlValue::Integer(*value)),
        Value::Float(value) if !value.is_nan() => Some(SqlValue::Real(*value)),
        _ => None,
    }
}

fn scan_owner_id_candidates(
    snapshot: &IndexScan<'_, '_, '_>,
    sql: &str,
    parameters: &[&dyn rusqlite::ToSql],
) -> QueryResult<BTreeSet<i64>> {
    let mut statement = snapshot.connection_for_query().prepare(sql)?;
    let mut rows = statement.query(parameters)?;
    let mut values = BTreeSet::new();
    while let Some(row) = rows.next()? {
        snapshot.insert(&mut values, row.get(0)?)?;
    }
    Ok(values)
}

fn scan_scalar_range_candidates(
    base: (&IndexScan<'_, '_, '_>, &str, i64, i64, i64),
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
    snapshot: &IndexScan<'_, '_, '_>,
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
    snapshot: &IndexScan<'_, '_, '_>,
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
    snapshot: &IndexScan<'_, '_, '_>,
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
    snapshot: &IndexScan<'_, '_, '_>,
    index_name: &str,
    owner_kind: i64,
    ordinal: usize,
    value: &Value,
    after: i64,
) -> QueryResult<BTreeSet<i64>> {
    let owned =
        crate::query::expression::clone_value_unreserved_controlled(value, Some(snapshot.control))?;
    let Some(property) = property_from_value_controlled(owned, snapshot.control)? else {
        return Ok(BTreeSet::new());
    };
    let Some(key) = super::persistent::equality_key(&property, snapshot.control)? else {
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
    snapshot: &IndexScan<'_, '_, '_>,
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
    snapshot: &IndexScan<'_, '_, '_>,
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
    snapshot: &IndexScan<'_, '_, '_>,
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
    let mut parameters = index_scan_parameters(snapshot, index_name, owner_kind, ordinal, after);
    parameters.push(SqlValue::Integer(i64::from(center.srid())));
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
    snapshot: &IndexScan<'_, '_, '_>,
    sql: &str,
    parameters: impl rusqlite::Params,
    predicate: &StandardIndexPredicate,
) -> QueryResult<BTreeSet<i64>> {
    let mut statement = snapshot.connection_for_query().prepare(sql)?;
    let mut rows = statement.query(parameters)?;
    let mut result = BTreeSet::new();
    while let Some(row) = rows.next()? {
        snapshot.control.work(1)?;
        let bytes = row.get_ref(1)?.as_blob().map_err(rusqlite::Error::from)?;
        if encoded_predicate_matches(snapshot, bytes, predicate)? {
            snapshot.insert(&mut result, row.get(0)?)?;
        }
        snapshot.control.check()?;
    }
    Ok(result)
}

fn encoded_predicate_matches(
    snapshot: &IndexScan<'_, '_, '_>,
    bytes: &[u8],
    predicate: &StandardIndexPredicate,
) -> QueryResult<bool> {
    // Tagged canonical records bound the number of decoded containers as well
    // as owned payload bytes. Keep their growth slack before decoding.
    let _decode = snapshot
        .control
        .reserve(checked_add(checked_mul(bytes.len() as u64, 256)?, 4096)?)?;
    let check = || {
        snapshot
            .control
            .check()
            .map_err(QueryError::into_storage_error)
    };
    let property = storage::PropertyValue::from_canonical_bytes_checked(bytes, &check)?;
    let (value, _value_memory) =
        crate::query::graph::borrowed_property_with_control(&property, snapshot.control)?;
    predicate_matches(&value, predicate, snapshot.control)
}

fn scan_filtered_candidates(
    snapshot: &IndexScan<'_, '_, '_>,
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

fn predicate_matches(
    value: &Value,
    predicate: &StandardIndexPredicate,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    match predicate {
        StandardIndexPredicate::Lookup => Ok(true),
        StandardIndexPredicate::Equal(_) | StandardIndexPredicate::In(_) => {
            equality_predicate_matches(value, predicate, control)
        }
        StandardIndexPredicate::Less(_, _) | StandardIndexPredicate::Greater(_, _) => {
            order_predicate_matches(value, predicate, control)
        }
        StandardIndexPredicate::Bounds { lower, upper } => {
            let lower_matches = match lower {
                Some((expected, inclusive)) => order_predicate_matches(
                    value,
                    &StandardIndexPredicate::Greater(
                        crate::query::expression::clone_value_unreserved_controlled(
                            expected,
                            Some(control),
                        )?,
                        *inclusive,
                    ),
                    control,
                )?,
                None => true,
            };
            let upper_matches = match upper {
                Some((expected, inclusive)) => order_predicate_matches(
                    value,
                    &StandardIndexPredicate::Less(
                        crate::query::expression::clone_value_unreserved_controlled(
                            expected,
                            Some(control),
                        )?,
                        *inclusive,
                    ),
                    control,
                )?,
                None => true,
            };
            Ok(lower_matches && upper_matches)
        }
        StandardIndexPredicate::IsNotNull => Ok(!matches!(value, Value::Null)),
        StandardIndexPredicate::StartsWith(_)
        | StandardIndexPredicate::EndsWith(_)
        | StandardIndexPredicate::Contains(_) => text_predicate_matches(value, predicate, control),
        StandardIndexPredicate::WithinBBox { .. } | StandardIndexPredicate::Distance { .. } => {
            spatial_predicate_matches(value, predicate)
        }
    }
}

fn equality_predicate_matches(
    value: &Value,
    predicate: &StandardIndexPredicate,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    match predicate {
        StandardIndexPredicate::Equal(expected) => Ok(
            crate::query::expression::cypher_equals_controlled(value, expected, Some(control))?
                .unwrap_or(false),
        ),
        StandardIndexPredicate::In(expected) => {
            for candidate in expected {
                if crate::query::expression::cypher_equals_controlled(
                    value,
                    candidate,
                    Some(control),
                )? == Some(true)
                {
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

fn order_predicate_matches(
    value: &Value,
    predicate: &StandardIndexPredicate,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
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
    ordering_matches(
        crate::query::expression::cypher_compare_controlled(value, expected, Some(control))?,
        desired,
        inclusive,
    )
}

fn text_predicate_matches(
    value: &Value,
    predicate: &StandardIndexPredicate,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let Value::String(actual) = value else {
        return Ok(false);
    };
    match predicate {
        StandardIndexPredicate::StartsWith(expected) => {
            text_edge_matches(actual, expected, false, control)
        }
        StandardIndexPredicate::EndsWith(expected) => {
            text_edge_matches(actual, expected, true, control)
        }
        StandardIndexPredicate::Contains(expected) => {
            crate::query::functions::find_text_controlled(actual, expected, 0, Some(control))
                .map(|found| found.is_some())
        }
        _ => Err(QueryError::internal(
            "text predicate helper received a non-text predicate",
        )),
    }
}

fn text_edge_matches(
    actual: &str,
    expected: &str,
    suffix: bool,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let Some(remaining) = actual.len().checked_sub(expected.len()) else {
        return Ok(false);
    };
    let start = if suffix { remaining } else { 0 };
    let actual = &actual.as_bytes()[start..start + expected.len()];
    for (actual, expected) in actual
        .chunks(64 * 1024)
        .zip(expected.as_bytes().chunks(64 * 1024))
    {
        control.check()?;
        if actual != expected {
            return Ok(false);
        }
    }
    Ok(true)
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

use super::persistent::ensure_index_cache;

#[cfg(test)]
mod tests;
