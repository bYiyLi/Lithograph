//! One owner build, one prefix-run setup, and one probe per outer row.
//! This is separate from the rescan Nested Loop cost; it never multiplies the
//! entire independent build by the prefix cardinality.
use super::*;

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::query::plan::join) struct Work {
    pub(in crate::query::plan::join) io: u64,
    pub(in crate::query::plan::join) cpu: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::query::plan::join) struct TempWork {
    pub(in crate::query::plan::join) owner: u64,
    pub(in crate::query::plan::join) run: u64,
    pub(in crate::query::plan::join) per_probe: u64,
}

#[derive(Debug, Clone, Copy)]
pub(in crate::query::plan::join) struct HashEstimate {
    pub(in crate::query::plan::join) owner: Work,
    pub(in crate::query::plan::join) run: Work,
    pub(in crate::query::plan::join) probe: Work,
    /// Matching Hash keys before the other residual/identity/uniqueness tests.
    pub(in crate::query::plan::join) matches: CardinalityEstimate,
    pub(in crate::query::plan::join) resident: u64,
    pub(in crate::query::plan::join) temp: TempWork,
}

impl HashEstimate {
    pub(in crate::query::plan::join) fn append(
        self,
        prefix: Cost,
        output: Option<CardinalityEstimate>,
    ) -> Cost {
        // This models the executor's first-real-outer-row lazy boundary. It is
        // an estimate only; execution never skips a build using this estimate.
        if prefix.empty() {
            return Cost {
                rows: Some(CardinalityEstimate::exact(0)),
                ..prefix
            };
        }
        let outer = prefix.rows.map(|rows| rows.expected);
        let io = prefix.io.zip(outer).map(|(old, outer)| {
            old.saturating_add(self.owner.io)
                .saturating_add(self.run.io)
                .saturating_add(outer.saturating_mul(self.probe.io))
        });
        let cpu = prefix.cpu.zip(outer).map(|(old, outer)| {
            old.saturating_add(self.owner.cpu)
                .saturating_add(self.run.cpu)
                .saturating_add(outer.saturating_mul(self.probe.cpu))
                // Match merging and residual checking mirror the two per-row CPU
                // units in the existing stream estimate, including duplicate keys.
                .saturating_add(self.matches.expected.saturating_mul(2))
        });
        let temp_bytes = prefix.temp_bytes.zip(outer).map(|(old, outer)| {
            old.saturating_add(self.temp.owner)
                .saturating_add(self.temp.run)
                .saturating_add(outer.saturating_mul(self.temp.per_probe))
        });
        Cost {
            io,
            cpu,
            rows: output,
            memory: prefix.memory.saturating_add(self.resident),
            temp_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(build: u64, matches: u64) -> HashEstimate {
        HashEstimate {
            owner: Work {
                io: build,
                cpu: build.saturating_mul(3),
            },
            run: Work::default(),
            probe: Work { io: 0, cpu: 2 },
            matches: expectation(matches, matches),
            resident: 2048,
            temp: TempWork::default(),
        }
    }

    fn prefix(rows: u64) -> Cost {
        Cost {
            rows: Some(CardinalityEstimate::exact(rows)),
            ..Cost::root()
        }
    }

    #[test]
    fn independent_build_is_once_and_small_outer_still_prefers_scan() {
        let outer = prefix(1);
        let nl = outer.append(AccessEstimate::stream(
            Some(CardinalityEstimate::exact(100)),
            0,
        ));
        let hashed = hash(100, 100).append(outer, nl.rows);
        assert_eq!(hashed.io, Some(100));
        assert_eq!(hashed.cpu, Some(502));
        assert!(nl.beats(hashed));
        let many = hash(100, 1000).append(prefix(1000), Some(expectation(1000, 100_000)));
        assert_eq!(
            many.io,
            Some(100),
            "build is not multiplied by the outer rows"
        );
    }

    #[test]
    fn ready_selective_index_nested_loop_can_beat_hash() {
        let outer = prefix(10);
        let seek = AccessEstimate::stream(Some(expectation(1, 1)), 2);
        let nl = outer.append(seek);
        let hashed = hash(10_000, 10).append(outer, nl.rows);
        assert!(nl.beats(hashed));
    }

    #[test]
    fn no_ndv_large_join_uses_conservative_matches_without_becoming_free() {
        let outer = prefix(10_000);
        let rows = Some(expectation(100_000_000, 100_000_000));
        let mut nl = outer.append(AccessEstimate::stream(
            Some(CardinalityEstimate::exact(10_000)),
            0,
        ));
        nl.rows = rows;
        let hashed = hash(10_000, 100_000_000).append(outer, rows);
        assert!(hashed.beats(nl));
        assert_eq!(hashed.cpu, Some(200_050_000));
    }

    #[test]
    fn grace_run_and_each_probe_pay_real_temp_work() {
        let mut estimate = hash(100, 20);
        estimate.owner.io = 200;
        estimate.run = Work { io: 100, cpu: 100 };
        estimate.probe = Work { io: 2, cpu: 4 };
        estimate.temp = TempWork {
            owner: 10_000,
            run: 10_000,
            per_probe: 200,
        };
        let cost = estimate.append(prefix(10), Some(expectation(5, 20)));
        assert_eq!(cost.io, Some(320));
        assert_eq!(cost.cpu, Some(480));
        assert_eq!(cost.temp_bytes, Some(22_000));
        assert_eq!(
            cost.rows.unwrap().expected,
            5,
            "matches are not the final residual output"
        );
    }

    #[test]
    fn lazy_zero_outer_and_unknown_rows_never_fake_a_free_nonempty_build() {
        let empty = hash(1_000_000, 0).append(prefix(0), None);
        assert_eq!(empty.io, Some(0));
        assert_eq!(empty.memory, 0);
        let mut unknown = prefix(1);
        unknown.rows = None;
        let cost = hash(100, 1).append(unknown, Some(expectation(1, 1)));
        assert_eq!(cost.io, None);
        assert_eq!(cost.cpu, None);
        assert!(cost.score().is_none());
    }
}
