//! Read-only estimates over an existing immutable Standard Index generation.
//! Never binds/builds the execution cache or creates TEMP state.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{OptionalExtension, params};

use super::*;
use crate::query::access::PropertyRules;
use crate::query::stats::{CardinalityEstimate, IndexEstimate};
use crate::query::{QueryErrorKind, resources};

const MAX_ANCESTORS: usize = 64;
const MAX_CHANGED: usize = 64;
pub(super) const MAX_SAMPLES: usize = 32;
const FREQUENCY_CAP: usize = 128;
pub(super) const MAX_KEY_BYTES: usize = 512;
const WORKSPACE_BYTES: u64 = 16 * 1024;
const FREQUENCY_SQL: &str = "SELECT CASE WHEN typeof(owner_id)='integer' AND owner_id>0 THEN owner_id END \
    FROM main._lithograph_index_entries WHERE generation_id=?1 AND owner_kind=?2 AND property_ordinal=0 \
    AND equality_blob IS NOT NULL AND equality_blob=?3 ORDER BY owner_id LIMIT ?4";

#[cfg(test)]
mod tests;

pub(super) struct Generation {
    pub(super) id: i64,
    pub(super) owners: u64,
    pub(super) layers: Vec<i64>,
    pub(super) anchor: HashId,
    pub(super) definition_hash: HashId,
}

struct Distribution {
    ndistinct: Option<u64>,
    mcv: Option<u64>,
    samples: usize,
    outside_frequency: u64,
}

struct Frequency {
    lower: u64,
    capped: bool,
    observed: u64,
    last_owner: Option<i64>,
}

#[derive(Default)]
struct SampleOwner {
    outside_frequency: bool,
}

pub(crate) fn estimate_index_access(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    seek: &super::super::StandardIndexSeek,
    query_access: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<IndexEstimate>> {
    control.check()?;
    let Some((kind, properties)) = scope(index) else {
        return Ok(None);
    };
    if properties.len() != 1 || query_access.denies(&properties[0]) {
        return Ok(None);
    }
    let [(0, super::super::StandardIndexPredicate::Equal(value))] = seek.predicates.as_slice()
    else {
        return Ok(None);
    };
    if !small_value(value, control)? || !small_definition(index) {
        return Ok(None);
    }
    let _memory = match control.reserve(WORKSPACE_BYTES) {
        Ok(memory) => memory,
        Err(error) if error.kind == QueryErrorKind::Resource => return Ok(None),
        Err(error) => return Err(error),
    };
    let result = collect(snapshot, index, kind, value, control);
    match result {
        Err(error) if error.kind == QueryErrorKind::Resource => Ok(None),
        result => result,
    }
}

pub(super) fn scope(index: &IndexDefinition) -> Option<(OwnerKind, &[String])> {
    match &index.target {
        IndexTarget::NodeProperties { properties, .. } => Some((OwnerKind::Node, properties)),
        IndexTarget::RelationshipProperties { properties, .. } => {
            Some((OwnerKind::Relationship, properties))
        }
        _ => None,
    }
}

pub(super) fn small_definition(index: &IndexDefinition) -> bool {
    let target_bytes = match &index.target {
        IndexTarget::NodeProperties { label, properties } => {
            label.len() + properties.iter().map(String::len).sum::<usize>()
        }
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => relationship_type.len() + properties.iter().map(String::len).sum::<usize>(),
        _ => return false,
    };
    index.configuration.is_none()
        && index.labels_or_types.is_empty()
        && index.additional_properties.is_empty()
        && index.name.len() + target_bytes + index.owning_constraint.as_ref().map_or(0, String::len)
            < 256
}

fn small_value(value: &crate::cypher::Value, control: &ExecutionControl<'_>) -> QueryResult<bool> {
    if matches!(
        value,
        crate::cypher::Value::List(_)
            | crate::cypher::Value::Map(_)
            | crate::cypher::Value::Vector(_)
            | crate::cypher::Value::Node(_)
            | crate::cypher::Value::Relationship(_)
            | crate::cypher::Value::Path(_)
    ) {
        return Ok(false);
    }
    Ok(resources::value_bytes_controlled(value, control)? < MAX_KEY_BYTES as u64 / 2)
}

fn collect(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    kind: OwnerKind,
    value: &crate::cypher::Value,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<IndexEstimate>> {
    let Some(generation) = find_generation(snapshot, index, control)? else {
        return Ok(None);
    };
    let Some(changed) = changed_owners(snapshot, &generation.layers, control)? else {
        return Ok(None);
    };
    // A type proof bounds numerical/Boolean changed values. Other families
    // can own arbitrarily large strings/arrays; do not decode them for advice.
    if !changed.is_empty()
        && !matches!(
            value,
            crate::cypher::Value::Boolean(_)
                | crate::cypher::Value::Integer(_)
                | crate::cypher::Value::Float(_)
        )
    {
        return Ok(None);
    }
    let Some(property_value) =
        crate::query::mutation::property_from_value_controlled(value.clone(), control)?
    else {
        return Ok(None);
    };
    let Some(key) = equality_key(&property_value, control)? else {
        return Ok(None);
    };
    if key.len() > MAX_KEY_BYTES {
        return Ok(None);
    }
    let Some(frequency) = frequency(snapshot, generation.id, kind, &key, &changed, control)? else {
        return Ok(None);
    };
    let Some((added, removed)) =
        changed_frequency(snapshot, index, generation.id, &key, &changed, control)?
    else {
        return Ok(None);
    };
    let Some((owners, lower, observed)) =
        corrected_counts(generation.owners, &frequency, added, removed)
    else {
        return Ok(None);
    };
    let Some(distribution) = sample(
        snapshot,
        &generation,
        kind,
        &key,
        &changed,
        &frequency,
        control,
    )?
    else {
        return Ok(None);
    };
    let Some(rows) = estimated_rows(
        generation.owners,
        owners,
        lower,
        observed,
        added.1,
        frequency.capped,
        &distribution,
    ) else {
        return Ok(None);
    };
    Ok(Some(IndexEstimate {
        rows,
        indexed_owners: owners,
        ndistinct: distribution.ndistinct,
        mcv_rows: distribution.mcv,
        sampled_owners: distribution.samples,
    }))
}

fn corrected_counts(
    owners: u64,
    frequency: &Frequency,
    added: (u64, u64),
    removed: u64,
) -> Option<(u64, u64, u64)> {
    // Frequency excludes changed owners, so removed owners are disjoint.
    // A capped probe has also observed one extra matching sentinel owner.
    let observed = frequency.observed.checked_add(removed)?;
    if observed > owners || added.1 > added.0 {
        return None;
    }
    let owners = owners.checked_sub(removed)?.checked_add(added.0)?;
    let lower = frequency.lower.checked_add(added.1)?;
    (lower <= owners).then_some((owners, lower, observed))
}

fn estimated_rows(
    base_owners: u64,
    owners: u64,
    lower: u64,
    observed: u64,
    added_matches: u64,
    capped: bool,
    sample: &Distribution,
) -> Option<CardinalityEstimate> {
    if sample.outside_frequency > base_owners.checked_sub(observed)? {
        return None;
    }
    if !capped {
        return Some(CardinalityEstimate::exact(lower));
    }
    if sample.samples < 8 {
        return None;
    }
    let expected = sample
        .mcv
        .unwrap_or_else(|| base_owners.div_ceil(sample.ndistinct.unwrap_or(1).max(1)));
    Some(CardinalityEstimate {
        expected: expected.checked_add(added_matches)?.clamp(lower, owners),
        lower,
        upper: owners,
    })
}

pub(super) fn find_generation(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<Generation>> {
    let (definition, hash) = index_definition_identity(index)?;
    let mut statement = snapshot.connection_for_query().prepare(
        "SELECT c.parent1,c.layer_id,CASE WHEN typeof(g.generation_id)='integer' THEN g.generation_id END, \
         CASE WHEN typeof(g.indexed_entities)='integer' THEN g.indexed_entities END, \
         CASE WHEN typeof(g.entry_count)='integer' THEN g.entry_count END, \
         CASE WHEN typeof(g.definition_blob)='blob' AND length(g.definition_blob)<=4096 THEN g.definition_blob END \
         FROM main._lithograph_commits c LEFT JOIN main._lithograph_index_generations g \
         ON g.anchor_commit=c.id AND g.definition_hash=?2 AND g.encoding_version=?3 AND g.complete=1 WHERE c.id=?1")?;
    let mut commit = snapshot.commit();
    let mut layers = Vec::new();
    for _ in 0..MAX_ANCESTORS {
        control.work(1)?;
        let row = statement.query_row(
            params![
                commit.as_bytes().as_slice(),
                hash.as_bytes().as_slice(),
                INDEX_ENCODING_VERSION
            ],
            |row| {
                Ok((
                    row.get::<_, Option<Vec<u8>>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                ))
            },
        )?;
        if let Some((id, owners)) = usable_manifest(row.2, row.3, row.4, row.5, &definition) {
            return Ok(Some(Generation {
                id,
                owners,
                layers,
                anchor: commit,
                definition_hash: hash,
            }));
        }
        layers.push(row.1);
        let Some(parent) = row.0 else { return Ok(None) };
        commit = HashId::from_slice(&parent)?;
    }
    Ok(None)
}

fn usable_manifest(
    id: Option<i64>,
    owners: Option<i64>,
    entries: Option<i64>,
    blob: Option<Vec<u8>>,
    definition: &[u8],
) -> Option<(i64, u64)> {
    let (id, owners, entries, blob) = (id?, owners?, entries?, blob?);
    (id > 0 && owners >= 0 && entries == owners && blob == definition)
        .then_some((id, owners as u64))
}

pub(super) fn changed_owners(
    snapshot: &Snapshot<'_>,
    layers: &[i64],
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<BTreeSet<(i64, i64)>>> {
    let mut changed = BTreeSet::new();
    let mut work = 0;
    for layer in layers {
        for (table, owner, column) in [
            ("_lithograph_node_delta", "1", "node_id"),
            ("_lithograph_label_delta", "1", "node_id"),
            ("_lithograph_rel_delta", "2", "relationship_id"),
            ("_lithograph_property_delta", "owner_kind", "owner_id"),
        ] {
            control.work(1)?;
            let sql = format!(
                "SELECT {owner},{column} FROM main.{table} WHERE layer_id=?1 LIMIT {}",
                MAX_CHANGED + 1
            );
            let mut statement = snapshot.connection_for_query().prepare(&sql)?;
            let mut rows = statement.query([layer])?;
            while let Some(row) = rows.next()? {
                control.work(1)?;
                work += 1;
                if work > MAX_CHANGED {
                    return Ok(None);
                }
                changed.insert((row.get(0)?, row.get(1)?));
            }
        }
    }
    if let Some(layer) = snapshot.query_local_layer()
        && !collect_staged(layer, &mut changed, &mut work, control)?
    {
        return Ok(None);
    }
    Ok((changed.len() <= MAX_CHANGED).then_some(changed))
}

fn collect_staged(
    layer: &storage::LayerBuilder,
    changed: &mut BTreeSet<(i64, i64)>,
    work: &mut usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let owners = layer
        .node_changes()
        .map(|(id, _)| (1, id))
        .chain(layer.label_changes().map(|(id, _, _)| (1, id)))
        .chain(layer.relationship_changes().map(|(rel, _)| (2, rel.id)))
        .chain(
            layer
                .property_changes()
                .map(|(kind, id, _, _)| (kind as i64, id)),
        );
    for owner in owners {
        control.work(1)?;
        *work += 1;
        if *work > MAX_CHANGED {
            return Ok(false);
        }
        changed.insert(owner);
    }
    Ok(true)
}

fn frequency(
    snapshot: &Snapshot<'_>,
    generation: i64,
    kind: OwnerKind,
    key: &[u8],
    changed: &BTreeSet<(i64, i64)>,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<Frequency>> {
    let mut statement = snapshot.connection_for_query().prepare(FREQUENCY_SQL)?;
    let mut rows = statement.query(params![
        generation,
        kind as i64,
        key,
        (FREQUENCY_CAP + MAX_CHANGED + 1) as i64
    ])?;
    let mut matches = 0;
    let mut last_owner = None;
    while let Some(row) = rows.next()? {
        control.work(1)?;
        let Some(owner) = row.get::<_, Option<i64>>(0)? else {
            return Ok(None);
        };
        if !changed.contains(&(kind as i64, owner)) {
            matches += 1;
            last_owner = Some(owner);
        }
        if matches > FREQUENCY_CAP as u64 {
            return Ok(Some(Frequency {
                lower: FREQUENCY_CAP as u64,
                capped: true,
                observed: matches,
                last_owner,
            }));
        }
    }
    Ok(Some(Frequency {
        lower: matches,
        capped: false,
        observed: matches,
        last_owner,
    }))
}

fn owner_visible(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    kind: OwnerKind,
    id: i64,
) -> QueryResult<bool> {
    match &index.target {
        IndexTarget::NodeProperties { label, .. } => {
            let Some(label) = storage::find_label(snapshot.connection_for_query(), label)? else {
                return Ok(false);
            };
            Ok(snapshot.node_exists(id)?
                && snapshot.scan_label_after(label, id - 1, 1)?.items.first() == Some(&id))
        }
        IndexTarget::RelationshipProperties {
            relationship_type, ..
        } => {
            let wanted = storage::find_relationship_type(
                snapshot.connection_for_query(),
                relationship_type,
            )?;
            Ok(kind == OwnerKind::Relationship
                && snapshot
                    .relationship(id)?
                    .is_some_and(|rel| Some(rel.type_id) == wanted))
        }
        _ => Ok(false),
    }
}

type ChangedFrequency = Option<((u64, u64), u64)>;
fn changed_frequency(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    generation: i64,
    key: &[u8],
    changed: &BTreeSet<(i64, i64)>,
    control: &ExecutionControl<'_>,
) -> QueryResult<ChangedFrequency> {
    let Some((kind, properties)) = scope(index) else {
        return Ok(None);
    };
    let [property] = properties else {
        return Ok(None);
    };
    let Some(key_id) = storage::find_property_key(snapshot.connection_for_query(), property)?
    else {
        return Ok(None);
    };
    let mut before=snapshot.connection_for_query().prepare("SELECT EXISTS(SELECT 1 FROM main._lithograph_index_entries WHERE generation_id=?1 AND owner_kind=?2 AND owner_id=?3 AND property_ordinal=0)")?;
    let (mut added, mut matches, mut removed) = (0, 0, 0);
    for &(owner, id) in changed {
        control.work(1)?;
        if owner != kind as i64 {
            continue;
        }
        if before.query_row(params![generation, owner, id], |row| row.get::<_, bool>(0))? {
            removed += 1;
        }
        if !owner_visible(snapshot, index, kind, id)? {
            continue;
        }
        let result = snapshot.with_property(kind, id, key_id, |value| {
            let Some(value) = value else {
                return Ok::<_, QueryError>(Some((false, false)));
            };
            if matches!(
                value,
                storage::PropertyValue::List(_) | storage::PropertyValue::Vector(_)
            ) {
                return Ok(None);
            }
            let Some(encoded) = equality_key(value, control)? else {
                return Ok(Some((true, false)));
            };
            if encoded.len() > MAX_KEY_BYTES {
                return Ok(None);
            }
            Ok(Some((true, encoded == key)))
        })?;
        let Some((present, matched)) = result else {
            return Ok(None);
        };
        added += u64::from(present);
        matches += u64::from(matched);
    }
    Ok(Some(((added, matches), removed)))
}

fn sample(
    snapshot: &Snapshot<'_>,
    generation: &Generation,
    kind: OwnerKind,
    key: &[u8],
    changed: &BTreeSet<(i64, i64)>,
    frequency: &Frequency,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<Distribution>> {
    let mut seen = BTreeMap::new();
    let mut outside_frequency = 0;
    let Some(sample) = super::distribution::sample_generation(
        snapshot,
        generation,
        kind,
        changed,
        control,
        |owner, sample_key| {
            let matched = match sample_key {
                super::distribution::SampleKey::Unprobed => None,
                super::distribution::SampleKey::Unavailable => Some(false),
                super::distribution::SampleKey::Bytes(blob) => Some(blob == key),
            };
            outside_frequency += u64::from(observe_owner(
                &mut seen,
                owner,
                matched,
                changed.contains(&(kind as i64, owner)),
                frequency,
            ));
        },
    )?
    else {
        return Ok(None);
    };
    let samples = sample.sampled_owners;
    let query_hash = *blake3::hash(key).as_bytes();
    if samples == 0 {
        return Ok(Some(Distribution {
            ndistinct: (generation.owners == 0).then_some(0),
            mcv: None,
            samples,
            outside_frequency,
        }));
    }
    let distinct =
        (sample.counts.len() as u128 * generation.owners as u128 / samples as u128) as u64;
    let mcv = sample
        .counts
        .get(&query_hash)
        .filter(|count| **count > 1)
        .map(|count| (*count as u128 * generation.owners as u128 / samples as u128) as u64);
    Ok(Some(Distribution {
        ndistinct: Some(distinct),
        mcv,
        samples,
        outside_frequency,
    }))
}

fn observe_owner(
    seen: &mut BTreeMap<i64, SampleOwner>,
    owner: i64,
    matches_key: Option<bool>,
    changed: bool,
    frequency: &Frequency,
) -> bool {
    let state = seen.entry(owner).or_default();
    // The ordered frequency probe visited every equal-key owner through its
    // last ID. Changed owners belong to the separately counted removed set.
    let outside = !changed
        && (frequency.last_owner.is_none_or(|last| owner > last) || matches_key == Some(false));
    let newly_observed = outside && !state.outside_frequency;
    state.outside_frequency |= outside;
    newly_observed
}

pub(super) type SampleBounds = Option<Option<(i64, i64)>>;

pub(super) fn sample_bounds(
    connection: &Connection,
    generation: i64,
    owners: u64,
    kind: OwnerKind,
) -> QueryResult<SampleBounds> {
    // Separate extremum probes use the primary key; min+max together would
    // scan the generation. Preserve row presence separately from invalid type.
    let first=connection.query_row("SELECT CASE WHEN typeof(owner_id)='integer' AND owner_id>0 THEN owner_id END FROM main._lithograph_index_entries WHERE generation_id=?1 AND owner_kind=?2 ORDER BY owner_id LIMIT 1",params![generation,kind as i64],|row|row.get::<_,Option<i64>>(0)).optional()?;
    let last=connection.query_row("SELECT CASE WHEN typeof(owner_id)='integer' AND owner_id>0 THEN owner_id END FROM main._lithograph_index_entries WHERE generation_id=?1 AND owner_kind=?2 ORDER BY owner_id DESC LIMIT 1",params![generation,kind as i64],|row|row.get::<_,Option<i64>>(0)).optional()?;
    Ok(match (first, last) {
        (None, None) if owners == 0 => Some(None),
        (Some(Some(first)), Some(Some(last)))
            if last >= first && owners > u64::from(first != last) =>
        {
            Some(Some((first, last)))
        }
        _ => None,
    })
}
