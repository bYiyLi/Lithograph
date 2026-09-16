//! Test-support-only internal performance instrumentation.
//!
//! These counters are intentionally excluded from normal release builds. They
//! exist so Phase 11 can prove physical-work invariants without changing the
//! public SQL/C ABI or query result contract.

use std::cell::Cell;

/// Internal counters collected by the Phase 11 performance harness.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PerformanceCounters {
    pub resolved_state_builds: u64,
    pub lineage_layers_loaded: u64,
    pub standard_index_builds: u64,
    pub changed_owners: u64,
    pub standard_index_scan_micros: u64,
    pub adjacency_pages: u64,
    pub vector_cache_builds: u64,
    pub vector_cache_build_micros: u64,
    pub vector_cache_entry_loads: u64,
    pub vector_cache_entry_load_micros: u64,
    pub vector_cache_search_micros: u64,
    pub merge_finalize_prepare_micros: u64,
    pub merge_finalize_writer_wait_micros: u64,
    pub merge_finalize_writer_hold_micros: u64,
}

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static COUNTERS: Cell<PerformanceCounters> = const { Cell::new(PerformanceCounters {
        resolved_state_builds: 0,
        lineage_layers_loaded: 0,
        standard_index_builds: 0,
        changed_owners: 0,
        standard_index_scan_micros: 0,
        adjacency_pages: 0,
        vector_cache_builds: 0,
        vector_cache_build_micros: 0,
        vector_cache_entry_loads: 0,
        vector_cache_entry_load_micros: 0,
        vector_cache_search_micros: 0,
        merge_finalize_prepare_micros: 0,
        merge_finalize_writer_wait_micros: 0,
        merge_finalize_writer_hold_micros: 0,
    }) };
}

/// Enables or disables internal instrumentation for the current execution thread.
pub fn set_enabled(enabled: bool) {
    ENABLED.set(enabled);
}

/// Resets all counters to zero without changing whether collection is enabled.
pub fn reset() {
    COUNTERS.set(PerformanceCounters::default());
}

/// Returns a point-in-time snapshot of all counters.
pub fn snapshot() -> PerformanceCounters {
    COUNTERS.get()
}

pub(crate) fn record_resolved_state(layers: usize) {
    if ENABLED.get() {
        let mut counters = COUNTERS.get();
        counters.resolved_state_builds = counters.resolved_state_builds.saturating_add(1);
        counters.lineage_layers_loaded =
            counters.lineage_layers_loaded.saturating_add(layers as u64);
        COUNTERS.set(counters);
    }
}

pub(crate) fn record_standard_index_build() {
    if ENABLED.get() {
        let mut counters = COUNTERS.get();
        counters.standard_index_builds = counters.standard_index_builds.saturating_add(1);
        COUNTERS.set(counters);
    }
}

pub(crate) fn record_changed_owners(count: usize) {
    if ENABLED.get() {
        let mut counters = COUNTERS.get();
        counters.changed_owners = counters.changed_owners.saturating_add(count as u64);
        COUNTERS.set(counters);
    }
}

pub(crate) fn record_standard_index_scan(micros: u128) {
    record_micros(micros, |counters, value| {
        counters.standard_index_scan_micros =
            counters.standard_index_scan_micros.saturating_add(value);
    });
}

pub(crate) fn record_adjacency_page() {
    if ENABLED.get() {
        let mut counters = COUNTERS.get();
        counters.adjacency_pages = counters.adjacency_pages.saturating_add(1);
        COUNTERS.set(counters);
    }
}

pub(crate) fn record_vector_cache_build(micros: u128) {
    if ENABLED.get() {
        let mut counters = COUNTERS.get();
        counters.vector_cache_builds = counters.vector_cache_builds.saturating_add(1);
        counters.vector_cache_build_micros = counters
            .vector_cache_build_micros
            .saturating_add(u64::try_from(micros).unwrap_or(u64::MAX));
        COUNTERS.set(counters);
    }
}

pub(crate) fn record_vector_cache_entry_load(micros: u128) {
    if ENABLED.get() {
        let mut counters = COUNTERS.get();
        counters.vector_cache_entry_loads = counters.vector_cache_entry_loads.saturating_add(1);
        counters.vector_cache_entry_load_micros = counters
            .vector_cache_entry_load_micros
            .saturating_add(u64::try_from(micros).unwrap_or(u64::MAX));
        COUNTERS.set(counters);
    }
}

pub(crate) fn record_vector_cache_search(micros: u128) {
    record_micros(micros, |counters, value| {
        counters.vector_cache_search_micros =
            counters.vector_cache_search_micros.saturating_add(value);
    });
}

pub(crate) fn record_merge_finalize_prepare(micros: u128) {
    record_micros(micros, |counters, value| {
        counters.merge_finalize_prepare_micros = value;
    });
}

pub(crate) fn record_merge_finalize_writer_wait(micros: u128) {
    record_micros(micros, |counters, value| {
        counters.merge_finalize_writer_wait_micros = value;
    });
}

pub(crate) fn record_merge_finalize_writer_hold(micros: u128) {
    record_micros(micros, |counters, value| {
        counters.merge_finalize_writer_hold_micros = value;
    });
}

fn record_micros(micros: u128, update: impl FnOnce(&mut PerformanceCounters, u64)) {
    if ENABLED.get() {
        let mut counters = COUNTERS.get();
        update(&mut counters, u64::try_from(micros).unwrap_or(u64::MAX));
        COUNTERS.set(counters);
    }
}
