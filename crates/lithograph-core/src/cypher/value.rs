use std::cmp::Ordering;
use std::collections::BTreeMap;

use super::temporal::{
    DateValue, DurationValue, LocalDateTimeValue, LocalTimeValue, TimeValue, ZonedDateTimeValue,
    compare_time, compare_zoned_datetime,
};
pub use super::uuid::UuidValue;

/// Cypher runtime value, deliberately distinct from persistent graph properties.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
    Node(NodeValue),
    Relationship(RelationshipValue),
    Path(PathValue),
    Date(DateValue),
    LocalTime(LocalTimeValue),
    Time(TimeValue),
    LocalDateTime(LocalDateTimeValue),
    ZonedDateTime(ZonedDateTimeValue),
    Duration(DurationValue),
    Point(PointValue),
    Vector(VectorValue),
    Uuid(UuidValue),
}

impl Value {
    /// Returns whether this value can be supplied as a Cypher parameter.
    pub fn is_parameter_value(&self) -> bool {
        match self {
            Self::Node(_) | Self::Relationship(_) | Self::Path(_) => false,
            Self::List(values) => values.iter().all(Self::is_parameter_value),
            Self::Map(values) => values.values().all(Self::is_parameter_value),
            _ => true,
        }
    }

    /// Returns whether this runtime value is a valid persistent property value.
    pub fn is_property_value(&self) -> bool {
        match self {
            Self::Null | Self::Map(_) | Self::Node(_) | Self::Relationship(_) | Self::Path(_) => {
                false
            }
            Self::List(values) => property_list_is_valid(values),
            _ => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeValue {
    pub element_id: String,
    pub labels: Vec<String>,
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelationshipValue {
    pub element_id: String,
    pub relationship_type: String,
    pub start: String,
    pub end: String,
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathValue {
    pub nodes: Vec<NodeValue>,
    pub relationships: Vec<RelationshipValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PointValue {
    crs: String,
    srid: u32,
    coordinates: Vec<f64>,
}

impl PointValue {
    pub fn new(crs: &str, mut coordinates: Vec<f64>) -> Result<Self, ValueError> {
        let (canonical_crs, srid, dimension, geographic) = match crs.to_ascii_lowercase().as_str() {
            "wgs-84" => ("wgs-84", 4_326, 2, true),
            "wgs-84-3d" => ("wgs-84-3d", 4_979, 3, true),
            "cartesian" => ("cartesian", 7_203, 2, false),
            "cartesian-3d" => ("cartesian-3d", 9_157, 3, false),
            _ => return Err(ValueError::new("Point crs is unsupported")),
        };
        if coordinates.len() != dimension {
            return Err(ValueError::new(format!(
                "Point {canonical_crs} requires exactly {dimension} coordinates"
            )));
        }
        if coordinates.iter().any(|value| !value.is_finite()) {
            return Err(ValueError::new("Point coordinates must be finite"));
        }
        if geographic && !(-90.0..=90.0).contains(&coordinates[1]) {
            return Err(ValueError::new(
                "WGS-84 latitude is outside its valid range",
            ));
        }
        if geographic && !(-180.0..=180.0).contains(&coordinates[0]) {
            coordinates[0] = (coordinates[0] + 180.0).rem_euclid(360.0) - 180.0;
        }
        Ok(Self {
            crs: canonical_crs.to_owned(),
            srid,
            coordinates,
        })
    }

    pub fn crs(&self) -> &str {
        &self.crs
    }

    pub fn coordinates(&self) -> &[f64] {
        &self.coordinates
    }

    pub fn srid(&self) -> u32 {
        self.srid
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VectorValue {
    coordinate_type: VectorCoordinateType,
    values: VectorValues,
}

impl VectorValue {
    pub fn new(
        coordinate_type: VectorCoordinateType,
        values: VectorValues,
    ) -> Result<Self, ValueError> {
        let matches_type = matches!(
            (coordinate_type, &values),
            (VectorCoordinateType::I8, VectorValues::I8(_))
                | (VectorCoordinateType::I16, VectorValues::I16(_))
                | (VectorCoordinateType::I32, VectorValues::I32(_))
                | (VectorCoordinateType::I64, VectorValues::I64(_))
                | (VectorCoordinateType::F32, VectorValues::F32(_))
                | (VectorCoordinateType::F64, VectorValues::F64(_))
        );
        if !matches_type {
            return Err(ValueError::new(
                "Vector values do not match their coordinateType",
            ));
        }
        let dimension = values.len();
        if !(1..=4_096).contains(&dimension) {
            return Err(ValueError::new(
                "Vector dimension must be between 1 and 4096",
            ));
        }
        let finite = match &values {
            VectorValues::F32(values) => values.iter().all(|value| value.is_finite()),
            VectorValues::F64(values) => values.iter().all(|value| value.is_finite()),
            _ => true,
        };
        if !finite {
            return Err(ValueError::new(
                "Vector coordinates cannot contain NaN or Infinity",
            ));
        }
        Ok(Self {
            coordinate_type,
            values,
        })
    }

    pub fn dimension(&self) -> usize {
        self.values.len()
    }

    pub fn coordinate_type(&self) -> VectorCoordinateType {
        self.coordinate_type
    }

    pub fn values(&self) -> &VectorValues {
        &self.values
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorCoordinateType {
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}

impl VectorCoordinateType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::I8 => "I8",
            Self::I16 => "I16",
            Self::I32 => "I32",
            Self::I64 => "I64",
            Self::F32 => "F32",
            Self::F64 => "F64",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_uppercase().as_str() {
            "I8" | "INTEGER8" => Some(Self::I8),
            "I16" | "INTEGER16" => Some(Self::I16),
            "I32" | "INTEGER32" => Some(Self::I32),
            "I64" | "INTEGER" | "INTEGER64" => Some(Self::I64),
            "F32" | "FLOAT32" => Some(Self::F32),
            "F64" | "FLOAT" | "FLOAT64" => Some(Self::F64),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum VectorValues {
    I8(Vec<i8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
}

impl VectorValues {
    pub fn len(&self) -> usize {
        match self {
            Self::I8(values) => values.len(),
            Self::I16(values) => values.len(),
            Self::I32(values) => values.len(),
            Self::I64(values) => values.len(),
            Self::F32(values) => values.len(),
            Self::F64(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueError {
    pub message: String,
}

impl ValueError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ValueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ValueError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CypherComparison {
    Less,
    Equal,
    Greater,
    Unordered,
}

impl From<Ordering> for CypherComparison {
    fn from(value: Ordering) -> Self {
        match value {
            Ordering::Less => Self::Less,
            Ordering::Equal => Self::Equal,
            Ordering::Greater => Self::Greater,
        }
    }
}

/// Cypher three-valued equality.
pub fn cypher_equals(left: &Value, right: &Value) -> Result<Option<bool>, ValueError> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(None);
    }
    let result = match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => Some(left == right),
        (Value::Integer(left), Value::Float(right)) => {
            integer_float_cmp(*left, *right).map(|ordering| ordering == Ordering::Equal)
        }
        (Value::Float(left), Value::Integer(right)) => {
            integer_float_cmp(*right, *left).map(|ordering| ordering == Ordering::Equal)
        }
        (Value::Float(left), Value::Float(right)) => Some(numeric_equal(*left, *right)),
        (Value::Boolean(left), Value::Boolean(right)) => Some(left == right),
        (Value::String(left), Value::String(right)) => Some(left == right),
        (Value::List(left), Value::List(right)) => list_equals(left, right)?,
        (Value::Map(left), Value::Map(right)) => map_equals(left, right)?,
        (Value::Uuid(left), Value::Uuid(right)) => Some(left == right),
        (Value::Date(left), Value::Date(right)) => Some(left.days() == right.days()),
        (Value::LocalTime(left), Value::LocalTime(right)) => {
            Some(left.nanoseconds() == right.nanoseconds())
        }
        (Value::Time(left), Value::Time(right)) => {
            Some(left.comparison_key() == right.comparison_key())
        }
        (Value::LocalDateTime(left), Value::LocalDateTime(right)) => {
            Some(left.comparison_key() == right.comparison_key())
        }
        (Value::Duration(left), Value::Duration(right)) => {
            Some(left.components() == right.components())
        }
        (Value::ZonedDateTime(left), Value::ZonedDateTime(right)) => {
            Some(left.comparison_key() == right.comparison_key())
        }
        (Value::Point(left), Value::Point(right)) => Some(left == right),
        (Value::Vector(left), Value::Vector(right)) => vector_equal(left, right)?,
        (Value::Node(left), Value::Node(right)) => Some(left.element_id == right.element_id),
        (Value::Relationship(left), Value::Relationship(right)) => {
            Some(left.element_id == right.element_id)
        }
        (Value::Path(left), Value::Path(right)) => Some(path_identity_equals(left, right)),
        (Value::Path(path), Value::List(values)) | (Value::List(values), Value::Path(path)) => {
            Some(path_list_equals(path, values))
        }
        _ => Some(false),
    };
    Ok(result)
}

/// Comparison semantics for `<`, `<=`, `>`, and `>=`.
///
/// `Ok(None)` is the Cypher `null` result (including direct DURATION and POINT comparisons),
/// while `Unordered` represents NaN, for which every ordering predicate is false.
/// Incomparable types return an error instead of being silently treated as null.
pub fn cypher_compare(left: &Value, right: &Value) -> Result<Option<CypherComparison>, ValueError> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(None);
    }
    let comparison = match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => CypherComparison::from(left.cmp(right)),
        (Value::Integer(left), Value::Float(right)) => integer_float_cmp(*left, *right)
            .map(CypherComparison::from)
            .unwrap_or(CypherComparison::Unordered),
        (Value::Float(left), Value::Integer(right)) => integer_float_cmp(*right, *left)
            .map(Ordering::reverse)
            .map(CypherComparison::from)
            .unwrap_or(CypherComparison::Unordered),
        (Value::Float(left), Value::Float(right)) => partial_number_cmp(*left, *right)
            .map(CypherComparison::from)
            .unwrap_or(CypherComparison::Unordered),
        (Value::String(left), Value::String(right)) => CypherComparison::from(left.cmp(right)),
        (Value::Boolean(left), Value::Boolean(right)) => CypherComparison::from(left.cmp(right)),
        (Value::Date(left), Value::Date(right)) => {
            CypherComparison::from(left.days().cmp(&right.days()))
        }
        (Value::LocalTime(left), Value::LocalTime(right)) => {
            CypherComparison::from(left.nanoseconds().cmp(&right.nanoseconds()))
        }
        (Value::Time(left), Value::Time(right)) => {
            CypherComparison::from(compare_time(left, right))
        }
        (Value::LocalDateTime(left), Value::LocalDateTime(right)) => {
            CypherComparison::from(left.comparison_key().cmp(&right.comparison_key()))
        }
        (Value::ZonedDateTime(left), Value::ZonedDateTime(right)) => {
            CypherComparison::from(compare_zoned_datetime(left, right))
        }
        (Value::Duration(_), Value::Duration(_)) | (Value::Point(_), Value::Point(_)) => {
            return Ok(None);
        }
        _ => {
            return Err(ValueError::new(format!(
                "Cypher ordering comparison cannot compare {} with {}",
                value_type_name(left),
                value_type_name(right)
            )));
        }
    };
    Ok(Some(comparison))
}

fn list_equals(left: &[Value], right: &[Value]) -> Result<Option<bool>, ValueError> {
    if left.len() != right.len() {
        return Ok(Some(false));
    }
    let mut has_null = false;
    for (left, right) in left.iter().zip(right) {
        match cypher_equals(left, right)? {
            Some(true) => {}
            Some(false) => return Ok(Some(false)),
            None => has_null = true,
        }
    }
    Ok((!has_null).then_some(true))
}

fn map_equals(
    left: &BTreeMap<String, Value>,
    right: &BTreeMap<String, Value>,
) -> Result<Option<bool>, ValueError> {
    if left.len() != right.len() || left.keys().ne(right.keys()) {
        return Ok(Some(false));
    }
    let mut has_null = false;
    for (key, left_value) in left {
        let Some(right_value) = right.get(key) else {
            return Ok(Some(false));
        };
        match cypher_equals(left_value, right_value)? {
            Some(true) => {}
            Some(false) => return Ok(Some(false)),
            None => has_null = true,
        }
    }
    Ok((!has_null).then_some(true))
}

fn vector_equal(left: &VectorValue, right: &VectorValue) -> Result<Option<bool>, ValueError> {
    if left.coordinate_type() != right.coordinate_type() || left.dimension() != right.dimension() {
        return Err(ValueError::new(
            "VECTOR values with different coordinate types or dimensions are not equality-comparable",
        ));
    }
    let equal = match (&left.values, &right.values) {
        (VectorValues::I8(left), VectorValues::I8(right)) => left == right,
        (VectorValues::I16(left), VectorValues::I16(right)) => left == right,
        (VectorValues::I32(left), VectorValues::I32(right)) => left == right,
        (VectorValues::I64(left), VectorValues::I64(right)) => left == right,
        (VectorValues::F32(left), VectorValues::F32(right)) => left
            .iter()
            .zip(right)
            .all(|(left, right)| numeric_equal(f64::from(*left), f64::from(*right))),
        (VectorValues::F64(left), VectorValues::F64(right)) => left
            .iter()
            .zip(right)
            .all(|(left, right)| numeric_equal(*left, *right)),
        _ => false,
    };
    Ok(Some(equal))
}

fn path_identity_equals(left: &PathValue, right: &PathValue) -> bool {
    left.nodes.len() == right.nodes.len()
        && left.relationships.len() == right.relationships.len()
        && left
            .nodes
            .iter()
            .zip(&right.nodes)
            .all(|(left, right)| left.element_id == right.element_id)
        && left
            .relationships
            .iter()
            .zip(&right.relationships)
            .all(|(left, right)| left.element_id == right.element_id)
}

fn path_list_equals(path: &PathValue, values: &[Value]) -> bool {
    if path.nodes.len() != path.relationships.len().saturating_add(1)
        || values.len() != path.nodes.len().saturating_add(path.relationships.len())
    {
        return false;
    }
    for (index, node) in path.nodes.iter().enumerate() {
        let Some(Value::Node(node_value)) = values.get(index.saturating_mul(2)) else {
            return false;
        };
        if node.element_id != node_value.element_id {
            return false;
        }
        if let Some(relationship) = path.relationships.get(index) {
            let Some(Value::Relationship(relationship_value)) =
                values.get(index.saturating_mul(2).saturating_add(1))
            else {
                return false;
            };
            if relationship.element_id != relationship_value.element_id {
                return false;
            }
        }
    }
    true
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Boolean(_) => "BOOLEAN",
        Value::Integer(_) => "INTEGER",
        Value::Float(_) => "FLOAT",
        Value::String(_) => "STRING",
        Value::List(_) => "LIST",
        Value::Map(_) => "MAP",
        Value::Node(_) => "NODE",
        Value::Relationship(_) => "RELATIONSHIP",
        Value::Path(_) => "PATH",
        Value::Date(_) => "DATE",
        Value::LocalTime(_) => "LOCAL TIME",
        Value::Time(_) => "ZONED TIME",
        Value::LocalDateTime(_) => "LOCAL DATETIME",
        Value::ZonedDateTime(_) => "ZONED DATETIME",
        Value::Duration(_) => "DURATION",
        Value::Point(_) => "POINT",
        Value::Vector(_) => "VECTOR",
        Value::Uuid(_) => "UUID",
    }
}

fn numeric_equal(left: f64, right: f64) -> bool {
    if left.is_nan() || right.is_nan() {
        false
    } else {
        left == right
    }
}

fn partial_number_cmp(left: f64, right: f64) -> Option<Ordering> {
    if left.is_nan() || right.is_nan() {
        None
    } else {
        left.partial_cmp(&right)
    }
}

pub(super) fn integer_float_cmp(integer: i64, float: f64) -> Option<Ordering> {
    if float.is_nan() {
        return None;
    }
    if float == f64::INFINITY {
        return Some(Ordering::Less);
    }
    if float == f64::NEG_INFINITY {
        return Some(Ordering::Greater);
    }
    const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;
    const I64_LOWER_INCLUSIVE: f64 = -9_223_372_036_854_775_808.0;
    if float >= I64_UPPER_EXCLUSIVE {
        return Some(Ordering::Less);
    }
    if float < I64_LOWER_INCLUSIVE {
        return Some(Ordering::Greater);
    }
    let truncated = float.trunc() as i64;
    match integer.cmp(&truncated) {
        Ordering::Equal => {
            let fractional = float - float.trunc();
            if fractional > 0.0 {
                Some(Ordering::Less)
            } else if fractional < 0.0 {
                Some(Ordering::Greater)
            } else {
                Some(Ordering::Equal)
            }
        }
        ordering => Some(ordering),
    }
}

fn property_list_is_valid(values: &[Value]) -> bool {
    let Some(first) = values.first() else {
        return true;
    };
    let Some(family) = property_list_family(first) else {
        return false;
    };
    values
        .iter()
        .all(|value| property_list_family(value) == Some(family))
}

fn property_list_family(value: &Value) -> Option<u8> {
    match value {
        Value::Boolean(_) => Some(1),
        Value::Integer(_) => Some(2),
        Value::Float(_) => Some(3),
        Value::String(_) => Some(4),
        Value::Date(_) => Some(5),
        Value::LocalTime(_) => Some(6),
        Value::Time(_) => Some(7),
        Value::LocalDateTime(_) => Some(8),
        Value::ZonedDateTime(_) => Some(9),
        Value::Duration(_) => Some(10),
        Value::Point(_) => Some(11),
        Value::Uuid(_) => Some(12),
        Value::Null
        | Value::List(_)
        | Value::Map(_)
        | Value::Node(_)
        | Value::Relationship(_)
        | Value::Path(_)
        | Value::Vector(_) => None,
    }
}
