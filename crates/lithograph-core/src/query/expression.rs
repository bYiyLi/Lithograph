use std::collections::{BTreeMap, BTreeSet};

use crate::cypher::{
    AstKind, AstNode, ExpressionKind, LiteralKind, Value, VectorCoordinateType, unescape_identifier,
};
use crate::storage::RelationshipRecord;

use super::{QueryError, QueryResult};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BindingValue {
    Node(i64),
    Relationship(RelationshipRecord),
    Path {
        nodes: Vec<i64>,
        relationships: Vec<RelationshipRecord>,
    },
    Scalar(Value),
    Null,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct BindingRow {
    pub values: BTreeMap<String, BindingValue>,
    pub used_relationships: BTreeSet<i64>,
    pub order: Vec<String>,
}

impl BindingRow {
    pub(crate) fn insert(&mut self, name: String, value: BindingValue) {
        if !self.values.contains_key(&name) {
            self.order.push(name.clone());
        }
        self.values.insert(name, value);
    }
}

pub(crate) fn surface_expressions(node: &AstNode) -> Vec<&AstNode> {
    fn collect<'a>(node: &'a AstNode, output: &mut Vec<&'a AstNode>) {
        for child in &node.children {
            if matches!(child.kind, AstKind::Expression(_)) {
                output.push(child);
            } else {
                collect(child, output);
            }
        }
    }

    let mut output = Vec::new();
    collect(node, &mut output);
    output.sort_by_key(|expression| expression.span.start);
    output
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Literal(Value),
    List(Vec<Expr>),
    Map(BTreeMap<String, Expr>),
    Variable(String),
    Parameter(String),
    Property(Box<Expr>, String),
    Function {
        name: String,
        args: Vec<Expr>,
        distinct: bool,
        star: bool,
    },
    Case {
        operand: Option<Box<Expr>>,
        alternatives: Vec<(Expr, Expr)>,
        fallback: Option<Box<Expr>>,
    },
    ListComprehension {
        variable: String,
        collection: Box<Expr>,
        predicate: Option<Box<Expr>>,
        projection: Option<Box<Expr>>,
    },
    ListPredicate {
        kind: ListPredicateKind,
        variable: String,
        collection: Box<Expr>,
        predicate: Option<Box<Expr>>,
    },
    Reduce {
        accumulator: String,
        initial: Box<Expr>,
        variable: String,
        collection: Box<Expr>,
        reduction: Box<Expr>,
    },
    AllReduce {
        accumulator: String,
        initial: Box<Expr>,
        variable: String,
        collection: Box<Expr>,
        reduction: Box<Expr>,
        predicate: Box<Expr>,
    },
    MapProjection {
        base: String,
        include_all: bool,
        entries: Vec<(String, Expr)>,
    },
    Subscript {
        base: Box<Expr>,
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
        slice: bool,
    },
    IsNull {
        value: Box<Expr>,
        negated: bool,
    },
    NormalizedPredicate {
        value: Box<Expr>,
        form: NormalizationForm,
        negated: bool,
    },
    TypePredicate {
        value: Box<Expr>,
        type_spec: TypeSpec,
        negated: bool,
    },
    LabelPredicate {
        value: Box<Expr>,
        name_expression: AstNode,
        negated: bool,
    },
    PatternPredicate(AstNode),
    PatternComprehension(AstNode),
    Interpolated(Vec<InterpolatedPart>),
    Subquery {
        kind: crate::cypher::SubqueryKind,
        node: AstNode,
    },
    Unary(UnaryOp, Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TypeSpec {
    terms: Vec<TypeTermSpec>,
}

#[derive(Debug, Clone, PartialEq)]
struct TypeTermSpec {
    kind: TypeKind,
    nullable: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum TypeKind {
    Named(String),
    List(Box<TypeSpec>),
    Vector {
        coordinate_type: Option<VectorCoordinateType>,
        dimension: Option<usize>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum InterpolatedPart {
    Text(String),
    Expression(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NormalizationForm {
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListPredicateKind {
    All,
    Any,
    None,
    Single,
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
    In,
    StartsWith,
    EndsWith,
    Contains,
    Regex,
    Add,
    Concat,
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
        AstKind::Subquery(kind) => Ok(Expr::Subquery {
            kind: *kind,
            node: node.clone(),
        }),
        AstKind::Pattern => Ok(Expr::PatternPredicate(node.clone())),
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
        ExpressionKind::Case => compile_case(node),
        ExpressionKind::InterpolatedString => compile_interpolated(node),
        ExpressionKind::Subquery => node
            .descendants()
            .find(|child| matches!(child.kind, AstKind::Subquery(_)))
            .ok_or_else(|| QueryError::semantic("subquery expression is missing its body"))
            .and_then(compile_expression),
    }
}

fn compile_list(node: &AstNode) -> QueryResult<Expr> {
    if node
        .descendants()
        .any(|child| matches!(child.kind, AstKind::Pattern))
    {
        return Ok(Expr::PatternComprehension(node.clone()));
    }
    if let Some(variable) = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::BindingVariable)
        .and_then(|child| child.text.clone())
    {
        let expressions = node
            .children
            .iter()
            .filter(|child| matches!(child.kind, AstKind::Expression(_)))
            .collect::<Vec<_>>();
        let collection = expressions
            .first()
            .ok_or_else(|| QueryError::semantic("list comprehension is missing IN input"))?;
        let has_projection = node.text.as_deref().is_some_and(|text| text.contains('|'));
        let (predicate, projection) = match expressions.as_slice() {
            [_] => (None, None),
            [_, second] if has_projection => (None, Some(*second)),
            [_, second] => (Some(*second), None),
            [_, second, third] => (Some(*second), Some(*third)),
            _ => {
                return Err(QueryError::semantic(
                    "list comprehension has an invalid expression shape",
                ));
            }
        };
        return Ok(Expr::ListComprehension {
            variable,
            collection: Box::new(compile_expression(collection)?),
            predicate: predicate.map(compile_expression).transpose()?.map(Box::new),
            projection: projection
                .map(compile_expression)
                .transpose()?
                .map(Box::new),
        });
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
    if let Some(base) = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::Variable)
        .and_then(|child| child.text.clone())
    {
        return compile_map_projection(node, base);
    }
    compile_map_literal(node)
}

fn compile_map_projection(node: &AstNode, base: String) -> QueryResult<Expr> {
    let mut include_all = false;
    let mut entries = Vec::new();
    for item in node
        .children
        .iter()
        .filter(|child| child.kind == AstKind::Syntax)
    {
        let text = item.text.as_deref().unwrap_or_default().trim();
        if text == ".*" {
            include_all = true;
            continue;
        }
        if let Some(entry) = compile_map_projection_item(item, &base)? {
            entries.push(entry);
        }
    }
    Ok(Expr::MapProjection {
        base,
        include_all,
        entries,
    })
}

fn compile_map_projection_item(item: &AstNode, base: &str) -> QueryResult<Option<(String, Expr)>> {
    if let Some(property) = item
        .descendants()
        .find(|child| child.kind == AstKind::PropertyKey)
        .and_then(|child| child.text.clone())
    {
        let expression =
            Expr::Property(Box::new(Expr::Variable(base.to_owned())), property.clone());
        return Ok(Some((property, expression)));
    }
    if let Some(key) = item
        .descendants()
        .find(|child| child.kind == AstKind::MapKey)
        .and_then(|child| child.text.as_deref())
    {
        let value = item
            .children
            .iter()
            .find(|child| is_expression_value(child))
            .or_else(|| {
                item.descendants()
                    .skip(1)
                    .find(|child| is_expression_value(child))
            })
            .ok_or_else(|| QueryError::semantic("map projection item is missing its expression"))?;
        return Ok(Some((parse_map_key(key)?, compile_expression(value)?)));
    }
    Ok(item
        .children
        .iter()
        .find(|child| child.kind == AstKind::Variable)
        .and_then(|child| child.text.clone())
        .map(|variable| (variable.clone(), Expr::Variable(variable))))
}

fn compile_map_literal(node: &AstNode) -> QueryResult<Expr> {
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

fn compile_case(node: &AstNode) -> QueryResult<Expr> {
    if let Some(inner) = node
        .children
        .iter()
        .find(|child| matches!(child.kind, AstKind::Expression(ExpressionKind::Case)))
    {
        return compile_case(inner);
    }
    let alternatives = node
        .children
        .iter()
        .filter(|child| child.kind == AstKind::CaseAlternative)
        .map(|alternative| {
            let expressions = alternative
                .children
                .iter()
                .filter(|child| is_expression_value(child))
                .collect::<Vec<_>>();
            match expressions.as_slice() {
                [condition, result] => {
                    Ok((compile_expression(condition)?, compile_expression(result)?))
                }
                _ => Err(QueryError::semantic(
                    "CASE alternative must contain WHEN and THEN expressions",
                )),
            }
        })
        .collect::<QueryResult<Vec<_>>>()?;
    let direct = node
        .children
        .iter()
        .filter(|child| is_expression_value(child))
        .collect::<Vec<_>>();
    let first_alternative = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::CaseAlternative)
        .map_or(usize::MAX, |child| child.span.start);
    let last_alternative = node
        .children
        .iter()
        .filter(|child| child.kind == AstKind::CaseAlternative)
        .map(|child| child.span.end)
        .max()
        .unwrap_or(0);
    let operand = direct
        .iter()
        .find(|value| value.span.start < first_alternative)
        .map(|value| compile_expression(value))
        .transpose()?;
    let fallback = direct
        .iter()
        .find(|value| value.span.start >= last_alternative)
        .map(|value| compile_expression(value))
        .transpose()?;
    Ok(Expr::Case {
        operand: operand.map(Box::new),
        alternatives,
        fallback: fallback.map(Box::new),
    })
}

fn compile_interpolated(node: &AstNode) -> QueryResult<Expr> {
    let raw = node
        .text
        .as_deref()
        .ok_or_else(|| QueryError::semantic("interpolated String is missing its source text"))?;
    let fragments = crate::cypher::interpolation_fragments(raw).map_err(QueryError::semantic)?;
    let mut parts = Vec::new();
    let mut cursor = 2_usize;
    for fragment in fragments {
        let open = raw[..fragment.offset]
            .rfind('{')
            .ok_or_else(|| QueryError::semantic("interpolation fragment is missing '{'"))?;
        if open > cursor {
            parts.push(InterpolatedPart::Text(unescape_interpolation_text(
                &raw[cursor..open],
            )?));
        }
        let parsed = crate::cypher::parse_expression_fragment_at(fragment.text, fragment.text, 0)?;
        parts.push(InterpolatedPart::Expression(compile_expression(&parsed)?));
        cursor = fragment.end.saturating_add(1);
    }
    let end = raw.len().saturating_sub(1);
    if cursor < end {
        parts.push(InterpolatedPart::Text(unescape_interpolation_text(
            &raw[cursor..end],
        )?));
    }
    Ok(Expr::Interpolated(parts))
}

fn unescape_interpolation_text(text: &str) -> QueryResult<String> {
    let quoted = format!("\"{text}\"");
    parse_string(&quoted)
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
        AstKind::Expression(_)
            | AstKind::Subquery(_)
            | AstKind::Pattern
            | AstKind::Variable
            | AstKind::Parameter
            | AstKind::Literal(_)
    )
}
fn is_expression_terminal(node: &AstNode) -> bool {
    matches!(
        node.kind,
        AstKind::Pattern | AstKind::Variable | AstKind::Parameter | AstKind::Literal(_)
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
        let comparison = compile_comparison_suffix(suffix, &mut previous)?;
        result = Some(conjoin_expression(result, comparison));
    }
    result.ok_or_else(|| QueryError::internal("comparison suffix lowering produced no expression"))
}

fn compile_comparison_suffix(suffix: &AstNode, previous: &mut Expr) -> QueryResult<Expr> {
    let operator = suffix
        .descendants()
        .find(|child| matches!(child.kind, AstKind::Operator))
        .and_then(|node| node.text.as_deref())
        .ok_or_else(|| QueryError::semantic("comparison is missing its operator"))?;
    let suffix_text = suffix
        .text
        .as_deref()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if operator.eq_ignore_ascii_case("IS") {
        return compile_is_predicate(suffix, previous, &suffix_text);
    }
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
        Box::new(previous.clone()),
        Box::new(right.clone()),
    );
    *previous = right;
    Ok(comparison)
}

fn conjoin_expression(existing: Option<Expr>, expression: Expr) -> Expr {
    existing.map_or(expression.clone(), |left| {
        Expr::Binary(BinaryOp::And, Box::new(left), Box::new(expression))
    })
}

fn compile_is_predicate(suffix: &AstNode, value: &Expr, text: &str) -> QueryResult<Expr> {
    let negated = text.contains("IS NOT");
    if let Some(type_expression) = suffix
        .descendants()
        .find(|child| child.kind == AstKind::TypeExpression)
    {
        return Ok(Expr::TypePredicate {
            value: Box::new(value.clone()),
            type_spec: compile_type_spec(type_expression)?,
            negated,
        });
    }
    if let Some(name_expression) = suffix
        .descendants()
        .find(|child| matches!(child.kind, AstKind::NameExpression(_)))
    {
        return Ok(Expr::LabelPredicate {
            value: Box::new(value.clone()),
            name_expression: name_expression.clone(),
            negated,
        });
    }
    if text.contains("NORMALIZED") {
        let form = if text.contains("NFKC") {
            NormalizationForm::Nfkc
        } else if text.contains("NFKD") {
            NormalizationForm::Nfkd
        } else if text.contains("NFD") {
            NormalizationForm::Nfd
        } else {
            NormalizationForm::Nfc
        };
        return Ok(Expr::NormalizedPredicate {
            value: Box::new(value.clone()),
            form,
            negated,
        });
    }
    let is_null = suffix
        .descendants()
        .any(|child| matches!(child.kind, AstKind::Literal(LiteralKind::Null)));
    if !is_null && !text.contains("NULL") {
        return Err(QueryError::semantic("IS predicate is missing its target"));
    }
    Ok(Expr::IsNull {
        value: Box::new(value.clone()),
        negated,
    })
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
    for operator in node.children.iter().skip(1) {
        if let Some(subscript) = operator
            .descendants()
            .find(|child| child.kind == AstKind::Subscript)
        {
            expression = compile_subscript(expression, subscript)?;
            continue;
        }
        if let Some(type_expression) = operator
            .descendants()
            .find(|child| child.kind == AstKind::TypeExpression)
        {
            expression = Expr::TypePredicate {
                value: Box::new(expression),
                type_spec: compile_type_spec(type_expression)?,
                negated: false,
            };
            continue;
        }
        if let Some(name_expression) = operator
            .descendants()
            .find(|child| matches!(child.kind, AstKind::NameExpression(_)))
        {
            expression = Expr::LabelPredicate {
                value: Box::new(expression),
                name_expression: name_expression.clone(),
                negated: false,
            };
            continue;
        }
        if let Some(property) = operator
            .descendants()
            .find(|child| child.kind == AstKind::PropertyKey)
        {
            expression = Expr::Property(
                Box::new(expression),
                property.text.clone().unwrap_or_default(),
            );
        }
    }
    Ok(expression)
}

fn compile_subscript(base: Expr, subscript: &AstNode) -> QueryResult<Expr> {
    let mut values = Vec::new();
    collect_immediate_expression_values(subscript, &mut values);
    let slice = subscript
        .text
        .as_deref()
        .is_some_and(|text| text.contains(".."));
    let (start, end) = if slice {
        let text = subscript.text.as_deref().unwrap_or_default();
        let separator = text.find("..").unwrap_or(0);
        let start = values
            .iter()
            .find(|value| value.span.start < subscript.span.start + separator);
        let end = values
            .iter()
            .find(|value| value.span.start > subscript.span.start + separator);
        (start.copied(), end.copied())
    } else {
        (values.first().copied(), None)
    };
    Ok(Expr::Subscript {
        base: Box::new(base),
        start: start.map(compile_expression).transpose()?.map(Box::new),
        end: end.map(compile_expression).transpose()?.map(Box::new),
        slice,
    })
}

fn compile_function(node: &AstNode) -> QueryResult<Expr> {
    let name = function_name(node)?;
    if is_gql_trim(node, &name) {
        return compile_gql_trim(node);
    }
    if name.eq_ignore_ascii_case("reduce") || name.eq_ignore_ascii_case("allReduce") {
        return compile_reduction(node, &name);
    }
    if !super::registry::is_function(&name) {
        return Err(QueryError::semantic(format!(
            "unknown current-graph function {name}"
        )));
    }
    if let Some(variable) = predicate_variable(node) {
        return compile_list_predicate(node, &name, variable);
    }
    if is_list_predicate_name(&name) {
        return Err(QueryError::semantic(format!(
            "{name}() requires variable IN list WHERE predicate syntax"
        )));
    }
    let distinct = node
        .descendants()
        .any(|child| matches!(child.kind, AstKind::SetQuantifier(_)));
    let star = node
        .children
        .iter()
        .flat_map(AstNode::descendants)
        .any(|child| child.kind == AstKind::StarProjection);
    let mut args = compile_function_arguments(node)?;
    if name.eq_ignore_ascii_case("exists")
        && args.len() == 1
        && matches!(args.first(), Some(Expr::PatternPredicate(_)))
    {
        return Ok(args.remove(0));
    }
    append_property_exists_key(&name, node, &mut args);
    Ok(Expr::Function {
        name,
        args,
        distinct,
        star,
    })
}

fn function_name(node: &AstNode) -> QueryResult<String> {
    node.descendants()
        .find(|child| matches!(child.kind, AstKind::FunctionName))
        .and_then(|child| child.text.clone())
        .or_else(|| {
            node.text
                .as_deref()
                .and_then(|text| text.split_once('(').map(|(name, _)| name.to_owned()))
        })
        .ok_or_else(|| QueryError::semantic("function call is missing its name"))
}

fn is_gql_trim(node: &AstNode, name: &str) -> bool {
    name.eq_ignore_ascii_case("trim")
        && node
            .children
            .iter()
            .any(|child| child.kind == AstKind::TrimFromArguments)
}

fn compile_gql_trim(node: &AstNode) -> QueryResult<Expr> {
    let mut expressions = Vec::new();
    collect_immediate_expression_values(node, &mut expressions);
    let mut args = expressions
        .into_iter()
        .map(compile_expression)
        .collect::<QueryResult<Vec<_>>>()?;
    if args.len() == 2 {
        args.swap(0, 1);
    }
    let name = match node
        .descendants()
        .find(|child| child.kind == AstKind::TrimSpecification)
        .and_then(|child| child.text.as_deref())
        .map(str::to_ascii_uppercase)
        .as_deref()
    {
        Some("LEADING") => "ltrim",
        Some("TRAILING") => "rtrim",
        _ => "trim",
    };
    Ok(Expr::Function {
        name: name.to_owned(),
        args,
        distinct: false,
        star: false,
    })
}

fn predicate_variable(node: &AstNode) -> Option<String> {
    node.children
        .iter()
        .find(|child| child.kind == AstKind::PredicateVariable)
        .and_then(|child| child.text.clone())
}

fn is_list_predicate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "all" | "any" | "none" | "single"
    )
}

fn compile_list_predicate(node: &AstNode, name: &str, variable: String) -> QueryResult<Expr> {
    let expressions = node
        .children
        .iter()
        .filter(|child| matches!(child.kind, AstKind::Expression(_)))
        .collect::<Vec<_>>();
    let collection = expressions
        .first()
        .ok_or_else(|| QueryError::semantic("list predicate is missing IN input"))?;
    let predicate = expressions
        .get(1)
        .map(|value| compile_expression(value))
        .transpose()?;
    let kind = match name.to_ascii_lowercase().as_str() {
        "all" => ListPredicateKind::All,
        "any" => ListPredicateKind::Any,
        "none" => ListPredicateKind::None,
        "single" => ListPredicateKind::Single,
        _ => return Err(QueryError::internal("invalid list-predicate function name")),
    };
    Ok(Expr::ListPredicate {
        kind,
        variable,
        collection: Box::new(compile_expression(collection)?),
        predicate: predicate.map(Box::new),
    })
}

fn compile_function_arguments(node: &AstNode) -> QueryResult<Vec<Expr>> {
    if let Some(arguments) = node
        .children
        .iter()
        .find(|child| matches!(child.kind, AstKind::ArgumentList))
    {
        let mut argument_nodes = Vec::new();
        collect_immediate_expression_values(arguments, &mut argument_nodes);
        return argument_nodes
            .into_iter()
            .map(compile_expression)
            .collect::<QueryResult<_>>();
    }
    let mut args = node
        .children
        .iter()
        .filter(|child| matches!(child.kind, AstKind::Expression(_)))
        .map(compile_expression)
        .collect::<QueryResult<Vec<_>>>()?;
    for kind in [
        AstKind::VectorCoordinateTypeName,
        AstKind::VectorDistanceMetric,
        AstKind::NormalizationForm,
    ] {
        if let Some(value) = node
            .children
            .iter()
            .find(|child| child.kind == kind)
            .and_then(|child| child.text.clone())
        {
            args.push(Expr::Literal(Value::String(value)));
        }
    }
    Ok(args)
}

fn compile_reduction(node: &AstNode, name: &str) -> QueryResult<Expr> {
    let (accumulator, variable) = reduction_bindings(node)?;
    let expressions = node
        .children
        .iter()
        .filter(|child| matches!(child.kind, AstKind::Expression(_)))
        .collect::<Vec<_>>();
    if name.eq_ignore_ascii_case("allReduce") {
        compile_all_reduction(accumulator, variable, &expressions)
    } else {
        compile_basic_reduction(accumulator, variable, &expressions)
    }
}

fn reduction_bindings(node: &AstNode) -> QueryResult<(String, String)> {
    let accumulator = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::ReductionAccumulator)
        .and_then(|child| child.text.clone())
        .ok_or_else(|| QueryError::semantic("reduction function has no accumulator variable"))?;
    let variable = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::ReductionVariable)
        .and_then(|child| child.text.clone())
        .ok_or_else(|| QueryError::semantic("reduction function has no step variable"))?;
    Ok((accumulator, variable))
}

fn compile_basic_reduction(
    accumulator: String,
    variable: String,
    expressions: &[&AstNode],
) -> QueryResult<Expr> {
    if expressions.len() != 3 {
        return Err(QueryError::semantic(
            "reduce() has an invalid expression shape",
        ));
    }
    let initial = expressions
        .first()
        .ok_or_else(|| QueryError::semantic("reduction function is missing its initial value"))?;
    let collection = expressions
        .get(1)
        .ok_or_else(|| QueryError::semantic("reduction function is missing its list"))?;
    let reduction = expressions
        .get(2)
        .ok_or_else(|| QueryError::semantic("reduction function is missing its expression"))?;
    Ok(Expr::Reduce {
        accumulator,
        initial: Box::new(compile_expression(initial)?),
        variable,
        collection: Box::new(compile_expression(collection)?),
        reduction: Box::new(compile_expression(reduction)?),
    })
}

fn compile_all_reduction(
    accumulator: String,
    variable: String,
    expressions: &[&AstNode],
) -> QueryResult<Expr> {
    if expressions.len() != 4 {
        return Err(QueryError::semantic(
            "allReduce() has an invalid expression shape",
        ));
    }
    let [initial, collection, reduction, predicate] = expressions else {
        return Err(QueryError::internal(
            "validated allReduce expression shape changed",
        ));
    };
    Ok(Expr::AllReduce {
        accumulator,
        initial: Box::new(compile_expression(initial)?),
        variable,
        collection: Box::new(compile_expression(collection)?),
        reduction: Box::new(compile_expression(reduction)?),
        predicate: Box::new(compile_expression(predicate)?),
    })
}

fn append_property_exists_key(name: &str, node: &AstNode, args: &mut Vec<Expr>) {
    if name.eq_ignore_ascii_case("property_exists")
        && let Some(property) = node
            .descendants()
            .find(|child| child.kind == AstKind::PropertyKey)
            .and_then(|child| child.text.clone())
    {
        args.push(Expr::Literal(Value::String(property)));
    }
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

fn compile_type_spec(node: &AstNode) -> QueryResult<TypeSpec> {
    let terms = if node.kind == AstKind::TypeTerm {
        vec![compile_type_term(node)?]
    } else {
        node.children
            .iter()
            .filter(|child| child.kind == AstKind::TypeTerm)
            .map(compile_type_term)
            .collect::<QueryResult<Vec<_>>>()?
    };
    if terms.is_empty() {
        return Err(QueryError::semantic("type predicate is missing its type"));
    }
    if terms.len() > 1 && terms.iter().any(|term| term.nullable != terms[0].nullable) {
        return Err(QueryError::semantic(
            "union type members must use the same nullability",
        ));
    }
    Ok(TypeSpec { terms })
}

fn compile_type_term(node: &AstNode) -> QueryResult<TypeTermSpec> {
    let type_node = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::TypeName)
        .ok_or_else(|| QueryError::semantic("type term is missing its base type"))?;
    let name = type_node
        .text
        .as_deref()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let kind = if matches!(name.as_str(), "LIST" | "ARRAY") {
        let parameters = node
            .children
            .iter()
            .find(|child| child.kind == AstKind::TypeParameters)
            .and_then(|parameters| {
                parameters
                    .children
                    .iter()
                    .find(|child| child.kind == AstKind::TypeExpression)
            })
            .ok_or_else(|| QueryError::semantic("LIST type requires an inner type"))?;
        TypeKind::List(Box::new(compile_type_spec(parameters)?))
    } else if name == "VECTOR" {
        let coordinate_type = type_node
            .descendants()
            .find(|child| child.kind == AstKind::VectorCoordinateTypeName)
            .and_then(|child| child.text.as_deref())
            .map(|name| {
                VectorCoordinateType::parse(name).ok_or_else(|| {
                    QueryError::semantic("VECTOR type has an unsupported coordinate type")
                })
            })
            .transpose()?;
        let dimension = type_node
            .descendants()
            .find(|child| child.kind == AstKind::VectorDimension)
            .and_then(|child| child.text.as_deref())
            .map(|value| {
                value
                    .parse::<usize>()
                    .map_err(|_| QueryError::semantic("VECTOR dimension is outside range"))
            })
            .transpose()?;
        TypeKind::Vector {
            coordinate_type,
            dimension,
        }
    } else {
        TypeKind::Named(name)
    };
    let mut term = TypeTermSpec {
        kind,
        nullable: true,
    };
    for child in node
        .children
        .iter()
        .skip_while(|child| *child != type_node)
        .skip(1)
    {
        match child.kind {
            AstKind::TypeNotNull => term.nullable = false,
            AstKind::TypeListSuffix => {
                term = TypeTermSpec {
                    kind: TypeKind::List(Box::new(TypeSpec { terms: vec![term] })),
                    nullable: true,
                };
            }
            _ => {}
        }
    }
    Ok(term)
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
        if escaped == 'u' {
            let mut codepoint = 0_u32;
            for _ in 0..4 {
                let digit = chars
                    .next()
                    .and_then(|digit| digit.to_digit(16))
                    .ok_or_else(|| QueryError::semantic("invalid Unicode String escape"))?;
                codepoint = (codepoint << 4) | digit;
            }
            let character = char::from_u32(codepoint)
                .ok_or_else(|| QueryError::semantic("invalid Unicode String escape"))?;
            output.push(character);
            continue;
        }
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
        "IN" => Ok(BinaryOp::In),
        "STARTS" => Ok(BinaryOp::StartsWith),
        "ENDS" => Ok(BinaryOp::EndsWith),
        "CONTAINS" => Ok(BinaryOp::Contains),
        "=~" => Ok(BinaryOp::Regex),
        "+" => Ok(BinaryOp::Add),
        "||" => Ok(BinaryOp::Concat),
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

mod evaluate;
mod operators;
mod properties;
mod transform;

pub(crate) use evaluate::{
    binding_from_value, binding_value, count_contributes, evaluate, evaluate_with_aliases,
    is_count, predicate, value_to_string,
};
pub(crate) use transform::transform_expression;
