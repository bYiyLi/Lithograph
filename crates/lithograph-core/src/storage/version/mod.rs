//! User-facing version-reference and immutable-history storage primitives.

mod gc;
mod history;
mod refs;
mod session;
mod state;

pub use history::{
    CommitRecord, best_common_ancestors, clear_commit_data, commit_data, create_empty_commit,
    discard_uncommitted_chain, is_ancestor, load_commit, reachable_child_count, reachable_commits,
    reverse_topological_log, set_commit_data,
};
pub use refs::{
    NamedRef, active_branch, create_branch_ref, create_tag, delete_branch_ref, delete_tag,
    initialize_connection_state, list_branches, list_tags, move_branch_ref, move_tag,
    resolve_version_descriptor, set_active_branch, validate_ref_name,
};
pub use state::{
    AllocationState, SnapshotState, capture_allocation_state, layer_between, load_snapshot_state,
    restore_allocation_state,
};

pub use session::{
    MergeSessionRecord, create_merge_session, delete_merge_session, list_merge_sessions_after,
    load_merge_resolutions, load_merge_session, merge_session_roots, update_merge_resolutions,
};

pub use gc::{GcCounters, collect_garbage};
