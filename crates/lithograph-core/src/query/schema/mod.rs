pub(crate) use index::{
    BoundStandardIndexRead, StandardIndexPage, bind_standard_index_read, scan_bound_index_page,
};
pub(crate) use index::{IndexDistribution, estimate_index_distribution};
mod access;
mod command;
mod equality;
mod execute;
pub(crate) use index::{join_index_cache_allowed, join_index_definition_hash};
mod index;
mod show;
mod validate;

pub(crate) use access::validate_external_transition;
pub(crate) use command::{PreparedSchema, SchemaCounters, prepare_schema};
pub(crate) use execute::execute_schema;
pub(crate) use index::{
    StandardIndexCursor, StandardIndexPredicate, StandardIndexSeek, build_persistent_generation,
    estimate_index_access, scan_node_index_page, scan_relationship_index_page, seek_bytes,
    select_standard_index_seeks,
};
pub(crate) use show::show_rows;
pub(crate) use validate::{
    validate_layer_against_commit_schema_controlled, validate_schema_transition_controlled,
    validation_conflicts_controlled,
};

pub(crate) fn standard_index_kind_name(kind: crate::storage::StandardIndexKind) -> &'static str {
    match kind {
        crate::storage::StandardIndexKind::Lookup => "LOOKUP",
        crate::storage::StandardIndexKind::Range => "RANGE",
        crate::storage::StandardIndexKind::Text => "TEXT",
        crate::storage::StandardIndexKind::Point => "POINT",
        crate::storage::StandardIndexKind::FullText => "FULLTEXT",
        crate::storage::StandardIndexKind::Vector => "VECTOR",
        crate::storage::StandardIndexKind::Semantic => "SEMANTIC",
    }
}
