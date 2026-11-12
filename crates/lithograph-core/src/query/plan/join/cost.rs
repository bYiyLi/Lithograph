//! Costs describe only the streaming singleton-inner cursor that executes them.
//! Unknown cardinality remains unknown; it is never a cheap zero-row producer.
use super::{PartOrientation, StageAccess};
use crate::query::stats::CardinalityEstimate;

mod hash;
pub(super) use hash::{HashEstimate, TempWork, Work};

#[derive(Clone, Copy, Debug)]
pub(super) struct AccessEstimate {
    pub(super) rows: Option<CardinalityEstimate>,
    pub(super) io: Option<u64>,
    pub(super) cpu: Option<u64>,
    pub(super) memory: u64,
}

impl AccessEstimate {
    pub(super) fn stream(rows: Option<CardinalityEstimate>, startup: u64) -> Self {
        Self {
            rows,
            io: rows.map(|rows| rows.expected.saturating_add(startup)),
            cpu: rows.map(|rows| rows.expected.saturating_mul(2).saturating_add(startup)),
            memory: 256 * 8,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Cost {
    pub(super) io: Option<u64>,
    pub(super) cpu: Option<u64>,
    pub(super) rows: Option<CardinalityEstimate>,
    pub(super) memory: u64,
    /// Logical TEMP traffic, not a measurement of allocated SQLite disk pages.
    pub(super) temp_bytes: Option<u64>,
}

impl Cost {
    pub(super) fn root() -> Self {
        Self {
            io: Some(0),
            cpu: Some(0),
            rows: Some(CardinalityEstimate::exact(1)),
            memory: 0,
            temp_bytes: Some(0),
        }
    }

    pub(super) fn append(self, inner: AccessEstimate) -> Self {
        let outer = self.rows.map(|rows| rows.expected);
        let io = self
            .io
            .zip(outer)
            .zip(inner.io)
            .map(|((old, outer), inner)| old.saturating_add(outer.saturating_mul(inner)));
        let cpu = self
            .cpu
            .zip(outer)
            .zip(inner.cpu)
            .map(|((old, outer), inner)| old.saturating_add(outer.saturating_mul(inner)));
        Self {
            io,
            cpu,
            rows: multiply(self.rows, inner.rows),
            memory: self.memory.saturating_add(inner.memory),
            temp_bytes: self.temp_bytes,
        }
    }

    pub(super) fn score(self) -> Option<u128> {
        Some(
            u128::from(self.io?) + u128::from(self.cpu?) + u128::from(self.memory / 4096)
            // Wide intervals are retained as risk, rather than silently becoming exact.
            + u128::from(self.rows?.upper.saturating_sub(self.rows?.lower)) / 16,
        )
    }

    pub(super) fn empty(self) -> bool {
        self.rows.is_some_and(|rows| rows.upper == 0)
    }

    pub(super) fn beats(self, other: Self) -> bool {
        if self.empty() && !other.empty() {
            return true;
        }
        self.score()
            .zip(other.score())
            .is_some_and(|(new, old)| new < old)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Choice {
    pub(super) part: usize,
    pub(super) orientation: PartOrientation,
    pub(super) access: StageAccess,
    pub(super) cartesian: bool,
}

pub(super) fn multiply(
    left: Option<CardinalityEstimate>,
    right: Option<CardinalityEstimate>,
) -> Option<CardinalityEstimate> {
    if left.is_some_and(|rows| rows.upper == 0) || right.is_some_and(|rows| rows.upper == 0) {
        return Some(CardinalityEstimate::exact(0));
    }
    let (left, right) = (left?, right?);
    Some(CardinalityEstimate {
        expected: left.expected.saturating_mul(right.expected),
        lower: left.lower.saturating_mul(right.lower),
        upper: left.upper.saturating_mul(right.upper),
    })
}

pub(super) fn expectation(expected: u64, upper: u64) -> CardinalityEstimate {
    CardinalityEstimate {
        expected: expected.min(upper),
        lower: 0,
        upper,
    }
}
