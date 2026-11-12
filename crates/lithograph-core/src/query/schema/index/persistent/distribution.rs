//! Value-independent advice over one existing immutable Range Index generation.
//! The caller prepays the shared planning work and a 16 KiB scratch lease.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{OptionalExtension, params};

use super::statistics::{self, Generation, MAX_KEY_BYTES, MAX_SAMPLES};
use super::*;
use crate::query::access::PropertyRules;
use crate::query::stats::CardinalityEstimate;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexDistribution {
    pub(crate) indexed_owners: u64,
    pub(crate) ndistinct: CardinalityEstimate,
    pub(crate) max_frequency: CardinalityEstimate,
    pub(crate) max_observed_frequency: u64,
    pub(crate) sampled_owners: usize,
    pub(crate) generation_id: i64,
    pub(crate) generation_anchor: HashId,
    pub(crate) definition_hash: HashId,
}

/// Small retained state shared with the value-specific frequency estimator.
/// Keys are hashes only; no property value survives the bounded row callback.
pub(super) struct GenerationSample {
    pub(super) counts: BTreeMap<[u8; 32], u64>,
    pub(super) sampled_owners: usize,
    pub(super) observed_owners: usize,
}

/// Bounds identify an owner without reading its key; a guarded unavailable key
/// cannot equal a caller's already-bounded query key. Keep those proofs apart.
pub(super) enum SampleKey<'a> {
    Unprobed,
    Unavailable,
    Bytes(&'a [u8]),
}

pub(crate) fn estimate_index_distribution(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<IndexDistribution>> {
    control.check()?;
    let Some((kind, properties)) = statistics::scope(index) else {
        return Ok(None);
    };
    if index.kind != StandardIndexKind::Range
        || properties.len() != 1
        || query_access.denies(&properties[0])
        || !statistics::small_definition(index)
    {
        return Ok(None);
    }
    let Some(generation) = statistics::find_generation(snapshot, index, control)? else {
        return Ok(None);
    };
    let Some(changed) = statistics::changed_owners(snapshot, &generation.layers, control)? else {
        return Ok(None);
    };
    // There is no bound query value/type proof here. Do not decode arbitrary
    // changed Strings/Lists or project the ancestor's NDV into a staged state.
    if !changed.is_empty() {
        return Ok(None);
    }
    let Some(sample) =
        sample_generation(snapshot, &generation, kind, &changed, control, |_, _| {})?
    else {
        return Ok(None);
    };
    let Some((ndistinct, max_frequency, maximum)) = estimates(generation.owners, &sample) else {
        return Ok(None);
    };
    Ok(Some(IndexDistribution {
        indexed_owners: generation.owners,
        ndistinct,
        max_frequency,
        max_observed_frequency: maximum,
        sampled_owners: sample.sampled_owners,
        generation_id: generation.id,
        generation_anchor: generation.anchor,
        definition_hash: generation.definition_hash,
    }))
}

fn estimates(
    owners: u64,
    sample: &GenerationSample,
) -> Option<(CardinalityEstimate, CardinalityEstimate, u64)> {
    if owners == 0 {
        return (sample.observed_owners == 0).then_some((
            CardinalityEstimate::exact(0),
            CardinalityEstimate::exact(0),
            0,
        ));
    }
    let rows = sample.sampled_owners as u64;
    let distinct = sample.counts.len() as u64;
    let maximum = sample.counts.values().copied().max()?;
    if rows == 0 || rows > owners || distinct == 0 || maximum > rows {
        return None;
    }
    if rows == owners {
        return Some((
            CardinalityEstimate::exact(distinct),
            CardinalityEstimate::exact(maximum),
            maximum,
        ));
    }
    // ID strata are deterministic: expected is planning advice, while bounds
    // express only facts proved by the sample, not a confidence interval.
    let expected_distinct = scaled(distinct, owners, rows).clamp(distinct, owners);
    let upper_frequency = owners.checked_sub(distinct.checked_sub(1)?)?;
    let expected_frequency = if maximum > 1 {
        scaled(maximum, owners, rows)
    } else {
        owners.div_ceil(expected_distinct)
    };
    Some((
        CardinalityEstimate {
            expected: expected_distinct,
            lower: distinct,
            upper: owners,
        },
        CardinalityEstimate {
            expected: expected_frequency.clamp(maximum, upper_frequency),
            lower: maximum,
            upper: upper_frequency,
        },
        maximum,
    ))
}

fn scaled(count: u64, owners: u64, samples: u64) -> u64 {
    (u128::from(count) * u128::from(owners) / u128::from(samples)) as u64
}

/// The single ID-strata sampling loop for both query-dependent frequency and
/// query-independent join advice. Bounds, skipped keys and changed owners are
/// observed before filtering so malformed counts cannot turn into exact data.
pub(super) fn sample_generation(
    snapshot: &Snapshot<'_>,
    generation: &Generation,
    kind: OwnerKind,
    changed: &BTreeSet<(i64, i64)>,
    control: &ExecutionControl<'_>,
    mut observe: impl FnMut(i64, SampleKey<'_>),
) -> QueryResult<Option<GenerationSample>> {
    control.check()?;
    let connection = snapshot.connection_for_query();
    let Some(bounds) =
        statistics::sample_bounds(connection, generation.id, generation.owners, kind)?
    else {
        return Ok(None);
    };
    let mut sample = GenerationSample {
        counts: BTreeMap::new(),
        sampled_owners: 0,
        observed_owners: 0,
    };
    let Some((first, last)) = bounds else {
        return Ok(Some(sample));
    };
    let mut seen = BTreeSet::from([first, last]);
    let mut sampled = BTreeSet::new();
    observe(first, SampleKey::Unprobed);
    observe(last, SampleKey::Unprobed);
    let mut statement = connection.prepare(
        "SELECT CASE WHEN typeof(owner_id)='integer' AND owner_id>0 THEN owner_id END,\
         CASE WHEN typeof(equality_blob)='blob' AND length(equality_blob)<=?4 THEN equality_blob END \
         FROM main._lithograph_index_entries WHERE generation_id=?1 AND owner_kind=?2 \
         AND owner_id>=?3 ORDER BY owner_id LIMIT 1",
    )?;
    for ordinal in 0..MAX_SAMPLES {
        control.work(1)?;
        let target =
            first + ((last - first) as u128 * ordinal as u128 / MAX_SAMPLES as u128) as i64;
        let row = statement
            .query_row(
                params![generation.id, kind as i64, target, MAX_KEY_BYTES as i64],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                    ))
                },
            )
            .optional()?;
        let Some((Some(owner), blob)) = row else {
            return Ok(None);
        };
        seen.insert(owner);
        observe(
            owner,
            blob.as_deref()
                .map_or(SampleKey::Unavailable, SampleKey::Bytes),
        );
        if seen.len() as u64 > generation.owners {
            return Ok(None);
        }
        if changed.contains(&(kind as i64, owner)) || sampled.contains(&owner) {
            continue;
        }
        let Some(blob) = blob else { continue };
        sampled.insert(owner);
        *sample
            .counts
            .entry(*blake3::hash(&blob).as_bytes())
            .or_default() += 1;
    }
    sample.sampled_owners = sampled.len();
    sample.observed_owners = seen.len();
    Ok(Some(sample))
}
