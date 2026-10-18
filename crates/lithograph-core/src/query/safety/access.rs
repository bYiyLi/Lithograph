use crate::cypher::{
    AstKind, AstNode, ClauseKind, ExecutionMode, ExpressionKind, QueryAst, SubqueryKind,
};

use super::super::{AccessPolicy, QueryError, QueryResult, access::require, registry};

pub(super) fn validate(ast: &QueryAst, policy: &AccessPolicy) -> QueryResult<()> {
    if policy.read.ast_is_unrestricted() && policy.write.ast_is_unrestricted() {
        return Ok(());
    }
    match ast.execution_mode {
        ExecutionMode::Explain => require(policy.read.allow_explain, "readAccess.allowExplain")?,
        ExecutionMode::Profile => require(policy.read.allow_profile, "readAccess.allowProfile")?,
        ExecutionMode::Execute => {}
    }
    validate_node(&ast.root, policy)
}

fn validate_node(root: &AstNode, policy: &AccessPolicy) -> QueryResult<()> {
    for node in root.descendants() {
        match node.kind {
            AstKind::Clause(ClauseKind::Call) => validate_call(node, policy)?,
            AstKind::Clause(ClauseKind::Show) => {
                require(policy.read.allow_show, "readAccess.allowShow")?
            }
            AstKind::Clause(ClauseKind::LoadCsv) => policy.check_external_io()?,
            AstKind::Clause(
                ClauseKind::CreateIndex
                | ClauseKind::DropIndex
                | ClauseKind::CreateConstraint
                | ClauseKind::DropConstraint
                | ClauseKind::GraphType,
            ) => {
                require(
                    policy.write.allow_schema_changes,
                    "writeAccess.allowSchemaChanges",
                )?;
            }
            AstKind::Expression(ExpressionKind::FunctionCall) => validate_function(node, policy)?,
            AstKind::Expression(ExpressionKind::InterpolatedString) => {
                super::validate_interpolation_with(node, |expression| {
                    validate_node(expression, policy)
                })?
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_function(node: &AstNode, policy: &AccessPolicy) -> QueryResult<()> {
    let name = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::FunctionName)
        .and_then(|child| child.text.as_deref())
        .or_else(|| keyword_function_name(node));
    if name.is_some_and(|name| {
        policy
            .read
            .deny_functions
            .contains(&name.to_ascii_lowercase())
    }) {
        return Err(QueryError::access_denied(
            "readAccess.denyFunctions forbids this function",
        ));
    }
    Ok(())
}

fn keyword_function_name(node: &AstNode) -> Option<&str> {
    // VECTOR, list predicates and reductions are formal FunctionCall nodes
    // whose grammar starts with a keyword rather than FunctionName. Inspect
    // only that node's leading keyword, never a function in its arguments.
    let source = node.text.as_deref()?;
    let length = source.bytes().take_while(u8::is_ascii_alphabetic).count();
    (length > 0).then_some(&source[..length])
}

fn validate_call(clause: &AstNode, policy: &AccessPolicy) -> QueryResult<()> {
    if clause
        .children
        .iter()
        .any(|child| child.kind == AstKind::Subquery(SubqueryKind::Call))
    {
        return Ok(());
    }
    require(
        policy.read.allow_named_procedures,
        "readAccess.allowNamedProcedures",
    )?;
    let name = clause
        .descendants()
        .find(|node| node.kind == AstKind::FunctionName)
        .and_then(|node| node.text.as_deref())
        .ok_or_else(|| QueryError::semantic("CALL is missing its procedure name"))?;
    if let Some(effects) = registry::procedure_effects(name) {
        if effects.external_io {
            policy.check_external_io()?;
        }
        if effects.schema {
            require(
                policy.write.allow_schema_changes,
                "writeAccess.allowSchemaChanges",
            )?;
        }
        if effects.version {
            require(
                policy.write.allow_version_changes,
                "writeAccess.allowVersionChanges",
            )?;
        }
        if effects.maintenance {
            require(
                policy.write.allow_maintenance,
                "writeAccess.allowMaintenance",
            )?;
        }
    }
    Ok(())
}
