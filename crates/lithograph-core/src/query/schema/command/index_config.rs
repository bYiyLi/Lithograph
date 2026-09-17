use std::collections::BTreeMap;

use crate::cypher::{AstKind, AstNode, ExpressionKind, Value};
use crate::query::expression::{Expr, UnaryOp, compile_expression};
use crate::query::{QueryResult, schema::command::schema_error};
use crate::storage::{IndexConfiguration, StandardIndexKind};

pub(super) fn parse_index_configuration(
    clause: &AstNode,
    kind: StandardIndexKind,
) -> QueryResult<Option<IndexConfiguration>> {
    if !matches!(
        kind,
        StandardIndexKind::FullText | StandardIndexKind::Vector
    ) {
        return Ok(None);
    }
    let target_end = clause
        .descendants()
        .find(|node| node.kind == AstKind::IndexTarget)
        .map_or(0, |node| node.span.end);
    let options = clause
        .descendants()
        .filter(|node| {
            matches!(node.kind, AstKind::Expression(ExpressionKind::Map))
                && node.span.start >= target_end
        })
        .min_by_key(|node| node.span.start)
        .map(compile_expression)
        .transpose()?
        .map(constant_schema_value)
        .transpose()?
        .unwrap_or_else(|| Value::Map(BTreeMap::new()));
    let Value::Map(options) = options else {
        return Err(schema_error("Index OPTIONS must be a literal Map"));
    };
    if options.keys().any(|key| key != "indexConfig") {
        return Err(schema_error(
            "semantic Index OPTIONS supports only the indexConfig map",
        ));
    }
    let config = match options.get("indexConfig") {
        None => BTreeMap::new(),
        Some(Value::Map(config)) => config.clone(),
        Some(_) => return Err(schema_error("Index OPTIONS indexConfig must be a Map")),
    };
    match kind {
        StandardIndexKind::FullText => parse_fulltext_configuration(config).map(Some),
        StandardIndexKind::Vector => parse_vector_configuration(config).map(Some),
        _ => Ok(None),
    }
}

fn parse_fulltext_configuration(
    mut config: BTreeMap<String, Value>,
) -> QueryResult<IndexConfiguration> {
    let analyzer = take_string_option(&mut config, "fulltext.analyzer", "unicode61")?;
    let eventually_consistent =
        take_boolean_option(&mut config, "fulltext.eventually_consistent", false)?;
    ensure_no_unknown_index_config(config)?;
    crate::query::semantic_index::validate_fulltext_schema_specification(&analyzer)?;
    Ok(IndexConfiguration::FullText {
        analyzer,
        eventually_consistent,
    })
}

fn parse_vector_configuration(
    mut config: BTreeMap<String, Value>,
) -> QueryResult<IndexConfiguration> {
    let dimensions = match config.remove("vector.dimensions") {
        None => None,
        Some(Value::Integer(value)) if (1..=4096).contains(&value) => Some(value as u64),
        Some(_) => {
            return Err(schema_error(
                "vector.dimensions must be an Integer between 1 and 4096",
            ));
        }
    };
    let similarity_function =
        take_string_option(&mut config, "vector.similarity_function", "cosine")?
            .to_ascii_lowercase();
    if !matches!(similarity_function.as_str(), "cosine" | "euclidean") {
        return Err(schema_error(
            "vector.similarity_function must be 'cosine' or 'euclidean'",
        ));
    }
    let legacy_quantization = match config.remove("vector.quantization.enabled") {
        None => None,
        Some(Value::Boolean(value)) => Some(value),
        Some(_) => {
            return Err(schema_error(
                "vector.quantization.enabled must be Boolean when supplied",
            ));
        }
    };
    let quantization_type = match config.remove("vector.quantization.type") {
        None => match legacy_quantization {
            Some(true) => "scalar".to_owned(),
            Some(false) => "none".to_owned(),
            None => "binary".to_owned(),
        },
        Some(Value::String(value)) => value.to_ascii_lowercase(),
        Some(_) => return Err(schema_error("vector.quantization.type must be String")),
    };
    if !matches!(quantization_type.as_str(), "none" | "scalar" | "binary") {
        return Err(schema_error(
            "vector.quantization.type must be 'none', 'scalar', or 'binary'",
        ));
    }

    let default_expansion = match quantization_type.as_str() {
        "none" => 1.0,
        "scalar" => 1.5,
        _ => 3.0,
    };
    let expansion = take_number_option(
        &mut config,
        "vector.default_search_expansion_factor",
        default_expansion,
        1.0,
        10_000.0,
    )?;
    let hnsw_m = take_integer_option(&mut config, "vector.hnsw.m", 16, 1, 512)?;
    let hnsw_ef_construction =
        take_integer_option(&mut config, "vector.hnsw.ef_construction", 100, 1, 3200)?;
    ensure_no_unknown_index_config(config)?;
    Ok(IndexConfiguration::Vector {
        dimensions,
        similarity_function,
        quantization_type,
        default_search_expansion_factor: expansion.to_string(),
        hnsw_m,
        hnsw_ef_construction,
    })
}

fn constant_schema_value(expression: Expr) -> QueryResult<Value> {
    match expression {
        Expr::Literal(value) => Ok(value),
        Expr::List(values) => values
            .into_iter()
            .map(constant_schema_value)
            .collect::<QueryResult<Vec<_>>>()
            .map(Value::List),
        Expr::Map(entries) => entries
            .into_iter()
            .map(|(key, value)| Ok((key, constant_schema_value(value)?)))
            .collect::<QueryResult<BTreeMap<_, _>>>()
            .map(Value::Map),
        Expr::Unary(UnaryOp::Positive, value) => constant_schema_value(*value),
        Expr::Unary(UnaryOp::Negative, value) => match constant_schema_value(*value)? {
            Value::Integer(value) => value
                .checked_neg()
                .map(Value::Integer)
                .ok_or_else(|| schema_error("Index OPTIONS Integer is out of range")),
            Value::Float(value) => Ok(Value::Float(-value)),
            _ => Err(schema_error("Index OPTIONS unary '-' requires a number")),
        },
        _ => Err(schema_error(
            "Index OPTIONS must contain only literal values",
        )),
    }
}

fn take_string_option(
    config: &mut BTreeMap<String, Value>,
    key: &str,
    default: &str,
) -> QueryResult<String> {
    match config.remove(key) {
        None => Ok(default.to_owned()),
        Some(Value::String(value)) => Ok(value),
        Some(_) => Err(schema_error(format!("{key} must be String"))),
    }
}

fn take_boolean_option(
    config: &mut BTreeMap<String, Value>,
    key: &str,
    default: bool,
) -> QueryResult<bool> {
    match config.remove(key) {
        None => Ok(default),
        Some(Value::Boolean(value)) => Ok(value),
        Some(_) => Err(schema_error(format!("{key} must be Boolean"))),
    }
}

fn take_integer_option(
    config: &mut BTreeMap<String, Value>,
    key: &str,
    default: u64,
    minimum: u64,
    maximum: u64,
) -> QueryResult<u64> {
    let Some(value) = config.remove(key) else {
        return Ok(default);
    };
    let Value::Integer(value) = value else {
        return Err(schema_error(format!("{key} must be Integer")));
    };
    let value = u64::try_from(value)
        .map_err(|_| schema_error(format!("{key} is outside its supported range")))?;
    if (minimum..=maximum).contains(&value) {
        Ok(value)
    } else {
        Err(schema_error(format!(
            "{key} is outside its supported range"
        )))
    }
}

fn take_number_option(
    config: &mut BTreeMap<String, Value>,
    key: &str,
    default: f64,
    minimum: f64,
    maximum: f64,
) -> QueryResult<f64> {
    let value = match config.remove(key) {
        None => return Ok(default),
        Some(Value::Integer(value)) => value as f64,
        Some(Value::Float(value)) => value,
        Some(_) => return Err(schema_error(format!("{key} must be numeric"))),
    };
    if value.is_finite() && (minimum..=maximum).contains(&value) {
        Ok(value)
    } else {
        Err(schema_error(format!(
            "{key} is outside its supported range"
        )))
    }
}

fn ensure_no_unknown_index_config(config: BTreeMap<String, Value>) -> QueryResult<()> {
    if let Some(key) = config.keys().next() {
        Err(schema_error(format!(
            "unsupported semantic Index configuration key {key:?}"
        )))
    } else {
        Ok(())
    }
}
