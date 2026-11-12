//! Pure work for the actual singleton build and prefix-probe implementation.
//! Width and capacity facts come from the executor, never a public threshold.
use super::super::cost::{AccessEstimate, HashEstimate, TempWork, Work};
use crate::query::stats::CardinalityEstimate;
use crate::query::stream::HashLayoutFacts;

#[derive(Debug, Clone, Copy)]
pub(super) struct HashBuild {
    pub(super) access: AccessEstimate,
    pub(super) layout: HashLayoutFacts,
    /// Includes the old/new buffer overlap during actual capacity growth.
    /// None includes a String key whose retained heap width is not known.
    pub(super) peak: Option<u64>,
    pub(super) key_property: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct HashProbe {
    pub(super) matches: CardinalityEstimate,
    pub(super) encoded_bytes: u64,
    pub(super) key_property: bool,
    pub(super) resident_target: u64,
    /// Actual bounded Grace routing depth, including its first write.
    pub(super) partition_passes: u64,
    /// An observed hot bucket may need several bounded build chunks.
    pub(super) hot_passes: u64,
}

pub(super) fn hash_work(build: HashBuild, probe: HashProbe) -> Option<HashEstimate> {
    let rows = build.access.rows?.expected;
    let encoded = build.layout.encoded_row_bytes?;
    let resident = if rows == 0 {
        Some(0)
    } else {
        build.peak.filter(|peak| *peak <= probe.resident_target)
    };
    let mut estimate = HashEstimate {
        owner: Work {
            io: build
                .access
                .io?
                .saturating_add(rows.saturating_mul(u64::from(build.key_property))),
            cpu: build.access.cpu?.saturating_add(rows.saturating_mul(3)),
        },
        run: Work::default(),
        probe: Work {
            io: u64::from(probe.key_property),
            cpu: 3,
        },
        matches: probe.matches,
        resident: resident.unwrap_or(probe.resident_target),
        temp: TempWork::default(),
    };
    if resident.is_none() && rows != 0 {
        if probe.resident_target == 0 {
            return None;
        }
        estimate.resident = estimate
            .resident
            .saturating_add(crate::query::spill::grace_resident_bound());
        grace_work(&mut estimate, rows, build, probe, encoded);
    }
    Some(estimate)
}

fn grace_work(
    cost: &mut HashEstimate,
    rows: u64,
    build: HashBuild,
    probe: HashProbe,
    encoded: u64,
) {
    let passes = probe.partition_passes.max(1);
    let hot = probe.hot_passes.max(1);
    let build_bytes = logical_record_bytes(encoded);
    let probe_bytes = logical_record_bytes(probe.encoded_bytes);
    let build_io = temp_records(rows, build_bytes);
    let probe_io = temp_records(1, probe_bytes);
    // Build partitions belong to the clause owner. Repartitioning reads the
    // preceding level before writing the next; the final read belongs to each
    // prefix run, where the actual leaf tables are rebuilt.
    let owner_passes = passes.saturating_mul(2).saturating_sub(1);
    cost.owner.io = cost
        .owner
        .io
        .saturating_add(build_io.saturating_mul(owner_passes));
    cost.owner.cpu = cost
        .owner
        .cpu
        .saturating_add(rows.saturating_mul(owner_passes).saturating_mul(2));
    cost.run = Work {
        io: build_io.saturating_add(rows.saturating_mul(u64::from(build.key_property))),
        cpu: rows.saturating_mul(5),
    };
    // Probe data is run-owned. A hot leaf is replayed for every real bounded
    // build chunk; it is not assumed to become uniformly distributed.
    let probe_passes = passes.saturating_mul(2).saturating_add(hot - 1);
    cost.probe.io = cost
        .probe
        .io
        .saturating_add(probe_io.saturating_mul(probe_passes))
        .saturating_add(hot.saturating_mul(u64::from(probe.key_property)))
        // Every finished run retires Probe records by key-page read and DELETE.
        .saturating_add(2);
    cost.probe.cpu = cost
        .probe
        .cpu
        .saturating_add(probe_passes.saturating_mul(2))
        .saturating_add(hot.saturating_mul(3))
        .saturating_add(2);
    cost.temp = TempWork {
        owner: rows
            .saturating_mul(build_bytes)
            .saturating_mul(owner_passes),
        run: rows.saturating_mul(build_bytes),
        per_probe: probe_bytes
            .saturating_mul(probe_passes)
            .saturating_add(crate::query::spill::GRACE_RECORD_FIXED_VALUE_BYTES.saturating_mul(2)),
    };
}

fn logical_record_bytes(payload: u64) -> u64 {
    // Three Integer values and the digest have this logical upper bound; the
    // actual SQLite serial types, headers and allocated pages are not fixed.
    payload.saturating_add(crate::query::spill::GRACE_RECORD_FIXED_VALUE_BYTES)
}

fn temp_records(rows: u64, bytes: u64) -> u64 {
    // One record operation plus payload traffic. 4096 is only the existing
    // cost scale: this does not assert the private SQLite page size is 4096.
    rows.saturating_add(rows.saturating_mul(bytes).div_ceil(4096))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(rows: u64) -> (HashBuild, HashProbe) {
        (
            HashBuild {
                access: AccessEstimate::stream(Some(CardinalityEstimate::exact(rows)), 0),
                layout: HashLayoutFacts {
                    entry_inline_bytes: 256,
                    bucket_bytes: 8,
                    binding_bytes: 256,
                    key_heap_bytes: Some(0),
                    encoded_row_bytes: Some(2048),
                    encoding_scratch_bytes: 2048,
                    headroom_bytes: 8192,
                    soft_cap: 1024 * 1024,
                },
                peak: Some(rows.saturating_mul(600)),
                key_property: true,
            },
            HashProbe {
                matches: CardinalityEstimate::exact(rows),
                encoded_bytes: 4096,
                key_property: true,
                resident_target: 1024 * 1024,
                partition_passes: 1,
                hot_passes: 1,
            },
        )
    }

    #[test]
    fn raw_producer_and_both_property_keys_pay_real_work() {
        let (build, probe) = input(10);
        let cost = hash_work(build, probe).unwrap();
        assert_eq!(cost.owner.io, 20);
        assert_eq!(cost.owner.cpu, 50);
        assert_eq!(cost.probe.io, 1);
        assert_eq!(cost.probe.cpu, 3);
        assert_eq!(cost.temp.owner, 0);
        assert_eq!(cost.resident, 6000);
    }

    #[test]
    fn string_heap_unknown_uses_bounded_grace_and_real_payload() {
        let (mut build, probe) = input(10);
        build.layout.key_heap_bytes = None;
        build.peak = None;
        let cost = hash_work(build, probe).unwrap();
        assert_eq!(
            cost.resident,
            probe.resident_target + crate::query::spill::grace_resident_bound()
        );
        assert_eq!(cost.owner.io, 36);
        assert_eq!(cost.run.io, 26);
        assert_eq!(cost.probe.io, 10);
        assert_eq!(cost.temp.owner, 21_040);
        assert_eq!(cost.temp.per_probe, 8416);
    }

    #[test]
    fn deeper_routing_and_observed_hot_leaf_are_not_one_pass() {
        let (mut build, mut probe) = input(10);
        build.peak = None;
        probe.partition_passes = 3;
        probe.hot_passes = 4;
        let cost = hash_work(build, probe).unwrap();
        assert_eq!(cost.owner.io, 100);
        assert_eq!(cost.run.io, 26);
        assert_eq!(cost.probe.io, 34);
        assert_eq!(cost.temp.owner, 105_200);
        assert_eq!(cost.temp.per_probe, 37_480);
    }

    #[test]
    fn unknown_producer_does_not_turn_into_a_free_build() {
        let (mut build, mut probe) = input(10);
        build.access.rows = None;
        assert!(hash_work(build, probe).is_none());
        let (build, _) = input(10);
        probe.resident_target = 0;
        assert!(hash_work(build, probe).is_none());
        let (mut build, probe) = input(10);
        build.layout.encoded_row_bytes = None;
        assert!(
            hash_work(build, probe).is_none(),
            "unknown payload keeps the existing plan"
        );
    }
}
