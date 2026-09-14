use super::*;

pub(super) fn compile_power(node: &AstNode) -> QueryResult<Expr> {
    let mut operands = node
        .children
        .iter()
        .filter(|child| is_expression_value(child))
        .map(compile_expression)
        .collect::<QueryResult<Vec<_>>>()?;
    let Some(mut result) = operands.pop() else {
        return Err(QueryError::semantic(
            "power expression is missing its operand",
        ));
    };
    while let Some(left) = operands.pop() {
        result = Expr::Binary(BinaryOp::Power, Box::new(left), Box::new(result));
    }
    Ok(result)
}

pub(super) fn compile_comparison(node: &AstNode) -> QueryResult<Expr> {
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
    let mut current = left;
    let mut pending_comparison: Option<(Expr, BinaryOp)> = None;
    let mut chain = None;
    for suffix in suffixes {
        if comparison_suffix_is_postfix_predicate(suffix) {
            current = compile_postfix_comparison_suffix(suffix, &current)?;
            continue;
        }
        let operator = binary_operator(comparison_suffix_operator(suffix)?)?;
        let right = compile_expression(comparison_suffix_right_operand(suffix)?)?;
        if comparison_operator_binds_tighter_than_chain(operator) {
            current = Expr::Binary(operator, Box::new(current), Box::new(right));
            continue;
        }
        if let Some((left, pending_operator)) = pending_comparison.take() {
            let comparison =
                Expr::Binary(pending_operator, Box::new(left), Box::new(current.clone()));
            chain = Some(conjoin_expression(chain, comparison));
        }
        pending_comparison = Some((current.clone(), operator));
        current = right;
    }
    if let Some((left, operator)) = pending_comparison {
        let comparison = Expr::Binary(operator, Box::new(left), Box::new(current));
        return Ok(conjoin_expression(chain, comparison));
    }
    Ok(chain.unwrap_or(current))
}

fn comparison_operator_binds_tighter_than_chain(operator: BinaryOp) -> bool {
    matches!(
        operator,
        BinaryOp::In
            | BinaryOp::StartsWith
            | BinaryOp::EndsWith
            | BinaryOp::Contains
            | BinaryOp::Regex
    )
}

fn comparison_suffix_operator(suffix: &AstNode) -> QueryResult<&str> {
    suffix
        .descendants()
        .find(|child| matches!(child.kind, AstKind::Operator))
        .and_then(|node| node.text.as_deref())
        .ok_or_else(|| QueryError::semantic("comparison is missing its operator"))
}

fn comparison_suffix_is_postfix_predicate(suffix: &AstNode) -> bool {
    comparison_suffix_operator(suffix).is_ok_and(|operator| operator.eq_ignore_ascii_case("IS"))
}

fn comparison_suffix_right_operand(suffix: &AstNode) -> QueryResult<&AstNode> {
    suffix
        .children
        .iter()
        .find(|child| is_expression_value(child))
        .or_else(|| {
            suffix
                .descendants()
                .skip(1)
                .find(|child| is_expression_value(child))
        })
        .ok_or_else(|| QueryError::semantic("comparison is missing its right operand"))
}

fn compile_postfix_comparison_suffix(suffix: &AstNode, value: &Expr) -> QueryResult<Expr> {
    let suffix_text = suffix
        .text
        .as_deref()
        .unwrap_or_default()
        .to_ascii_uppercase();
    compile_is_predicate(suffix, value, &suffix_text)
}

fn conjoin_expression(existing: Option<Expr>, expression: Expr) -> Expr {
    existing.map_or(expression.clone(), |left| {
        Expr::Binary(BinaryOp::And, Box::new(left), Box::new(expression))
    })
}
