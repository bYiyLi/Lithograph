//! Execution-owned memory leases and cooperative cancellation checkpoints.
//!
//! A lease follows the allocation it accounts for. Dropping a lease releases its
//! bytes, so streaming consumers do not spend a cumulative output allowance.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{QueryError, QueryErrorKind, QueryResult};

mod encoding;
mod sizing;
pub use encoding::{
    encoded_json_bytes, encoded_json_bytes_controlled, encoded_row_bytes,
    encoded_row_bytes_controlled,
};
pub use sizing::{
    allocation_bytes, checked_add, checked_mul, estimate_value_bytes, json_bytes,
    json_bytes_controlled, map_entry_bytes, owned_row_bytes, owned_row_bytes_controlled, row_bytes,
    row_bytes_controlled, value_bytes, value_bytes_controlled,
};

const WORK_INTERVAL: u64 = 256;
const BYTE_INTERVAL: u64 = 64 * 1024;

#[derive(Debug, Clone)]
pub struct QueryResources {
    state: Arc<ResourceState>,
}

#[derive(Debug)]
struct ResourceState {
    limit: Option<u64>,
    used: AtomicU64,
    peak: AtomicU64,
}

impl QueryResources {
    pub fn new(limit: Option<u64>) -> Self {
        Self {
            state: Arc::new(ResourceState {
                limit,
                used: AtomicU64::new(0),
                peak: AtomicU64::new(0),
            }),
        }
    }

    pub fn limit(&self) -> Option<u64> {
        self.state.limit
    }

    pub fn used(&self) -> u64 {
        self.state.used.load(Ordering::Relaxed)
    }

    pub fn peak(&self) -> u64 {
        self.state.peak.load(Ordering::Relaxed)
    }

    pub fn reserve(&self, bytes: u64) -> QueryResult<MemoryReservation> {
        self.acquire(bytes)?;
        Ok(MemoryReservation {
            resources: self.clone(),
            bytes,
        })
    }

    fn acquire(&self, bytes: u64) -> QueryResult<()> {
        let mut used = self.used();
        loop {
            let next = checked_add(used, bytes)?;
            if self.limit().is_some_and(|limit| next > limit) {
                return Err(memory_error("query execution memory limit exceeded"));
            }
            match self.state.used.compare_exchange_weak(
                used,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.state.peak.fetch_max(next, Ordering::Relaxed);
                    return Ok(());
                }
                Err(actual) => used = actual,
            }
        }
    }

    fn release(&self, bytes: u64) {
        let previous = self.state.used.fetch_sub(bytes, Ordering::Relaxed);
        debug_assert!(previous >= bytes, "memory lease released more than it held");
    }
}

impl Default for QueryResources {
    fn default() -> Self {
        Self::new(None)
    }
}

/// An exclusive claim on execution memory. It deliberately does not implement
/// `Clone`: copying an allocation requires a second checked reservation.
#[derive(Debug)]
pub struct MemoryReservation {
    resources: QueryResources,
    bytes: u64,
}

impl MemoryReservation {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Reserve before allocating additional storage. Failure leaves this lease
    /// and the query's existing allocations unchanged.
    pub fn grow(&mut self, additional: u64) -> QueryResult<()> {
        let next = checked_add(self.bytes, additional)?;
        self.resources.acquire(additional)?;
        self.bytes = next;
        Ok(())
    }

    /// Increase before allocation; decrease only after the corresponding
    /// allocation is freed, shrunk, or transferred to another existing lease.
    pub fn resize(&mut self, bytes: u64) -> QueryResult<()> {
        if bytes >= self.bytes {
            self.grow(bytes - self.bytes)
        } else {
            self.resources.release(self.bytes - bytes);
            self.bytes = bytes;
            Ok(())
        }
    }

    /// Transfers part of a claim without temporarily releasing shared budget.
    pub fn split(&mut self, bytes: u64) -> QueryResult<Self> {
        if bytes > self.bytes {
            return Err(QueryError::internal(
                "memory lease split exceeds its reservation",
            ));
        }
        self.bytes -= bytes;
        Ok(Self {
            resources: self.resources.clone(),
            bytes,
        })
    }

    /// Transfers an existing claim. Distinct executions cannot exchange claims.
    pub fn merge(&mut self, mut other: Self) -> QueryResult<()> {
        if !Arc::ptr_eq(&self.resources.state, &other.resources.state) {
            return Err(QueryError::internal(
                "cannot merge leases from different executions",
            ));
        }
        self.bytes = checked_add(self.bytes, other.bytes)?;
        other.bytes = 0;
        Ok(())
    }
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.resources.release(self.bytes);
    }
}

/// A borrowed host callback plus shared query accounting. Each worker creates
/// its own checkpoint counters while sharing the same `QueryResources`.
pub struct ExecutionControl<'a> {
    resources: &'a QueryResources,
    is_interrupted: &'a dyn Fn() -> bool,
    work: Cell<u64>,
    bytes: Cell<u64>,
}

impl<'a> ExecutionControl<'a> {
    pub fn new(resources: &'a QueryResources, is_interrupted: &'a dyn Fn() -> bool) -> Self {
        Self {
            resources,
            is_interrupted,
            work: Cell::new(0),
            bytes: Cell::new(0),
        }
    }

    pub fn resources(&self) -> &QueryResources {
        self.resources
    }

    pub fn reserve(&self, bytes: u64) -> QueryResult<MemoryReservation> {
        self.check()?;
        self.resources.reserve(bytes)
    }

    pub fn check(&self) -> QueryResult<()> {
        super::check_interrupted(self.is_interrupted)
    }

    pub fn check_now(&self) -> QueryResult<()> {
        self.check()
    }

    pub fn work(&self, units: u64) -> QueryResult<()> {
        self.advance(&self.work, units, WORK_INTERVAL)
    }

    pub fn checkpoint(&self, units: u64) -> QueryResult<()> {
        self.work(units)
    }

    pub fn bytes(&self, bytes: u64) -> QueryResult<()> {
        self.advance(&self.bytes, bytes, BYTE_INTERVAL)
    }

    fn advance(&self, counter: &Cell<u64>, amount: u64, interval: u64) -> QueryResult<()> {
        let remainder = amount % interval;
        let accumulated = counter.get() + remainder;
        counter.set(accumulated % interval);
        if amount >= interval || accumulated >= interval {
            self.check()?;
        }
        Ok(())
    }
}

pub fn memory_error(message: &str) -> QueryError {
    let mut error = QueryError::new(QueryErrorKind::Resource, message);
    error.sqlite_code = Some(rusqlite::ffi::SQLITE_TOOBIG);
    error
}

#[cfg(test)]
mod tests;
