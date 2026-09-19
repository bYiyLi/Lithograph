use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::cypher::{AstKind, AstNode, ClauseKind, Value};
use crate::storage::{HashId, SchemaState};

use super::super::super::QueryResult;
use super::{
    apply_create_constraint, apply_create_index, apply_drop_constraint, apply_drop_index,
    apply_graph_type, schema_error, semantic,
};
use crate::query::QueryError;

pub(super) enum SchemaCommand<'a> {
    Semantic(&'a AstNode, semantic::SemanticCreateKind),
    Ddl(&'a AstNode, ClauseKind),
}

pub(super) fn find_schema_command(root: &AstNode) -> QueryResult<Option<SchemaCommand<'_>>> {
    let semantic_create = semantic::create_call(root)?;
    let clauses = root
        .descendants()
        .filter(|node| matches!(node.kind, AstKind::Clause(kind) if is_schema_ddl(kind)))
        .collect::<Vec<_>>();
    if clauses.is_empty() {
        return Ok(semantic_create.map(|(clause, kind)| SchemaCommand::Semantic(clause, kind)));
    }
    if semantic_create.is_some() || clauses.len() != 1 {
        return Err(schema_error(
            "schema DDL must contain exactly one schema command",
        ));
    }
    let clause = clauses[0];
    let AstKind::Clause(kind) = clause.kind else {
        return Err(QueryError::internal(
            "schema command is missing its clause kind",
        ));
    };
    Ok(Some(SchemaCommand::Ddl(clause, kind)))
}

pub(super) fn apply_schema_command(
    connection: &Connection,
    base_commit: HashId,
    state: &mut SchemaState,
    command: SchemaCommand<'_>,
    source: &str,
    params: &BTreeMap<String, Value>,
) -> QueryResult<ClauseKind> {
    match command {
        SchemaCommand::Semantic(clause, semantic_kind) => {
            semantic::apply_create_index(
                connection,
                base_commit,
                state,
                clause,
                semantic_kind,
                params,
            )?;
            Ok(ClauseKind::CreateIndex)
        }
        SchemaCommand::Ddl(clause, kind) => {
            apply_ddl(state, clause, kind, source)?;
            Ok(kind)
        }
    }
}

fn apply_ddl(
    state: &mut SchemaState,
    clause: &AstNode,
    kind: ClauseKind,
    source: &str,
) -> QueryResult<()> {
    match kind {
        ClauseKind::GraphType => apply_graph_type(state, clause, source),
        ClauseKind::CreateConstraint => apply_create_constraint(state, clause, source),
        ClauseKind::DropConstraint => apply_drop_constraint(state, clause),
        ClauseKind::CreateIndex => apply_create_index(state, clause),
        ClauseKind::DropIndex => apply_drop_index(state, clause),
        _ => Err(QueryError::internal("unexpected schema command kind")),
    }
}

fn is_schema_ddl(kind: ClauseKind) -> bool {
    matches!(
        kind,
        ClauseKind::GraphType
            | ClauseKind::CreateConstraint
            | ClauseKind::DropConstraint
            | ClauseKind::CreateIndex
            | ClauseKind::DropIndex
    )
}
