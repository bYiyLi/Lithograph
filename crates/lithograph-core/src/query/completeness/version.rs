use super::effects::ProgramEffects;
use super::{ExecutionOptions, PreparedProgram, QueryError, QueryResult};

pub(super) struct VersionProgramFlags {
    pub(super) version_operation: bool,
    pub(super) version_mutation: bool,
    pub(super) checkout_operation: bool,
}

pub(super) fn validate_version_program(
    effects: &ProgramEffects,
    options: &ExecutionOptions,
    writes: bool,
    transaction_owning: bool,
) -> QueryResult<VersionProgramFlags> {
    let flags = VersionProgramFlags {
        version_operation: !effects.version_procedures.is_empty(),
        version_mutation: effects
            .version_procedures
            .iter()
            .any(|name| super::super::registry::is_version_mutation(name)),
        checkout_operation: effects.has_version_procedure("lithograph.branch.checkout"),
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
    let index_rebuild = effects.has_version_procedure("lithograph.index.rebuild");
    if index_rebuild && (transaction_owning || effects.version_procedures.len() != 1) {
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
    let mut procedures = program.effects.version_procedures.iter();
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
    program
        .effects
        .has_version_procedure("lithograph.index.rebuild")
}
