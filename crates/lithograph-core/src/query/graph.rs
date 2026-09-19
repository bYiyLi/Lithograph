use std::collections::BTreeMap;

use chrono::{Offset, TimeZone, Utc};
use rusqlite::Connection;

use crate::cypher::{
    NodeValue, RelationshipValue, Value, format_date_from_days, format_local_time, format_offset,
};
use crate::storage::{self, HashId, LabelId, OwnerKind, RelationshipRecord, Snapshot};

use super::options::{GraphViewSelector, SnapshotSelector};
use super::{QueryError, QueryErrorKind, QueryResult};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResolvedGraphView {
    required: Vec<LabelId>,
    excluded: Vec<LabelId>,
    required_unknown: bool,
}

impl ResolvedGraphView {
    pub(crate) fn resolve(
        connection: &Connection,
        selector: &GraphViewSelector,
    ) -> QueryResult<Self> {
        let mut required = Vec::new();
        let mut required_unknown = false;
        for name in &selector.require_all_labels {
            match storage::find_label(connection, name)? {
                Some(id) => required.push(id),
                None => required_unknown = true,
            }
        }
        let mut excluded = Vec::new();
        for name in &selector.exclude_any_labels {
            if let Some(id) = storage::find_label(connection, name)? {
                excluded.push(id);
            }
        }
        Ok(Self {
            required,
            excluded,
            required_unknown,
        })
    }

    pub(crate) fn visible_node(&self, snapshot: &Snapshot<'_>, node_id: i64) -> QueryResult<bool> {
        if self.required_unknown || !snapshot.node_exists(node_id)? {
            return Ok(false);
        }
        self.visible_existing_node(snapshot, node_id, None)
    }

    pub(crate) fn visible_existing_node(
        &self,
        snapshot: &Snapshot<'_>,
        node_id: i64,
        known_label: Option<LabelId>,
    ) -> QueryResult<bool> {
        if self.required_unknown {
            return Ok(false);
        }
        if self.required.is_empty() && self.excluded.is_empty() {
            return Ok(true);
        }
        if let Some(label) = known_label {
            if self.excluded.contains(&label) {
                return Ok(false);
            }
            if self.excluded.is_empty() && self.required.iter().all(|required| *required == label) {
                return Ok(true);
            }
        }
        let labels = snapshot.labels(node_id)?;
        Ok(self
            .required
            .iter()
            .all(|id| labels.binary_search(id).is_ok())
            && self
                .excluded
                .iter()
                .all(|id| labels.binary_search(id).is_err()))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.required_unknown
    }

    pub(crate) fn is_full_graph(&self) -> bool {
        !self.required_unknown && self.required.is_empty() && self.excluded.is_empty()
    }

    pub(crate) fn scan_label(&self) -> Option<LabelId> {
        if self.required_unknown {
            None
        } else {
            self.required.first().copied()
        }
    }

    pub(crate) fn visible_relationship(
        &self,
        snapshot: &Snapshot<'_>,
        relationship: RelationshipRecord,
    ) -> QueryResult<bool> {
        Ok(self.visible_node(snapshot, relationship.source)?
            && self.visible_node(snapshot, relationship.target)?)
    }
}

pub(crate) fn resolve_commit(
    connection: &Connection,
    selector: &SnapshotSelector,
) -> QueryResult<HashId> {
    match selector {
        SnapshotSelector::Current => {
            let branch = storage::active_branch(connection)?;
            resolve_branch(connection, &branch)
        }
        SnapshotSelector::Branch(name) => resolve_branch(connection, name),
        SnapshotSelector::Commit(value) => {
            if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(QueryError::invalid_argument(
                    "options.at Commit selector must contain a 64-character hexadecimal id",
                ));
            }
            let commit = HashId::from_hex(value).map_err(|_| {
                QueryError::new(
                    QueryErrorKind::VersionNotFound,
                    format!("Commit commit/{value} was not found"),
                )
            })?;
            if !storage::commit_exists(connection, commit)? {
                return Err(QueryError::new(
                    QueryErrorKind::VersionNotFound,
                    format!("Commit commit/{value} was not found"),
                ));
            }
            Ok(commit)
        }
        SnapshotSelector::Tag(name) => {
            validate_ref_selector(name)?;
            match storage::resolve_version_descriptor(connection, &format!("tag/{name}")) {
                Ok(commit) => Ok(commit),
                Err(storage::StorageError::NotFound(_)) => Err(QueryError::new(
                    QueryErrorKind::TagNotFound,
                    format!("Tag tag/{name} was not found"),
                )),
                Err(error) => Err(error.into()),
            }
        }
    }
}

fn resolve_branch(connection: &Connection, name: &str) -> QueryResult<HashId> {
    validate_ref_selector(name)?;
    match storage::branch_head(connection, name) {
        Ok(commit) => Ok(commit),
        Err(storage::StorageError::NotFound(_)) => Err(QueryError::new(
            QueryErrorKind::BranchNotFound,
            format!("Branch branch/{name} was not found"),
        )),
        Err(error) => Err(error.into()),
    }
}

fn validate_ref_selector(name: &str) -> QueryResult<()> {
    storage::validate_ref_name(name).map_err(|error| match error {
        storage::StorageError::Corrupt(message) => QueryError::invalid_argument(message),
        error => error.into(),
    })
}

pub(crate) fn materialize_node(snapshot: &Snapshot<'_>, node_id: i64) -> QueryResult<NodeValue> {
    if !snapshot.node_exists(node_id)? {
        return Err(QueryError::semantic(format!(
            "Node n:{node_id} is no longer present in the active graph state"
        )));
    }
    let mut labels = Vec::new();
    for label_id in snapshot.labels(node_id)? {
        let name =
            storage::label_name(snapshot_connection(snapshot), label_id)?.ok_or_else(|| {
                QueryError::internal(format!("Label dictionary id {label_id} is missing"))
            })?;
        labels.push(name);
    }
    let properties = materialize_properties(snapshot, OwnerKind::Node, node_id)?;
    Ok(NodeValue {
        element_id: format!("n:{node_id}"),
        labels,
        properties,
    })
}

pub(crate) fn materialize_relationship(
    snapshot: &Snapshot<'_>,
    relationship: RelationshipRecord,
) -> QueryResult<RelationshipValue> {
    if snapshot.relationship(relationship.id)?.is_none() {
        return Err(QueryError::semantic(format!(
            "Relationship r:{} is no longer present in the active graph state",
            relationship.id
        )));
    }
    let relationship_type =
        storage::relationship_type_name(snapshot_connection(snapshot), relationship.type_id)?
            .ok_or_else(|| {
                QueryError::internal(format!(
                    "Relationship Type id {} is missing",
                    relationship.type_id
                ))
            })?;
    let properties = materialize_properties(snapshot, OwnerKind::Relationship, relationship.id)?;
    Ok(RelationshipValue {
        element_id: format!("r:{}", relationship.id),
        relationship_type,
        start: format!("n:{}", relationship.source),
        end: format!("n:{}", relationship.target),
        properties,
    })
}

pub(crate) fn node_property(
    snapshot: &Snapshot<'_>,
    node_id: i64,
    key: &str,
) -> QueryResult<Value> {
    if !snapshot.node_exists(node_id)? {
        return Err(QueryError::semantic(format!(
            "Node n:{node_id} is no longer present in the active graph state"
        )));
    }
    element_property(snapshot, OwnerKind::Node, node_id, key)
}

pub(crate) fn relationship_property(
    snapshot: &Snapshot<'_>,
    id: i64,
    key: &str,
) -> QueryResult<Value> {
    if snapshot.relationship(id)?.is_none() {
        return Err(QueryError::semantic(format!(
            "Relationship r:{id} is no longer present in the active graph state"
        )));
    }
    element_property(snapshot, OwnerKind::Relationship, id, key)
}

fn materialize_properties(
    snapshot: &Snapshot<'_>,
    owner_kind: OwnerKind,
    owner_id: i64,
) -> QueryResult<BTreeMap<String, Value>> {
    let mut properties = BTreeMap::new();
    for (key_id, value) in snapshot.properties(owner_kind, owner_id)? {
        let name = storage::property_key_name(snapshot_connection(snapshot), key_id)?.ok_or_else(
            || QueryError::internal(format!("Property dictionary id {key_id} is missing")),
        )?;
        properties.insert(name, property_value(value)?);
    }
    Ok(properties)
}

fn element_property(
    snapshot: &Snapshot<'_>,
    owner_kind: OwnerKind,
    owner_id: i64,
    key: &str,
) -> QueryResult<Value> {
    let Some(key_id) = storage::find_property_key(snapshot_connection(snapshot), key)? else {
        return Ok(Value::Null);
    };
    snapshot
        .property(owner_kind, owner_id, key_id)?
        .map(property_value)
        .transpose()
        .map(|v| v.unwrap_or(Value::Null))
}

fn snapshot_connection<'a>(snapshot: &'a Snapshot<'a>) -> &'a Connection {
    snapshot.connection_for_query()
}

pub(crate) fn property_value(value: storage::PropertyValue) -> QueryResult<Value> {
    match value {
        storage::PropertyValue::Boolean(value) => Ok(Value::Boolean(value)),
        storage::PropertyValue::Integer(value) => Ok(Value::Integer(value)),
        storage::PropertyValue::Float(value) => Ok(Value::Float(value)),
        storage::PropertyValue::String(value) => Ok(Value::String(value)),
        storage::PropertyValue::List(values) => values
            .into_iter()
            .map(property_value)
            .collect::<QueryResult<Vec<_>>>()
            .map(Value::List),
        storage::PropertyValue::Date(days) => {
            let text = format_date_from_days(days);
            Ok(Value::Date(crate::cypher::DateValue::parse(&text)?))
        }
        storage::PropertyValue::LocalTime(nanoseconds) => Ok(Value::LocalTime(
            crate::cypher::LocalTimeValue::parse(&format_local_time(nanoseconds))?,
        )),
        storage::PropertyValue::Time {
            nanoseconds,
            offset_seconds,
        } => {
            let text = format!(
                "{}{}",
                format_local_time(nanoseconds),
                format_offset(offset_seconds)
            );
            Ok(Value::Time(crate::cypher::TimeValue::parse(&text)?))
        }
        storage::PropertyValue::LocalDateTime { day, nanoseconds } => {
            let text = format!(
                "{}T{}",
                format_date_from_days(day),
                format_local_time(nanoseconds)
            );
            Ok(Value::LocalDateTime(
                crate::cypher::LocalDateTimeValue::parse(&text)?,
            ))
        }
        storage::PropertyValue::ZonedDateTime(value) => zoned_datetime_value(value),
        storage::PropertyValue::Duration {
            months,
            days,
            seconds,
            nanoseconds,
        } => Ok(Value::Duration(
            crate::cypher::DurationValue::from_components(months, days, seconds, nanoseconds),
        )),
        storage::PropertyValue::Point(value) => point_value(value),
        storage::PropertyValue::Vector(value) => vector_value(value),
        storage::PropertyValue::Uuid(bytes) => {
            let text = uuid_text(bytes);
            Ok(Value::Uuid(crate::cypher::UuidValue::parse(&text)?))
        }
    }
}

fn point_value(value: storage::PointValue) -> QueryResult<Value> {
    let crs = match value.crs {
        4_326 => "wgs-84",
        4_979 => "wgs-84-3d",
        7_203 => "cartesian",
        9_157 => "cartesian-3d",
        _ => {
            return Err(QueryError::internal(format!(
                "unsupported persisted point CRS {}",
                value.crs
            )));
        }
    };
    Ok(Value::Point(crate::cypher::PointValue::new(
        crs,
        value.coordinates,
    )?))
}

fn vector_value(value: storage::VectorValue) -> QueryResult<Value> {
    use crate::cypher::{VectorCoordinateType as Ct, VectorValue as Cv, VectorValues as Vs};
    use storage::VectorCoordinateType as St;
    let values = match value.coordinate_type {
        St::I8 => Vs::I8(value.packed.into_iter().map(|v| v as i8).collect()),
        St::I16 => Vs::I16(
            value
                .packed
                .as_chunks::<2>()
                .0
                .iter()
                .map(|v| i16::from_le_bytes(*v))
                .collect(),
        ),
        St::I32 => Vs::I32(
            value
                .packed
                .as_chunks::<4>()
                .0
                .iter()
                .map(|v| i32::from_le_bytes(*v))
                .collect(),
        ),
        St::I64 => Vs::I64(
            value
                .packed
                .as_chunks::<8>()
                .0
                .iter()
                .map(|v| i64::from_le_bytes(*v))
                .collect(),
        ),
        St::F32 => Vs::F32(
            value
                .packed
                .as_chunks::<4>()
                .0
                .iter()
                .map(|v| f32::from_le_bytes(*v))
                .collect(),
        ),
        St::F64 => Vs::F64(
            value
                .packed
                .as_chunks::<8>()
                .0
                .iter()
                .map(|v| f64::from_le_bytes(*v))
                .collect(),
        ),
    };
    let coordinate_type = match value.coordinate_type {
        St::I8 => Ct::I8,
        St::I16 => Ct::I16,
        St::I32 => Ct::I32,
        St::I64 => Ct::I64,
        St::F32 => Ct::F32,
        St::F64 => Ct::F64,
    };
    Ok(Value::Vector(Cv::new(coordinate_type, values)?))
}

fn zoned_datetime_value(value: storage::ZonedDateTimeValue) -> QueryResult<Value> {
    let offset = zone_offset_at(&value.zone_id, value.epoch_seconds, value.nanoseconds)?;
    let local_seconds = value
        .epoch_seconds
        .checked_add(i64::from(offset))
        .ok_or_else(|| {
            QueryError::internal("persisted ZonedDateTime overflows local representation")
        })?;
    let day = local_seconds.div_euclid(86_400);
    let second = local_seconds.rem_euclid(86_400) as u64;
    let nanos = second * 1_000_000_000 + u64::from(value.nanoseconds);
    let text = format!(
        "{}T{}{}",
        format_date_from_days(day),
        format_local_time(nanos),
        format_offset(offset)
    );
    Ok(Value::ZonedDateTime(
        crate::cypher::ZonedDateTimeValue::parse(&text, &value.zone_id)?,
    ))
}

fn zone_offset_at(zone: &str, seconds: i64, nanoseconds: u32) -> QueryResult<i32> {
    if let Some(offset) = fixed_zone_offset(zone) {
        return Ok(offset);
    }
    let timezone = zone.parse::<chrono_tz::Tz>().map_err(|_| {
        QueryError::internal(format!(
            "persisted ZonedDateTime has unknown IANA zone {zone:?}"
        ))
    })?;
    let instant = Utc
        .timestamp_opt(seconds, nanoseconds)
        .single()
        .ok_or_else(|| {
            QueryError::internal("persisted ZonedDateTime is outside the IANA timezone range")
        })?;
    Ok(instant
        .with_timezone(&timezone)
        .offset()
        .fix()
        .local_minus_utc())
}

fn fixed_zone_offset(zone: &str) -> Option<i32> {
    if matches!(zone, "Z" | "UTC" | "GMT" | "GMT0") {
        return Some(0);
    }
    let bytes = zone.as_bytes();
    if bytes.len() != 6 || !matches!(bytes[0], b'+' | b'-') || bytes[3] != b':' {
        return None;
    }
    let hour = std::str::from_utf8(&bytes[1..3])
        .ok()?
        .parse::<i32>()
        .ok()?;
    let minute = std::str::from_utf8(&bytes[4..6])
        .ok()?
        .parse::<i32>()
        .ok()?;
    let sign = if bytes[0] == b'-' { -1 } else { 1 };
    Some(sign * (hour * 3_600 + minute * 60))
}

fn uuid_text(bytes: [u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}
