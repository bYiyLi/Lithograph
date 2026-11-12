//! Fixed-size counters for one actually selected Hash Join operator.

/// Cumulative work and high-water marks copied from one Hash Join execution.
/// These fields never contain keys, properties, parameters, or query text.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HashJoinRuntimeMetrics {
    /// Build input rows consumed, including keys that cannot match.
    pub build_rows: u64,
    /// Probe input rows consumed, including keys that cannot match.
    pub probe_rows: u64,
    /// Equal-digest comparisons rejected by actual equality; duplicates are not collisions.
    pub hash_collisions: u64,
    /// Spill partitions created across partitioning rounds, including empty children.
    pub spill_partitions: u64,
    /// Spill read, repartition, and processing batches actually completed.
    pub spill_batches: u64,
    /// SQLite VM steps of this operator's spill statements, not graph accesses.
    pub spill_vm_steps: u64,
    /// Cumulative encoded payload bytes written to spill, excluding SQLite pages.
    pub spill_encoded_bytes: u64,
    /// Peak live reservation bytes owned by this operator's Hash state.
    pub memory_peak_bytes: u64,
    /// Peak allocated bytes of the operator's TEMP store, including page overhead.
    pub temp_allocated_bytes: u64,
    /// A complete, actually installed query-local keyset and its keyed tail.
    pub dynamic_filter: Option<JoinDynamicFilterRuntimeMetrics>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JoinDynamicFilterRuntimeMetrics {
    pub applied: bool,
    pub build_rows: u64,
    pub distinct_keys: u64,
    pub keyset_peak_bytes: u64,
    pub collector_probes: u64,
    pub prefix_rows_read: u64,
    pub equality_seeks: u64,
    pub tail_owners: u64,
}
