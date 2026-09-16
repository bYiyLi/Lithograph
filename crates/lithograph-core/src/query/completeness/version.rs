use crate::cypher::{AstKind, AstNode};

use super::{ExecutionOptions, PreparedProgram, QueryError, QueryResult};

pub(super) struct VersionProgramFlags {
    pub(super) version_operation: bool,
    pub(super) version_mutation: bool,
    pub(super) checkout_operation: bool,
}

pub(super) fn contains_named_procedure(root: &AstNode, name: &str) -> bool {
    root.descendants()
        .filter(|node| node.kind == AstKind::FunctionName)
        .filter_map(|node| node.text.as_deref())
        .any(|value| value.eq_ignore_ascii_case(name))
}

pub(super) fn contains_version_procedure(root: &AstNode) -> bool {
    root.descendants()
        .filter(|node| node.kind == AstKind::FunctionName)
        .filter_map(|node| node.text.as_deref())
        .any(super::super::registry::is_version_procedure)
}

pub(super) fn contains_version_mutation(root: &AstNode) -> bool {
    root.descendants()
        .filter(|node| node.kind == AstKind::FunctionName)
        .filter_map(|node| node.text.as_deref())
        .any(super::super::registry::is_version_mutation)
}

pub(super) fn version_procedure_count(root: &AstNode) -> usize {
    root.descendants()
        .filter(|node| node.kind == AstKind::FunctionName)
        .filter_map(|node| node.text.as_deref())
        .filter(|name| super::super::registry::is_version_procedure(name))
        .count()
}

pub(super) fn validate_version_program(
    root: &AstNode,
    options: &ExecutionOptions,
    writes: bool,
    transaction_owning: bool,
) -> QueryResult<VersionProgramFlags> {
    let flags = VersionProgramFlags {
        version_operation: contains_version_procedure(root),
        version_mutation: contains_version_mutation(root),
        checkout_operation: contains_named_procedure(root, "lithograph.branch.checkout"),
    };
    if writes && flags.version_operation {
        return Err(QueryError::invalid_argument(
            "graph mutation and Version Procedures cannot share one query",
        ));
    }
    if flags.version_operation && !options.graph_view.is_full_graph() {
        return Err(QueryError::invalid_argument(
            "Version Procedures cannot execute with options.graphView",
        ));
    }
    let index_rebuild = contains_named_procedure(root, "lithograph.index.rebuild");
    if index_rebuild && (transaction_owning || version_procedure_count(root) != 1) {
        return Err(QueryError::transaction_boundary_required(
            "lithograph.index.rebuild must execute as one independent maintenance procedure",
        ));
    }
    Ok(flags)
}

pub(crate) fn retry_version_busy(program: &PreparedProgram) -> bool {
    if program.writes || !program.version_mutation {
        return false;
    }
    let mut procedures = program
        .root
        .descendants()
        .filter(|node| node.kind == AstKind::FunctionName)
        .filter_map(|node| node.text.as_deref())
        .filter(|name| super::super::registry::is_version_procedure(name));
    let Some(name) = procedures.next() else {
        return false;
    };
    if procedures.next().is_some() {
        return false;
    }
    matches!(
        name.to_ascii_lowercase().as_str(),
        "lithograph.merge.resolve" | "lithograph.merge.finalize" | "lithograph.merge.abort"
    )
}

pub(crate) fn suppress_version_summary_commit(program: &PreparedProgram) -> bool {
    contains_named_procedure(&program.root, "lithograph.index.rebuild")
}
