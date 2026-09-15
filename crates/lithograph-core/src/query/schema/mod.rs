mod command;
mod equality;
mod execute;
mod index;
mod show;
mod validate;

pub(crate) use command::{PreparedSchema, SchemaCounters, prepare_schema};
pub(crate) use execute::execute_schema;
pub(crate) use index::{
    StandardIndexSeek, scan_node_index_after, scan_relationship_index_after,
    select_standard_index_seeks,
};
pub(crate) use show::show_rows;
pub(crate) use validate::{
    validate_layer_against_commit_schema, validate_snapshot, validation_conflicts,
};

pub(crate) fn standard_index_kind_name(kind: crate::storage::StandardIndexKind) -> &'static str {
    match kind {
        crate::storage::StandardIndexKind::Lookup => "LOOKUP",
        crate::storage::StandardIndexKind::Range => "RANGE",
        crate::storage::StandardIndexKind::Text => "TEXT",
        crate::storage::StandardIndexKind::Point => "POINT",
        crate::storage::StandardIndexKind::FullText => "FULLTEXT",
        crate::storage::StandardIndexKind::Vector => "VECTOR",
    }
}
