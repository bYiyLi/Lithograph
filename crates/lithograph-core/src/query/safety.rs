//! Query-local read-only and external-I/O boundary, checked before planning or execution.

use crate::cypher::{AstKind, AstNode, ClauseKind, ExpressionKind, QueryAst, SubqueryKind};

use super::{ExecutionOptions, QueryError, QueryResult};

mod access;

pub(super) fn validate(ast: &QueryAst, options: &ExecutionOptions) -> QueryResult<()> {
    if options.read_only_no_external_io {
        validate_node(&ast.root)?;
    }
    access::validate(ast, &options.access)
}

fn validate_node(root: &AstNode) -> QueryResult<()> {
    for node in root.descendants() {
        match node.kind {
            AstKind::Clause(ClauseKind::Call) => validate_call(node)?,
            AstKind::Clause(ClauseKind::LoadCsv) => return forbidden("LOAD CSV"),
            AstKind::Clause(
                ClauseKind::Match
                | ClauseKind::OptionalMatch
                | ClauseKind::Filter
                | ClauseKind::Return
                | ClauseKind::With
                | ClauseKind::Let
                | ClauseKind::Unwind
                | ClauseKind::For
                | ClauseKind::Finish
                | ClauseKind::Show,
            ) => {}
            AstKind::Clause(kind) => return forbidden(&format!("mutating clause {kind:?}")),
            AstKind::TransactionSubclause => return forbidden("transaction subquery"),
            AstKind::Expression(ExpressionKind::InterpolatedString) => {
                validate_interpolation_with(node, validate_node)?;
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn validate_interpolation_with(
    node: &AstNode,
    mut validate: impl FnMut(&AstNode) -> QueryResult<()>,
) -> QueryResult<()> {
    // The grammar retains an interpolated string as one atomic AST node. Its
    // expressions are parsed separately by the frontend and executor, so the
    // same fragments must join the prepare-time safety walk as well.
    let source = node
        .text
        .as_deref()
        .ok_or_else(|| QueryError::semantic("interpolated String is missing its source text"))?;
    let fragments = crate::cypher::interpolation_fragments(source).map_err(QueryError::semantic)?;
    for fragment in fragments {
        let expression =
            crate::cypher::parse_expression_fragment_at(fragment.text, fragment.text, 0)?;
        validate(&expression)?;
    }
    Ok(())
}

fn validate_call(clause: &AstNode) -> QueryResult<()> {
    if clause
        .children
        .iter()
        .any(|child| child.kind == AstKind::Subquery(SubqueryKind::Call))
    {
        return Ok(());
    }
    let name = clause
        .descendants()
        .find(|node| node.kind == AstKind::FunctionName)
        .and_then(|node| node.text.as_deref())
        .ok_or_else(|| QueryError::semantic("CALL is missing its procedure name"))?;
    if safe_read_procedure(name) {
        Ok(())
    } else {
        forbidden(&format!("procedure {name}"))
    }
}

// Explicit allowlist: new procedures require an I/O and mutation review before
// this mode can execute them. Full-text procedures can invoke host tokenizers.
fn safe_read_procedure(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "db.labels"
            | "db.propertykeys"
            | "db.relationshiptypes"
            | "lithograph.branch.list"
            | "lithograph.commit.get"
            | "lithograph.tag.list"
            | "lithograph.log"
            | "lithograph.diff"
            | "lithograph.merge.get"
            | "lithograph.merge.list"
            | "lithograph.merge.conflicts"
    )
}

fn forbidden<T>(operation: &str) -> QueryResult<T> {
    Err(QueryError::invalid_argument(format!(
        "options.readOnlyNoExternalIO forbids {operation}"
    )))
}
