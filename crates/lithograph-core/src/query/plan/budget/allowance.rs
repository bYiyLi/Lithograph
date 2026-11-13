//! Fixed per-MATCH shares of one cumulative prepare budget.

use super::{MAX_BYTES, MAX_CANDIDATES, MAX_STATES, PlanningBudget};

#[derive(Debug)]
pub(super) struct ClauseAllowance {
    states: usize,
    bytes: usize,
    advice_bytes: usize,
    pub(super) stopped: bool,
}

impl PlanningBudget {
    pub(crate) fn begin_clause(&mut self, total_matches: usize) {
        if total_matches <= 1 {
            return;
        }
        self.clause = Some(ClauseAllowance {
            states: self.states.saturating_add(MAX_STATES / total_matches),
            bytes: self.bytes.saturating_add(MAX_BYTES / total_matches),
            advice_bytes: 0,
            stopped: false,
        });
    }

    pub(crate) fn end_clause(&mut self) {
        self.clause = None;
    }

    pub(crate) fn clause_scoped(&self) -> bool {
        self.clause.is_some()
    }

    /// Leave room for the actual plan; this only limits optional advice.
    /// The reservation is neither allocated nor counted as spent memory.
    pub(crate) fn reserve_advice_bytes(&mut self, bytes: usize) {
        if let Some(clause) = self.clause.as_mut() {
            clause.advice_bytes = bytes;
        }
    }

    pub(crate) fn remaining_advice_bytes(&self) -> usize {
        self.remaining_bytes()
            .saturating_sub(self.clause.as_ref().map_or(0, |clause| clause.advice_bytes))
    }

    pub(crate) fn remaining_states(&self) -> usize {
        if self.exhausted() {
            return 0;
        }
        let hard = MAX_STATES.saturating_sub(self.states);
        self.clause.as_ref().map_or(hard, |clause| {
            hard.min(clause.states.saturating_sub(self.states))
        })
    }

    pub(crate) fn remaining_bytes(&self) -> usize {
        if self.exhausted() {
            return 0;
        }
        let hard = MAX_BYTES.saturating_sub(self.bytes);
        self.clause.as_ref().map_or(hard, |clause| {
            hard.min(clause.bytes.saturating_sub(self.bytes))
        })
    }

    pub(crate) fn remaining_candidates(&self) -> usize {
        MAX_CANDIDATES.saturating_sub(self.candidates)
    }

    /// Admit before allocating or reading; unsuccessful work is never charged.
    /// An oversized optional request belongs to its local clause first. Hard
    /// depth/part failures and manual stop retain their original global scope.
    pub(super) fn accepts_work(&mut self, states: usize, bytes: usize) -> bool {
        if self.exhausted() {
            return false;
        }
        if self.hard_limit_is_first(states, bytes) {
            self.stop();
            return false;
        }
        !self.reject_clause_work(states, bytes)
    }

    fn hard_limit_is_first(&self, states: usize, bytes: usize) -> bool {
        let hard_states = MAX_STATES.saturating_sub(self.states);
        let hard_bytes = MAX_BYTES.saturating_sub(self.bytes);
        let (soft_states, soft_bytes) =
            self.clause
                .as_ref()
                .map_or((usize::MAX, usize::MAX), |clause| {
                    (
                        clause.states.saturating_sub(self.states),
                        clause.bytes.saturating_sub(self.bytes),
                    )
                });
        (states > hard_states && hard_states <= soft_states)
            || (bytes > hard_bytes && hard_bytes <= soft_bytes)
    }

    fn reject_clause_work(&mut self, states: usize, bytes: usize) -> bool {
        let Some(clause) = self.clause.as_mut() else {
            return false;
        };
        if clause.stopped
            || states > clause.states.saturating_sub(self.states)
            || bytes > clause.bytes.saturating_sub(self.bytes)
        {
            clause.stopped = true;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests;
