use std::collections::{BTreeMap, BTreeSet};

use lithograph_core::cypher::{NodeValue, PathValue, RelationshipValue, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum ExpectedValue {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
    List(Vec<ExpectedValue>),
    Map(BTreeMap<String, ExpectedValue>),
    Node(ExpectedNode),
    Relationship(ExpectedRelationship),
    Path(ExpectedPath),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedNode {
    labels: BTreeSet<String>,
    properties: BTreeMap<String, ExpectedValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedRelationship {
    relationship_type: Option<String>,
    properties: BTreeMap<String, ExpectedValue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathDirection {
    Forward,
    Reverse,
    Either,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedPath {
    nodes: Vec<ExpectedNode>,
    relationships: Vec<ExpectedRelationship>,
    directions: Vec<PathDirection>,
}

pub fn parse_expected_value(text: &str) -> Result<ExpectedValue, String> {
    let mut parser = ValueParser::new(text);
    let value = parser.parse_value()?;
    parser.skip_ws();
    if !parser.is_done() {
        return Err(format!(
            "unexpected trailing TCK value input at byte {} in {text:?}",
            parser.pos
        ));
    }
    Ok(value)
}

pub fn parse_parameter_value(text: &str) -> Result<Value, String> {
    expected_to_parameter(parse_expected_value(text)?)
}

pub fn value_matches_expected(
    actual: &Value,
    expected: &ExpectedValue,
    ignore_list_order: bool,
) -> bool {
    match (actual, expected) {
        (Value::Null, ExpectedValue::Null) => true,
        (Value::Boolean(actual), ExpectedValue::Boolean(expected)) => actual == expected,
        (Value::Integer(actual), ExpectedValue::Integer(expected)) => actual == expected,
        (Value::Float(actual), ExpectedValue::Float(expected)) => {
            (actual.is_nan() && expected.is_nan()) || actual == expected
        }
        (Value::String(actual), ExpectedValue::String(expected)) => actual == expected,
        (actual, ExpectedValue::String(expected)) => temporal_matches_tck_string(actual, expected),
        (Value::List(actual), ExpectedValue::List(expected)) => {
            values_match(actual, expected, ignore_list_order)
        }
        (Value::Map(actual), ExpectedValue::Map(expected)) => {
            maps_match(actual, expected, ignore_list_order)
        }
        (Value::Node(actual), ExpectedValue::Node(expected)) => {
            node_matches(actual, expected, ignore_list_order)
        }
        (Value::Relationship(actual), ExpectedValue::Relationship(expected)) => {
            relationship_matches(actual, expected, ignore_list_order)
        }
        (Value::Path(actual), ExpectedValue::Path(expected)) => {
            path_matches(actual, expected, ignore_list_order)
        }
        _ => false,
    }
}

fn temporal_matches_tck_string(actual: &Value, expected: &str) -> bool {
    let actual = match actual {
        Value::Date(value) => value.as_str().to_owned(),
        Value::LocalTime(value) => value.as_str().to_owned(),
        Value::Time(value) => value.as_str().to_owned(),
        Value::LocalDateTime(value) => value.as_str().to_owned(),
        Value::ZonedDateTime(value) => {
            if value.zone() == "Z" || value.zone().starts_with('+') || value.zone().starts_with('-')
            {
                value.value().to_owned()
            } else {
                format!("{}[{}]", value.value(), value.zone())
            }
        }
        Value::Duration(value) => value.as_str().to_owned(),
        _ => return false,
    };
    normalize_temporal_display(&actual) == normalize_temporal_display(expected)
}

fn normalize_temporal_display(value: &str) -> String {
    if value.starts_with('P') || value.starts_with("-P") || value.starts_with("+P") {
        return value.to_owned();
    }
    if let Some((date, time)) = value.split_once('T') {
        return format!("{date}T{}", normalize_time_display(time));
    }
    if value.contains(':') {
        return normalize_time_display(value);
    }
    value.to_owned()
}

fn normalize_time_display(value: &str) -> String {
    let (without_zone, zone_suffix) = match value.find('[') {
        Some(index) => (&value[..index], &value[index..]),
        None => (value, ""),
    };
    let (local, offset) = split_time_offset(without_zone);
    let local = if local.chars().filter(|ch| *ch == ':').count() == 1 {
        format!("{local}:00")
    } else {
        local.to_owned()
    };
    format!("{local}{offset}{zone_suffix}")
}

fn split_time_offset(value: &str) -> (&str, &str) {
    if let Some(local) = value.strip_suffix('Z') {
        return (local, "Z");
    }
    let offset_index = value
        .char_indices()
        .skip_while(|(index, _)| *index < 5)
        .find_map(|(index, ch)| matches!(ch, '+' | '-').then_some(index));
    match offset_index {
        Some(index) => (&value[..index], &value[index..]),
        None => (value, ""),
    }
}

fn expected_to_parameter(value: ExpectedValue) -> Result<Value, String> {
    match value {
        ExpectedValue::Null => Ok(Value::Null),
        ExpectedValue::Boolean(value) => Ok(Value::Boolean(value)),
        ExpectedValue::Integer(value) => Ok(Value::Integer(value)),
        ExpectedValue::Float(value) => Ok(Value::Float(value)),
        ExpectedValue::String(value) => Ok(Value::String(value)),
        ExpectedValue::List(values) => values
            .into_iter()
            .map(expected_to_parameter)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::List),
        ExpectedValue::Map(values) => values
            .into_iter()
            .map(|(key, value)| expected_to_parameter(value).map(|value| (key, value)))
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map(Value::Map),
        ExpectedValue::Node(_) | ExpectedValue::Relationship(_) | ExpectedValue::Path(_) => Err(
            "graph values are not legal openCypher parameter values in the TCK adapter".to_owned(),
        ),
    }
}

fn values_match(actual: &[Value], expected: &[ExpectedValue], ignore_list_order: bool) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    if ignore_list_order {
        let mut used = vec![false; actual.len()];
        return expected.iter().all(|expected_value| {
            actual.iter().enumerate().any(|(index, actual_value)| {
                !used[index] && value_matches_expected(actual_value, expected_value, true) && {
                    used[index] = true;
                    true
                }
            })
        });
    }
    actual
        .iter()
        .zip(expected)
        .all(|(actual, expected)| value_matches_expected(actual, expected, false))
}

fn maps_match(
    actual: &BTreeMap<String, Value>,
    expected: &BTreeMap<String, ExpectedValue>,
    ignore_list_order: bool,
) -> bool {
    actual.len() == expected.len()
        && expected.iter().all(|(key, expected)| {
            actual
                .get(key)
                .is_some_and(|actual| value_matches_expected(actual, expected, ignore_list_order))
        })
}

fn node_matches(actual: &NodeValue, expected: &ExpectedNode, ignore_list_order: bool) -> bool {
    actual.labels.iter().cloned().collect::<BTreeSet<_>>() == expected.labels
        && maps_match(&actual.properties, &expected.properties, ignore_list_order)
}

fn relationship_matches(
    actual: &RelationshipValue,
    expected: &ExpectedRelationship,
    ignore_list_order: bool,
) -> bool {
    expected
        .relationship_type
        .as_ref()
        .is_none_or(|expected| actual.relationship_type == *expected)
        && maps_match(&actual.properties, &expected.properties, ignore_list_order)
}

fn path_matches(actual: &PathValue, expected: &ExpectedPath, ignore_list_order: bool) -> bool {
    if actual.nodes.len() != expected.nodes.len()
        || actual.relationships.len() != expected.relationships.len()
        || expected.directions.len() != expected.relationships.len()
    {
        return false;
    }
    if !actual
        .nodes
        .iter()
        .zip(&expected.nodes)
        .all(|(actual, expected)| node_matches(actual, expected, ignore_list_order))
    {
        return false;
    }
    for (index, (actual_relationship, expected_relationship)) in actual
        .relationships
        .iter()
        .zip(&expected.relationships)
        .enumerate()
    {
        if !relationship_matches(
            actual_relationship,
            expected_relationship,
            ignore_list_order,
        ) {
            return false;
        }
        let current = &actual.nodes[index].element_id;
        let next = &actual.nodes[index + 1].element_id;
        let direction_matches = match expected.directions[index] {
            PathDirection::Forward => {
                actual_relationship.start == *current && actual_relationship.end == *next
            }
            PathDirection::Reverse => {
                actual_relationship.start == *next && actual_relationship.end == *current
            }
            PathDirection::Either => {
                (actual_relationship.start == *current && actual_relationship.end == *next)
                    || (actual_relationship.start == *next && actual_relationship.end == *current)
            }
        };
        if !direction_matches {
            return false;
        }
    }
    true
}

struct ValueParser<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> ValueParser<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, pos: 0 }
    }

    fn parse_value(&mut self) -> Result<ExpectedValue, String> {
        self.skip_ws();
        if self.starts_with("<(") {
            return self.parse_path().map(ExpectedValue::Path);
        }
        if self.starts_with("(") {
            return self.parse_node().map(ExpectedValue::Node);
        }
        if self.starts_with("[:") {
            return self.parse_relationship().map(ExpectedValue::Relationship);
        }
        if self.starts_with("[") {
            return self.parse_list().map(ExpectedValue::List);
        }
        if self.starts_with("{") {
            return self.parse_map().map(ExpectedValue::Map);
        }
        if self.starts_with("'") || self.starts_with("\"") {
            return self.parse_string().map(ExpectedValue::String);
        }
        if self.consume_keyword("null") {
            return Ok(ExpectedValue::Null);
        }
        if self.consume_keyword("true") {
            return Ok(ExpectedValue::Boolean(true));
        }
        if self.consume_keyword("false") {
            return Ok(ExpectedValue::Boolean(false));
        }
        if self.consume_keyword("NaN") {
            return Ok(ExpectedValue::Float(f64::NAN));
        }
        if self.consume_keyword("Infinity") {
            return Ok(ExpectedValue::Float(f64::INFINITY));
        }
        if self.consume_keyword("-Infinity") {
            return Ok(ExpectedValue::Float(f64::NEG_INFINITY));
        }
        self.parse_number()
    }

    fn parse_number(&mut self) -> Result<ExpectedValue, String> {
        let start = self.pos;
        if self.peek_char() == Some('-') || self.peek_char() == Some('+') {
            self.bump_char();
        }
        if self.starts_with("0x") || self.starts_with("0X") {
            self.pos += 2;
            let digits = self.take_while(|ch| ch.is_ascii_hexdigit());
            if digits.is_empty() {
                return self.error("hex integer has no digits");
            }
            let magnitude = i64::from_str_radix(digits, 16).map_err(|_| {
                format!("invalid TCK hex integer {:?}", &self.text[start..self.pos])
            })?;
            return Ok(ExpectedValue::Integer(
                if self.text[start..].starts_with('-') {
                    -magnitude
                } else {
                    magnitude
                },
            ));
        }
        self.take_while(|ch| ch.is_ascii_digit());
        let mut float = false;
        if self.peek_char() == Some('.') {
            float = true;
            self.bump_char();
            self.take_while(|ch| ch.is_ascii_digit());
        }
        if matches!(self.peek_char(), Some('e' | 'E')) {
            float = true;
            self.bump_char();
            if matches!(self.peek_char(), Some('+' | '-')) {
                self.bump_char();
            }
            self.take_while(|ch| ch.is_ascii_digit());
        }
        let token = &self.text[start..self.pos];
        if token.is_empty() || token == "+" || token == "-" {
            return self.error("expected a TCK value");
        }
        if float {
            token
                .parse::<f64>()
                .map(ExpectedValue::Float)
                .map_err(|_| format!("invalid TCK float {token:?}"))
        } else {
            token
                .parse::<i64>()
                .map(ExpectedValue::Integer)
                .map_err(|_| format!("invalid TCK integer {token:?}"))
        }
    }

    fn parse_list(&mut self) -> Result<Vec<ExpectedValue>, String> {
        self.expect("[")?;
        let mut values = Vec::new();
        loop {
            self.skip_ws();
            if self.consume("]") {
                return Ok(values);
            }
            values.push(self.parse_value()?);
            self.skip_ws();
            if self.consume("]") {
                return Ok(values);
            }
            self.expect(",")?;
        }
    }

    fn parse_map(&mut self) -> Result<BTreeMap<String, ExpectedValue>, String> {
        self.expect("{")?;
        let mut values = BTreeMap::new();
        loop {
            self.skip_ws();
            if self.consume("}") {
                return Ok(values);
            }
            let key = if matches!(self.peek_char(), Some('\'' | '"')) {
                self.parse_string()?
            } else {
                self.parse_identifier()?
            };
            self.skip_ws();
            self.expect(":")?;
            let value = self.parse_value()?;
            if values.insert(key, value).is_some() {
                return self.error("duplicate map key in TCK value");
            }
            self.skip_ws();
            if self.consume("}") {
                return Ok(values);
            }
            self.expect(",")?;
        }
    }

    fn parse_node(&mut self) -> Result<ExpectedNode, String> {
        self.expect("(")?;
        self.skip_ws();
        let mut labels = BTreeSet::new();
        while self.consume(":") {
            labels.insert(self.parse_identifier()?);
            self.skip_ws();
        }
        let properties = if self.starts_with("{") {
            self.parse_map()?
        } else {
            BTreeMap::new()
        };
        self.skip_ws();
        self.expect(")")?;
        Ok(ExpectedNode { labels, properties })
    }

    fn parse_relationship(&mut self) -> Result<ExpectedRelationship, String> {
        self.expect("[")?;
        self.skip_ws();
        let relationship_type = if self.consume(":") {
            Some(self.parse_identifier()?)
        } else {
            None
        };
        self.skip_ws();
        let properties = if self.starts_with("{") {
            self.parse_map()?
        } else {
            BTreeMap::new()
        };
        self.skip_ws();
        self.expect("]")?;
        Ok(ExpectedRelationship {
            relationship_type,
            properties,
        })
    }

    fn parse_path(&mut self) -> Result<ExpectedPath, String> {
        self.expect("<")?;
        let mut nodes = vec![self.parse_node()?];
        let mut relationships = Vec::new();
        let mut directions = Vec::new();
        loop {
            self.skip_ws();
            if self.consume(">") {
                return Ok(ExpectedPath {
                    nodes,
                    relationships,
                    directions,
                });
            }
            let direction = if self.consume("<-") {
                let relationship = self.parse_relationship()?;
                self.expect("-")?;
                relationships.push(relationship);
                PathDirection::Reverse
            } else {
                self.expect("-")?;
                let relationship = self.parse_relationship()?;
                relationships.push(relationship);
                if self.consume("->") {
                    PathDirection::Forward
                } else {
                    self.expect("-")?;
                    PathDirection::Either
                }
            };
            directions.push(direction);
            nodes.push(self.parse_node()?);
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        let quote = self
            .bump_char()
            .ok_or_else(|| "expected TCK string quote".to_owned())?;
        if !matches!(quote, '\'' | '"') {
            return self.error("expected TCK string quote");
        }
        let mut output = String::new();
        loop {
            let ch = self
                .bump_char()
                .ok_or_else(|| "unterminated TCK string".to_owned())?;
            if ch == quote {
                return Ok(output);
            }
            if ch != '\\' {
                output.push(ch);
                continue;
            }
            let escaped = self
                .bump_char()
                .ok_or_else(|| "unterminated TCK string escape".to_owned())?;
            match escaped {
                '\\' => output.push('\\'),
                '\'' => output.push('\''),
                '"' => output.push('"'),
                'n' => output.push('\n'),
                'r' => output.push('\r'),
                't' => output.push('\t'),
                'b' => output.push('\u{0008}'),
                'f' => output.push('\u{000c}'),
                'u' => output.push(self.parse_unicode_escape(4)?),
                'U' => output.push(self.parse_unicode_escape(8)?),
                other => output.push(other),
            }
        }
    }

    fn parse_unicode_escape(&mut self, digits: usize) -> Result<char, String> {
        if self.pos + digits > self.text.len() {
            return self.error("short Unicode escape in TCK string");
        }
        let text = &self.text[self.pos..self.pos + digits];
        if !text.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return self.error("invalid Unicode escape in TCK string");
        }
        self.pos += digits;
        let code = u32::from_str_radix(text, 16)
            .map_err(|_| format!("invalid Unicode escape {text:?}"))?;
        char::from_u32(code).ok_or_else(|| format!("invalid Unicode scalar U+{code:04X}"))
    }

    fn parse_identifier(&mut self) -> Result<String, String> {
        self.skip_ws();
        if self.consume("`") {
            let mut output = String::new();
            loop {
                let ch = self
                    .bump_char()
                    .ok_or_else(|| "unterminated escaped TCK identifier".to_owned())?;
                if ch != '`' {
                    output.push(ch);
                    continue;
                }
                if self.consume("`") {
                    output.push('`');
                    continue;
                }
                return Ok(output);
            }
        }
        let value = self.take_while(|ch| ch == '_' || ch.is_alphanumeric());
        if value.is_empty() {
            self.error("expected TCK identifier")
        } else {
            Ok(value.to_owned())
        }
    }

    fn consume_keyword(&mut self, keyword: &str) -> bool {
        if !self.starts_with(keyword) {
            return false;
        }
        let end = self.pos + keyword.len();
        let boundary = self.text[end..]
            .chars()
            .next()
            .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_');
        if boundary {
            self.pos = end;
        }
        boundary
    }

    fn take_while(&mut self, predicate: impl Fn(char) -> bool) -> &'a str {
        let start = self.pos;
        while self.peek_char().is_some_and(&predicate) {
            self.bump_char();
        }
        &self.text[start..self.pos]
    }

    fn expect(&mut self, token: &str) -> Result<(), String> {
        self.skip_ws();
        if self.consume(token) {
            Ok(())
        } else {
            self.error(&format!("expected {token:?}"))
        }
    }

    fn consume(&mut self, token: &str) -> bool {
        if self.starts_with(token) {
            self.pos += token.len();
            true
        } else {
            false
        }
    }

    fn starts_with(&self, token: &str) -> bool {
        self.text[self.pos..].starts_with(token)
    }

    fn skip_ws(&mut self) {
        while self.peek_char().is_some_and(char::is_whitespace) {
            self.bump_char();
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.text[self.pos..].chars().next()
    }

    fn bump_char(&mut self) -> Option<char> {
        let ch = self.peek_char()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn is_done(&self) -> bool {
        self.pos == self.text.len()
    }

    fn error<T>(&self, message: &str) -> Result<T, String> {
        Err(format!("{message} at byte {} in {:?}", self.pos, self.text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_tck_values() {
        let value = parse_expected_value("{node1: (:A {x: 1}), rel: [:T], xs: [null, 'x']}")
            .expect("value");
        let ExpectedValue::Map(value) = value else {
            panic!("expected map");
        };
        assert_eq!(value.len(), 3);
    }

    #[test]
    fn parses_directional_paths() {
        let value = parse_expected_value("<(:A)<-[:T {x: 1}]-(:B)-[:U]->(:C)>").expect("path");
        let ExpectedValue::Path(path) = value else {
            panic!("expected path");
        };
        assert_eq!(path.nodes.len(), 3);
        assert_eq!(path.relationships.len(), 2);
        assert_eq!(
            path.directions,
            vec![PathDirection::Reverse, PathDirection::Forward]
        );
    }

    #[test]
    fn temporal_values_match_tck_string_encoding() {
        let date = Value::Date(lithograph_core::cypher::DateValue::parse("1984-10-11").unwrap());
        assert!(value_matches_expected(
            &date,
            &ExpectedValue::String("1984-10-11".to_owned()),
            false,
        ));

        let time =
            Value::LocalTime(lithograph_core::cypher::LocalTimeValue::parse("12:31:00").unwrap());
        assert!(value_matches_expected(
            &time,
            &ExpectedValue::String("12:31".to_owned()),
            false,
        ));

        let zoned = Value::ZonedDateTime(
            lithograph_core::cypher::ZonedDateTimeValue::parse(
                "1984-10-11T12:00:00+01:00",
                "Europe/Stockholm",
            )
            .unwrap(),
        );
        assert!(value_matches_expected(
            &zoned,
            &ExpectedValue::String("1984-10-11T12:00+01:00[Europe/Stockholm]".to_owned()),
            false,
        ));
    }
}
