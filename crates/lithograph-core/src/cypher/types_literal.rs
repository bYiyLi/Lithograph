use super::ast::{AstKind, AstNode, LiteralKind};
use super::error::{FrontendError, Span};
use super::types::{CypherType, type_error};

pub(super) fn infer_literal(
    node: &AstNode,
    kind: LiteralKind,
    source: &str,
) -> Result<CypherType, FrontendError> {
    match kind {
        LiteralKind::Null => Ok(CypherType::Null),
        LiteralKind::Boolean => Ok(CypherType::Boolean),
        LiteralKind::Integer => {
            validate_integer_literal(node.text.as_deref().unwrap_or_default(), node.span, source)?;
            Ok(CypherType::Integer)
        }
        LiteralKind::Float => {
            let text = node.text.as_deref().unwrap_or_default();
            let value = text.parse::<f64>().map_err(|_| {
                type_error(
                    source,
                    node.span,
                    "Float literal is not a valid binary64 value",
                )
            })?;
            if !value.is_finite() {
                return Err(type_error(
                    source,
                    node.span,
                    "Float literal exceeds finite binary64 range",
                ));
            }
            Ok(CypherType::Float)
        }
        LiteralKind::String => {
            validate_string_literal(node.text.as_deref().unwrap_or_default(), node.span, source)?;
            Ok(CypherType::String)
        }
    }
}

fn validate_integer_literal(text: &str, span: Span, source: &str) -> Result<(), FrontendError> {
    let value = parse_integer_i128(text)
        .ok_or_else(|| type_error(source, span, "Integer literal is outside INTEGER64 range"))?;
    if value < i128::from(i64::MIN) || value > i128::from(i64::MAX) {
        return Err(type_error(
            source,
            span,
            "Integer literal is outside INTEGER64 range",
        ));
    }
    Ok(())
}

pub(super) fn is_i64_min_unary_expression(node: &AstNode) -> bool {
    let direct_operators = node
        .children
        .iter()
        .filter(|child| child.kind == AstKind::Operator)
        .filter_map(|child| child.text.as_deref())
        .collect::<Vec<_>>();
    if direct_operators.is_empty()
        || direct_operators
            .iter()
            .any(|operator| !matches!(*operator, "+" | "-"))
        || direct_operators
            .iter()
            .filter(|operator| **operator == "-")
            .count()
            % 2
            == 0
    {
        return false;
    }

    let mut literals = node
        .descendants()
        .filter(|child| matches!(child.kind, AstKind::Literal(LiteralKind::Integer)));
    let Some(literal) = literals.next() else {
        return false;
    };
    literals.next().is_none()
        && integer_magnitude(literal.text.as_deref().unwrap_or_default())
            == Some(i128::from(i64::MAX) + 1)
}

fn integer_magnitude(text: &str) -> Option<i128> {
    integer_sign_and_magnitude(text).map(|(_, magnitude)| magnitude)
}

pub(super) fn parse_integer_i128(text: &str) -> Option<i128> {
    integer_sign_and_magnitude(text).map(
        |(negative, magnitude)| {
            if negative { -magnitude } else { magnitude }
        },
    )
}

fn integer_sign_and_magnitude(text: &str) -> Option<(bool, i128)> {
    let (negative, unsigned) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (radix, digits) = if let Some(value) = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
    {
        (16, value)
    } else if let Some(value) = unsigned
        .strip_prefix("0o")
        .or_else(|| unsigned.strip_prefix("0O"))
    {
        (8, value)
    } else {
        (10, unsigned)
    };
    i128::from_str_radix(digits, radix)
        .ok()
        .map(|magnitude| (negative, magnitude))
}

fn validate_string_literal(text: &str, span: Span, source: &str) -> Result<(), FrontendError> {
    if text.len() < 2 {
        return Err(type_error(source, span, "String literal is malformed"));
    }
    let quote = text.as_bytes()[0];
    let bytes = text.as_bytes();
    let mut index = 1_usize;
    while index + 1 < bytes.len() {
        if bytes[index] == quote && bytes.get(index + 1) == Some(&quote) {
            index += 2;
            continue;
        }
        if bytes[index] != b'\\' {
            index += 1;
            continue;
        }
        index += 1;
        let escape = *bytes.get(index).ok_or_else(|| {
            type_error(
                source,
                span,
                "String literal has a truncated escape sequence",
            )
        })?;
        if escape == b'u' {
            let end = index + 5;
            if end > bytes.len() - 1 || !bytes[index + 1..end].iter().all(u8::is_ascii_hexdigit) {
                return Err(type_error(
                    source,
                    span,
                    "Unicode escape must contain four hexadecimal digits",
                ));
            }
            index = end;
            continue;
        }
        if !matches!(
            escape,
            b'\\' | b'\'' | b'"' | b'b' | b'f' | b'n' | b'r' | b't'
        ) {
            return Err(type_error(
                source,
                span,
                "String literal contains an unsupported escape sequence",
            ));
        }
        index += 1;
    }
    Ok(())
}
