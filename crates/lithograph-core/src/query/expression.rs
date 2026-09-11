use std::collections::{BTreeMap, BTreeSet};

use crate::cypher::{
    self, AstKind, AstNode, CypherComparison, ExpressionKind, LiteralKind, PathValue, Value,
    unescape_identifier,
};
use crate::storage::{RelationshipRecord, Snapshot};

use super::graph::{
    materialize_node, materialize_relationship, node_property, relationship_property,
};
use super::{QueryError, QueryResult};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BindingValue {
    Node(i64),
    Relationship(RelationshipRecord),
    Path {
        nodes: Vec<i64>,
        relationships: Vec<RelationshipRecord>,
    },
    Null,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct BindingRow {
    pub values: BTreeMap<String, BindingValue>,
    pub used_relationships: BTreeSet<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Literal(Value),
    List(Vec<Expr>),
    Map(BTreeMap<String, Expr>),
    Variable(String),
    Parameter(String),
    Property(Box<Expr>, String),
    Function(String, Vec<Expr>),
    Unary(UnaryOp, Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnaryOp {
    Not,
    Positive,
    Negative,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryOp {
    Or,
    Xor,
    And,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Power,
}

pub(crate) fn compile_expression(node: &AstNode) -> QueryResult<Expr> {
    match &node.kind {
        AstKind::Literal(kind) => compile_literal(*kind, node.text.as_deref().unwrap_or_default()),
        AstKind::Variable => Ok(Expr::Variable(node.text.clone().unwrap_or_default())),
        AstKind::Parameter => Ok(Expr::Parameter(
            node.text
                .as_deref()
                .unwrap_or_default()
                .trim_start_matches('$')
                .to_owned(),
        )),
        AstKind::Expression(kind) => compile_expression_kind(node, *kind),
        AstKind::Syntax => compile_only_child(node),
        other => Err(QueryError::semantic(format!(
            "Phase 04 cannot execute expression node {other:?}"
        ))),
    }
}

fn compile_expression_kind(node: &AstNode, kind: ExpressionKind) -> QueryResult<Expr> {
    match kind {
        ExpressionKind::Expression | ExpressionKind::Primary | ExpressionKind::Parenthesized => {
            compile_only_child(node)
        }
        ExpressionKind::Or => compile_fold(node, BinaryOp::Or),
        ExpressionKind::Xor => compile_fold(node, BinaryOp::Xor),
        ExpressionKind::And => compile_fold(node, BinaryOp::And),
        ExpressionKind::Comparison => compile_comparison(node),
        ExpressionKind::Additive => compile_operator_fold(node),
        ExpressionKind::Multiplicative => compile_operator_fold(node),
        ExpressionKind::Power => compile_operator_fold(node),
        ExpressionKind::Unary => compile_unary(node),
        ExpressionKind::Not => compile_not(node),
        ExpressionKind::Postfix => compile_postfix(node),
        ExpressionKind::FunctionCall => compile_function(node),
        ExpressionKind::List => compile_list(node),
        ExpressionKind::Map => compile_map(node),
        _ => Err(QueryError::semantic(format!(
            "Lithograph does not execute {kind:?} expressions yet"
        ))),
    }
}

fn compile_list(node: &AstNode) -> QueryResult<Expr> {
    if node
        .descendants()
        .any(|child| matches!(child.kind, AstKind::Pattern | AstKind::BindingVariable))
    {
        return Err(QueryError::semantic(
            "list and pattern comprehensions are owned by Phase 06",
        ));
    }
    let Some(arguments) = node
        .children
        .iter()
        .find(|child| matches!(child.kind, AstKind::ArgumentList))
    else {
        return Ok(Expr::List(Vec::new()));
    };
    arguments
        .children
        .iter()
        .filter(|child| is_expression_value(child))
        .map(compile_expression)
        .collect::<QueryResult<Vec<_>>>()
        .map(Expr::List)
}

fn compile_map(node: &AstNode) -> QueryResult<Expr> {
    if node
        .children
        .iter()
        .any(|child| child.kind == AstKind::Variable)
    {
        return Err(QueryError::semantic(
            "map projections are owned by Phase 06",
        ));
    }
    let mut entries = BTreeMap::new();
    let mut map_entries = Vec::new();
    collect_map_entries(node, &mut map_entries);
    for entry in map_entries {
        let key = entry
            .children
            .iter()
            .find(|child| child.kind == AstKind::MapKey)
            .and_then(|child| child.text.as_deref())
            .ok_or_else(|| QueryError::semantic("map entry is missing its key"))?;
        let key = parse_map_key(key)?;
        let value = entry
            .children
            .iter()
            .find(|child| is_expression_value(child))
            .ok_or_else(|| QueryError::semantic("map entry is missing its value"))?;
        if entries
            .insert(key.clone(), compile_expression(value)?)
            .is_some()
        {
            return Err(QueryError::semantic(format!(
                "map literal contains duplicate key {key:?}"
            )));
        }
    }
    Ok(Expr::Map(entries))
}

fn collect_map_entries<'a>(node: &'a AstNode, entries: &mut Vec<&'a AstNode>) {
    for child in &node.children {
        if child.kind == AstKind::MapEntry {
            entries.push(child);
        } else if !matches!(child.kind, AstKind::Expression(ExpressionKind::Map)) {
            collect_map_entries(child, entries);
        }
    }
}

fn parse_map_key(text: &str) -> QueryResult<String> {
    let trimmed = text.trim();
    if trimmed.starts_with('\'') || trimmed.starts_with('"') {
        return parse_string(trimmed);
    }
    Ok(unescape_identifier(trimmed))
}

fn compile_only_child(node: &AstNode) -> QueryResult<Expr> {
    let child = node
        .children
        .iter()
        .find(|child| is_expression_value(child))
        .or_else(|| {
            node.descendants()
                .skip(1)
                .find(|child| is_expression_terminal(child))
        })
        .ok_or_else(|| QueryError::semantic("expression is missing an executable value"))?;
    compile_expression(child)
}

fn is_expression_value(node: &AstNode) -> bool {
    matches!(
        node.kind,
        AstKind::Expression(_) | AstKind::Variable | AstKind::Parameter | AstKind::Literal(_)
    )
}
fn is_expression_terminal(node: &AstNode) -> bool {
    matches!(
        node.kind,
        AstKind::Variable | AstKind::Parameter | AstKind::Literal(_)
    )
}

fn compile_fold(node: &AstNode, op: BinaryOp) -> QueryResult<Expr> {
    let mut operands = node
        .children
        .iter()
        .filter(|child| is_expression_value(child));
    let first = operands
        .next()
        .ok_or_else(|| QueryError::semantic("binary expression is missing its left operand"))?;
    let mut result = compile_expression(first)?;
    for operand in operands {
        result = Expr::Binary(op, Box::new(result), Box::new(compile_expression(operand)?));
    }
    Ok(result)
}

fn compile_operator_fold(node: &AstNode) -> QueryResult<Expr> {
    let mut pieces = Vec::new();
    for child in &node.children {
        if is_expression_value(child) {
            pieces.push(FoldPiece::Expr(compile_expression(child)?));
        } else if matches!(child.kind, AstKind::Operator) {
            pieces.push(FoldPiece::Op(child.text.clone().unwrap_or_default()));
        }
    }
    let mut iter = pieces.into_iter();
    let Some(FoldPiece::Expr(mut result)) = iter.next() else {
        return compile_only_child(node);
    };
    while let Some(piece) = iter.next() {
        let FoldPiece::Op(operator) = piece else {
            continue;
        };
        let Some(FoldPiece::Expr(right)) = iter.next() else {
            return Err(QueryError::semantic(
                "binary operator is missing its right operand",
            ));
        };
        let op = binary_operator(&operator)?;
        result = Expr::Binary(op, Box::new(result), Box::new(right));
    }
    Ok(result)
}

enum FoldPiece {
    Expr(Expr),
    Op(String),
}

fn compile_comparison(node: &AstNode) -> QueryResult<Expr> {
    let left_node = node
        .children
        .iter()
        .find(|child| is_expression_value(child))
        .ok_or_else(|| QueryError::semantic("comparison is missing its left operand"))?;
    let left = compile_expression(left_node)?;
    let suffixes = node
        .children
        .iter()
        .filter(|child| matches!(child.kind, AstKind::ComparisonSuffix))
        .collect::<Vec<_>>();
    if suffixes.is_empty() {
        return Ok(left);
    }
    let mut previous = left;
    let mut result = None;
    for suffix in suffixes {
        let operator = suffix
            .descendants()
            .find(|child| matches!(child.kind, AstKind::Operator))
            .and_then(|node| node.text.as_deref())
            .ok_or_else(|| QueryError::semantic("comparison is missing its operator"))?;
        let right_node = suffix
            .children
            .iter()
            .find(|child| is_expression_value(child))
            .or_else(|| {
                suffix
                    .descendants()
                    .skip(1)
                    .find(|child| is_expression_value(child))
            })
            .ok_or_else(|| QueryError::semantic("comparison is missing its right operand"))?;
        let right = compile_expression(right_node)?;
        let comparison = Expr::Binary(
            binary_operator(operator)?,
            Box::new(previous),
            Box::new(right.clone()),
        );
        result = Some(match result {
            Some(existing) => Expr::Binary(BinaryOp::And, Box::new(existing), Box::new(comparison)),
            None => comparison,
        });
        previous = right;
    }
    result.ok_or_else(|| QueryError::internal("comparison suffix lowering produced no expression"))
}

fn compile_not(node: &AstNode) -> QueryResult<Expr> {
    let mut expression = compile_only_child(node)?;
    let count = node
        .children
        .iter()
        .filter(|child| {
            matches!(child.kind, AstKind::Operator)
                && child
                    .text
                    .as_deref()
                    .is_some_and(|text| text.eq_ignore_ascii_case("NOT"))
        })
        .count();
    for _ in 0..count {
        expression = Expr::Unary(UnaryOp::Not, Box::new(expression));
    }
    Ok(expression)
}

fn compile_unary(node: &AstNode) -> QueryResult<Expr> {
    let mut expression = compile_only_child(node)?;
    let operators = node
        .children
        .iter()
        .filter(|child| matches!(child.kind, AstKind::Operator))
        .filter_map(|child| child.text.as_deref())
        .collect::<Vec<_>>();
    for operator in operators.into_iter().rev() {
        let op = match operator {
            "+" => UnaryOp::Positive,
            "-" => UnaryOp::Negative,
            _ => {
                return Err(QueryError::semantic(format!(
                    "unsupported unary operator {operator}"
                )));
            }
        };
        expression = Expr::Unary(op, Box::new(expression));
    }
    Ok(expression)
}

fn compile_postfix(node: &AstNode) -> QueryResult<Expr> {
    let postfix_nodes = || node.children.iter().skip(1).flat_map(AstNode::descendants);
    if postfix_nodes().any(|child| {
        matches!(
            child.kind,
            AstKind::Subscript
                | AstKind::LabelExpression
                | AstKind::NameExpression(_)
                | AstKind::LabelName
                | AstKind::TypePredicate
        )
    }) {
        return Err(QueryError::semantic(
            "Phase 04 does not execute subscript, slice, label-predicate, or type-predicate postfix expressions",
        ));
    }
    let base = node
        .children
        .iter()
        .find(|child| is_expression_value(child))
        .or_else(|| {
            node.children.iter().find(|child| {
                matches!(
                    child.kind,
                    AstKind::Variable | AstKind::Parameter | AstKind::Literal(_)
                )
            })
        })
        .ok_or_else(|| QueryError::semantic("postfix expression is missing its base"))?;
    let mut expression = compile_expression(base)?;
    for property in postfix_nodes().filter(|child| matches!(child.kind, AstKind::PropertyKey)) {
        expression = Expr::Property(
            Box::new(expression),
            property.text.clone().unwrap_or_default(),
        );
    }
    Ok(expression)
}

fn compile_function(node: &AstNode) -> QueryResult<Expr> {
    let name = node
        .descendants()
        .find(|child| matches!(child.kind, AstKind::FunctionName))
        .and_then(|child| child.text.clone())
        .ok_or_else(|| QueryError::semantic("function call is missing its name"))?;
    let supported = matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "elementid" | "labels" | "size" | "type"
    );
    if !supported {
        return Err(QueryError::semantic(format!(
            "Phase 04 does not execute function {name}"
        )));
    }
    if node
        .descendants()
        .any(|child| matches!(child.kind, AstKind::SetQuantifier(_)))
    {
        return Err(QueryError::semantic(
            "Phase 04 function execution does not support DISTINCT aggregate arguments",
        ));
    }
    let Some(arguments) = node
        .children
        .iter()
        .find(|child| matches!(child.kind, AstKind::ArgumentList))
    else {
        return Ok(Expr::Function(name, Vec::new()));
    };
    let mut argument_nodes = Vec::new();
    collect_immediate_expression_values(arguments, &mut argument_nodes);
    let args = argument_nodes
        .into_iter()
        .map(compile_expression)
        .collect::<QueryResult<Vec<_>>>()?;
    Ok(Expr::Function(name, args))
}

fn collect_immediate_expression_values<'a>(node: &'a AstNode, output: &mut Vec<&'a AstNode>) {
    for child in &node.children {
        if is_expression_value(child) {
            output.push(child);
        } else if !matches!(child.kind, AstKind::Subquery(_)) {
            collect_immediate_expression_values(child, output);
        }
    }
}

fn compile_literal(kind: LiteralKind, text: &str) -> QueryResult<Expr> {
    let value = match kind {
        LiteralKind::Null => Value::Null,
        LiteralKind::Boolean => Value::Boolean(text.eq_ignore_ascii_case("true")),
        LiteralKind::Integer => Value::Integer(parse_integer(text)?),
        LiteralKind::Float => Value::Float(
            text.replace('_', "")
                .parse::<f64>()
                .map_err(|_| QueryError::semantic("invalid Float literal"))?,
        ),
        LiteralKind::String => Value::String(parse_string(text)?),
    };
    Ok(Expr::Literal(value))
}

fn parse_integer(text: &str) -> QueryResult<i64> {
    let value = text.replace('_', "");
    let parsed = if let Some(value) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        i64::from_str_radix(value, 16)
    } else if let Some(value) = value
        .strip_prefix("0o")
        .or_else(|| value.strip_prefix("0O"))
    {
        i64::from_str_radix(value, 8)
    } else {
        value.parse::<i64>()
    };
    parsed.map_err(|_| QueryError::semantic("Integer literal is outside INTEGER64 range"))
}

fn parse_string(text: &str) -> QueryResult<String> {
    if text.len() < 2 {
        return Err(QueryError::semantic("invalid String literal"));
    }
    let quote = text.as_bytes()[0];
    if !matches!(quote, b'\'' | b'"') || text.as_bytes()[text.len() - 1] != quote {
        return Err(QueryError::semantic("invalid String literal"));
    }
    let mut output = String::new();
    let mut chars = text[1..text.len() - 1].chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        let escaped = chars
            .next()
            .ok_or_else(|| QueryError::semantic("unterminated String escape"))?;
        output.push(match escaped {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'b' => '\u{0008}',
            'f' => '\u{000c}',
            '\\' => '\\',
            '\'' => '\'',
            '"' => '"',
            other => other,
        });
    }
    Ok(output)
}

fn binary_operator(text: &str) -> QueryResult<BinaryOp> {
    match text.trim().to_ascii_uppercase().as_str() {
        "OR" => Ok(BinaryOp::Or),
        "XOR" => Ok(BinaryOp::Xor),
        "AND" => Ok(BinaryOp::And),
        "=" => Ok(BinaryOp::Equal),
        "<>" | "!=" => Ok(BinaryOp::NotEqual),
        "<" => Ok(BinaryOp::Less),
        "<=" => Ok(BinaryOp::LessEqual),
        ">" => Ok(BinaryOp::Greater),
        ">=" => Ok(BinaryOp::GreaterEqual),
        "+" => Ok(BinaryOp::Add),
        "-" => Ok(BinaryOp::Subtract),
        "*" => Ok(BinaryOp::Multiply),
        "/" => Ok(BinaryOp::Divide),
        "%" => Ok(BinaryOp::Modulo),
        "^" => Ok(BinaryOp::Power),
        _ => Err(QueryError::semantic(format!(
            "unsupported Phase 04 operator {text}"
        ))),
    }
}

pub(crate) fn evaluate(
    expression: &Expr,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
) -> QueryResult<Value> {
    evaluate_with_aliases(expression, snapshot, row, params, &BTreeMap::new())
}

pub(crate) fn evaluate_with_aliases(
    expression: &Expr,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
    aliases: &BTreeMap<String, Value>,
) -> QueryResult<Value> {
    match expression {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::List(items) => items
            .iter()
            .map(|item| evaluate_with_aliases(item, snapshot, row, params, aliases))
            .collect::<QueryResult<Vec<_>>>()
            .map(Value::List),
        Expr::Map(entries) => entries
            .iter()
            .map(|(key, value)| {
                Ok((
                    key.clone(),
                    evaluate_with_aliases(value, snapshot, row, params, aliases)?,
                ))
            })
            .collect::<QueryResult<BTreeMap<_, _>>>()
            .map(Value::Map),
        Expr::Variable(name) => match aliases.get(name) {
            Some(value) => Ok(value.clone()),
            None => binding_value(
                snapshot,
                row.values.get(name).unwrap_or(&BindingValue::Null),
            ),
        },
        Expr::Parameter(name) => Ok(params.get(name).cloned().unwrap_or(Value::Null)),
        Expr::Property(base, key) => evaluate_property(base, key, snapshot, row, params, aliases),
        Expr::Function(name, args) => evaluate_function(name, args, snapshot, row, params, aliases),
        Expr::Unary(op, value) => evaluate_unary(
            *op,
            evaluate_with_aliases(value, snapshot, row, params, aliases)?,
        ),
        Expr::Binary(op, left, right) => {
            let left = evaluate_with_aliases(left, snapshot, row, params, aliases)?;
            let right = evaluate_with_aliases(right, snapshot, row, params, aliases)?;
            evaluate_binary(*op, left, right)
        }
    }
}

pub(crate) fn predicate(value: Value) -> QueryResult<bool> {
    match value {
        Value::Boolean(value) => Ok(value),
        Value::Null => Ok(false),
        _ => Err(QueryError::new(
            super::QueryErrorKind::Type,
            "WHERE expression must evaluate to Boolean or null",
        )),
    }
}

fn binding_value(snapshot: &Snapshot<'_>, value: &BindingValue) -> QueryResult<Value> {
    match value {
        BindingValue::Node(id) => Ok(Value::Node(materialize_node(snapshot, *id)?)),
        BindingValue::Relationship(record) => Ok(Value::Relationship(materialize_relationship(
            snapshot, *record,
        )?)),
        BindingValue::Path {
            nodes,
            relationships,
        } => Ok(Value::Path(PathValue {
            nodes: nodes
                .iter()
                .map(|id| materialize_node(snapshot, *id))
                .collect::<QueryResult<Vec<_>>>()?,
            relationships: relationships
                .iter()
                .map(|record| materialize_relationship(snapshot, *record))
                .collect::<QueryResult<Vec<_>>>()?,
        })),
        BindingValue::Null => Ok(Value::Null),
    }
}

fn evaluate_property(
    base: &Expr,
    key: &str,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
    aliases: &BTreeMap<String, Value>,
) -> QueryResult<Value> {
    if let Expr::Variable(name) = base {
        if let Some(value) = aliases.get(name) {
            return value_property(value, key);
        }
        return match row.values.get(name) {
            Some(BindingValue::Node(id)) => node_property(snapshot, *id, key),
            Some(BindingValue::Relationship(record)) => {
                relationship_property(snapshot, record.id, key)
            }
            Some(BindingValue::Path { .. }) => Err(QueryError::new(
                super::QueryErrorKind::Type,
                "property access requires Node, Relationship, Map, or null",
            )),
            Some(BindingValue::Null) | None => Ok(Value::Null),
        };
    }
    let value = evaluate_with_aliases(base, snapshot, row, params, aliases)?;
    value_property(&value, key)
}

fn value_property(value: &Value, key: &str) -> QueryResult<Value> {
    match value {
        Value::Node(node) => Ok(node.properties.get(key).cloned().unwrap_or(Value::Null)),
        Value::Relationship(relationship) => Ok(relationship
            .properties
            .get(key)
            .cloned()
            .unwrap_or(Value::Null)),
        Value::Map(values) => Ok(values.get(key).cloned().unwrap_or(Value::Null)),
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            super::QueryErrorKind::Type,
            "property access requires Node, Relationship, Map, or null",
        )),
    }
}

fn evaluate_function(
    name: &str,
    args: &[Expr],
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
    aliases: &BTreeMap<String, Value>,
) -> QueryResult<Value> {
    let lower = name.to_ascii_lowercase();
    if lower == "elementid"
        && args.len() == 1
        && let Expr::Variable(variable) = &args[0]
    {
        if let Some(value) = aliases.get(variable) {
            return match value {
                Value::Node(node) => Ok(Value::String(node.element_id.clone())),
                Value::Relationship(relationship) => {
                    Ok(Value::String(relationship.element_id.clone()))
                }
                Value::Null => Ok(Value::Null),
                _ => Err(QueryError::new(
                    super::QueryErrorKind::Type,
                    "elementId() requires Node or Relationship",
                )),
            };
        }
        return match row.values.get(variable) {
            Some(BindingValue::Node(id)) => Ok(Value::String(format!("n:{id}"))),
            Some(BindingValue::Relationship(record)) => {
                Ok(Value::String(format!("r:{}", record.id)))
            }
            Some(BindingValue::Path { .. }) => Err(QueryError::new(
                super::QueryErrorKind::Type,
                "elementId() requires Node or Relationship",
            )),
            Some(BindingValue::Null) | None => Ok(Value::Null),
        };
    }
    let values = args
        .iter()
        .map(|arg| evaluate_with_aliases(arg, snapshot, row, params, aliases))
        .collect::<QueryResult<Vec<_>>>()?;
    match (lower.as_str(), values.as_slice()) {
        ("elementid", [Value::Node(node)]) => Ok(Value::String(node.element_id.clone())),
        ("elementid", [Value::Relationship(relationship)]) => {
            Ok(Value::String(relationship.element_id.clone()))
        }
        ("labels", [Value::Node(node)]) => Ok(Value::List(
            node.labels.iter().cloned().map(Value::String).collect(),
        )),
        ("type", [Value::Relationship(rel)]) => Ok(Value::String(rel.relationship_type.clone())),
        ("size", [Value::List(values)]) => Ok(Value::Integer(values.len() as i64)),
        ("size", [Value::String(value)]) => Ok(Value::Integer(value.chars().count() as i64)),
        ("elementid" | "labels" | "type" | "size", [Value::Null]) => Ok(Value::Null),
        ("count", _) => Err(QueryError::internal(
            "count() reached scalar expression evaluation",
        )),
        _ => Err(QueryError::semantic(format!(
            "Phase 04 does not execute function {name}"
        ))),
    }
}

fn evaluate_unary(op: UnaryOp, value: Value) -> QueryResult<Value> {
    match (op, value) {
        (UnaryOp::Not, Value::Boolean(value)) => Ok(Value::Boolean(!value)),
        (UnaryOp::Not, Value::Null) => Ok(Value::Null),
        (UnaryOp::Positive, value @ (Value::Integer(_) | Value::Float(_))) => Ok(value),
        (UnaryOp::Negative, Value::Integer(value)) => {
            value.checked_neg().map(Value::Integer).ok_or_else(|| {
                QueryError::new(super::QueryErrorKind::Type, "INTEGER64 negation overflow")
            })
        }
        (UnaryOp::Negative, Value::Float(value)) => Ok(Value::Float(-value)),
        _ => Err(QueryError::new(
            super::QueryErrorKind::Type,
            "unary operator received an incompatible value",
        )),
    }
}

fn evaluate_binary(op: BinaryOp, left: Value, right: Value) -> QueryResult<Value> {
    match op {
        BinaryOp::Or | BinaryOp::Xor | BinaryOp::And => boolean_binary(op, left, right),
        BinaryOp::Equal | BinaryOp::NotEqual => {
            let value = cypher::cypher_equals(&left, &right)?;
            Ok(match value {
                Some(value) => Value::Boolean(if op == BinaryOp::NotEqual {
                    !value
                } else {
                    value
                }),
                None => Value::Null,
            })
        }
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual => {
            ordered_binary(op, &left, &right)
        }
        BinaryOp::Add
        | BinaryOp::Subtract
        | BinaryOp::Multiply
        | BinaryOp::Divide
        | BinaryOp::Modulo
        | BinaryOp::Power => numeric_binary(op, left, right),
    }
}

fn boolean_binary(op: BinaryOp, left: Value, right: Value) -> QueryResult<Value> {
    let left = bool_or_null(left)?;
    let right = bool_or_null(right)?;
    let value = match op {
        BinaryOp::And => match (left, right) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        },
        BinaryOp::Or => match (left, right) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
        BinaryOp::Xor => match (left, right) {
            (Some(left), Some(right)) => Some(left ^ right),
            _ => None,
        },
        _ => None,
    };
    Ok(value.map(Value::Boolean).unwrap_or(Value::Null))
}

fn bool_or_null(value: Value) -> QueryResult<Option<bool>> {
    match value {
        Value::Boolean(value) => Ok(Some(value)),
        Value::Null => Ok(None),
        _ => Err(QueryError::new(
            super::QueryErrorKind::Type,
            "Boolean operator requires Boolean or null",
        )),
    }
}

fn ordered_binary(op: BinaryOp, left: &Value, right: &Value) -> QueryResult<Value> {
    let Some(comparison) = cypher::cypher_compare(left, right)? else {
        return Ok(Value::Null);
    };
    let value = match comparison {
        CypherComparison::Unordered => false,
        CypherComparison::Less => matches!(op, BinaryOp::Less | BinaryOp::LessEqual),
        CypherComparison::Equal => matches!(op, BinaryOp::LessEqual | BinaryOp::GreaterEqual),
        CypherComparison::Greater => matches!(op, BinaryOp::Greater | BinaryOp::GreaterEqual),
    };
    Ok(Value::Boolean(value))
}

fn numeric_binary(op: BinaryOp, left: Value, right: Value) -> QueryResult<Value> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }
    match (left, right) {
        (Value::Integer(left), Value::Integer(right))
            if op != BinaryOp::Divide && op != BinaryOp::Power =>
        {
            integer_binary(op, left, right)
        }
        (Value::Integer(_), Value::Integer(0)) if op == BinaryOp::Divide => Err(QueryError::new(
            super::QueryErrorKind::Type,
            "division by zero",
        )),
        (Value::Integer(left), Value::Integer(right)) => {
            float_binary(op, left as f64, right as f64)
        }
        (Value::Integer(left), Value::Float(right)) => float_binary(op, left as f64, right),
        (Value::Float(left), Value::Integer(right)) => float_binary(op, left, right as f64),
        (Value::Float(left), Value::Float(right)) => float_binary(op, left, right),
        (Value::String(left), Value::String(right)) if op == BinaryOp::Add => {
            Ok(Value::String(left + &right))
        }
        _ => Err(QueryError::new(
            super::QueryErrorKind::Type,
            "arithmetic operator received incompatible values",
        )),
    }
}

fn integer_binary(op: BinaryOp, left: i64, right: i64) -> QueryResult<Value> {
    let value = match op {
        BinaryOp::Add => left.checked_add(right),
        BinaryOp::Subtract => left.checked_sub(right),
        BinaryOp::Multiply => left.checked_mul(right),
        BinaryOp::Modulo if right != 0 => left.checked_rem(right),
        BinaryOp::Modulo => {
            return Err(QueryError::new(
                super::QueryErrorKind::Type,
                "division by zero",
            ));
        }
        _ => None,
    };
    value.map(Value::Integer).ok_or_else(|| {
        QueryError::new(super::QueryErrorKind::Type, "INTEGER64 arithmetic overflow")
    })
}

fn float_binary(op: BinaryOp, left: f64, right: f64) -> QueryResult<Value> {
    let value = match op {
        BinaryOp::Add => left + right,
        BinaryOp::Subtract => left - right,
        BinaryOp::Multiply => left * right,
        BinaryOp::Divide => left / right,
        BinaryOp::Modulo => left % right,
        BinaryOp::Power => left.powf(right),
        _ => {
            return Err(QueryError::internal(
                "non-arithmetic operator reached Float evaluation",
            ));
        }
    };
    Ok(Value::Float(value))
}

pub(crate) fn is_count(expression: &Expr) -> bool {
    matches!(expression, Expr::Function(name, _) if name.eq_ignore_ascii_case("count"))
}

pub(crate) fn count_contributes(
    expression: &Expr,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
) -> QueryResult<bool> {
    let Expr::Function(name, args) = expression else {
        return Err(QueryError::internal(
            "non-count expression reached count aggregation",
        ));
    };
    if !name.eq_ignore_ascii_case("count") {
        return Err(QueryError::internal(
            "non-count function reached count aggregation",
        ));
    }
    match args.as_slice() {
        [] => Ok(true),
        [argument] => Ok(!matches!(
            evaluate(argument, snapshot, row, params)?,
            Value::Null
        )),
        _ => Err(QueryError::semantic(
            "count() expects zero or one executable argument in Phase 04",
        )),
    }
}
