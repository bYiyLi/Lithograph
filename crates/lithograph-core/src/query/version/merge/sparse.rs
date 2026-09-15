use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::cypher::Value;
use crate::storage::{self, HashId, LayerBuilder, OwnerKind, PropertyKeyId, SchemaState};

use super::*;

type PropertySlot = (OwnerKind, i64, PropertyKeyId);

struct SparsePropertySlot {
    owner: OwnerKind,
    owner_id: i64,
    key_id: PropertyKeyId,
    base: Option<Value>,
    ours: Option<Value>,
    theirs: Option<Value>,
}

pub(super) struct SparsePropertyMerge<'a> {
    identity: MergeIdentity<'a>,
    slots: BTreeMap<String, SparsePropertySlot>,
    pub(super) ours_schema: SchemaState,
}

struct SparseMergeInputs {
    bases: Vec<HashId>,
    base_commit: HashId,
    ours_layer: LayerBuilder,
    theirs_layer: LayerBuilder,
    ours_schema: SchemaState,
}

pub(crate) fn sparse_candidate_layer(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
    resolutions: &BTreeMap<HashId, Value>,
) -> QueryResult<Option<(LayerBuilder, SchemaState, usize)>> {
    let Some(sparse) =
        try_sparse_property_merge(connection, ours_commit, theirs_commit, resolutions)?
    else {
        return Ok(None);
    };
    let unresolved = sparse.unresolved_count()?;
    let layer = if unresolved == 0 {
        sparse.candidate_layer()?
    } else {
        LayerBuilder::default()
    };
    Ok(Some((layer, sparse.ours_schema, unresolved)))
}

pub(super) fn try_sparse_property_merge<'a>(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
    resolutions: &'a BTreeMap<HashId, Value>,
) -> QueryResult<Option<SparsePropertyMerge<'a>>> {
    let Some(inputs) = sparse_merge_inputs(connection, ours_commit, theirs_commit)? else {
        return Ok(None);
    };
    let ours_changes = sparse_property_changes(&inputs.ours_layer)?;
    let theirs_changes = sparse_property_changes(&inputs.theirs_layer)?;
    let touched = touched_property_slots(&ours_changes, &theirs_changes);
    let Some(key_names) = sparse_property_key_names(connection, &inputs.ours_schema, &touched)?
    else {
        return Ok(None);
    };
    let slots = sparse_property_slots(
        connection,
        inputs.base_commit,
        &touched,
        &key_names,
        &ours_changes,
        &theirs_changes,
    )?;
    Ok(Some(SparsePropertyMerge {
        identity: MergeIdentity {
            base_identity: merge_base_identity(&inputs.bases),
            ours_commit,
            ours_identity: commit_identity(ours_commit),
            theirs_identity: commit_identity(theirs_commit),
            resolutions,
        },
        slots,
        ours_schema: inputs.ours_schema,
    }))
}

fn sparse_merge_inputs(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
) -> QueryResult<Option<SparseMergeInputs>> {
    let Some((bases, base_commit)) = sparse_merge_base(connection, ours_commit, theirs_commit)?
    else {
        return Ok(None);
    };
    if !sparse_schema_hashes_match(connection, base_commit, ours_commit, theirs_commit)? {
        return Ok(None);
    }
    let Some((ours_layer, theirs_layer, ours_schema)) =
        sparse_property_inputs(connection, base_commit, ours_commit, theirs_commit)?
    else {
        return Ok(None);
    };
    Ok(Some(SparseMergeInputs {
        bases,
        base_commit,
        ours_layer,
        theirs_layer,
        ours_schema,
    }))
}

fn sparse_property_inputs(
    connection: &Connection,
    base_commit: HashId,
    ours_commit: HashId,
    theirs_commit: HashId,
) -> QueryResult<Option<(LayerBuilder, LayerBuilder, SchemaState)>> {
    let ours_layer = storage::layer_between_commits(connection, base_commit, ours_commit)?;
    let theirs_layer = storage::layer_between_commits(connection, base_commit, theirs_commit)?;
    if !ours_layer.is_property_only() || !theirs_layer.is_property_only() {
        return Ok(None);
    }
    let ours_schema = SchemaState::load(connection, ours_commit)?;
    if !ours_schema.graph_nodes.is_empty() || !ours_schema.graph_relationships.is_empty() {
        return Ok(None);
    }
    Ok(Some((ours_layer, theirs_layer, ours_schema)))
}

fn sparse_merge_base(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
) -> QueryResult<Option<(Vec<HashId>, HashId)>> {
    let bases = storage::best_common_ancestors(connection, ours_commit, theirs_commit)?;
    if bases.len() != 1 {
        return Ok(None);
    }
    let base_commit = bases[0];
    if !storage::is_first_parent_descendant(connection, base_commit, ours_commit)?
        || !storage::is_first_parent_descendant(connection, base_commit, theirs_commit)?
    {
        return Ok(None);
    }
    Ok(Some((bases, base_commit)))
}

fn sparse_schema_hashes_match(
    connection: &Connection,
    base_commit: HashId,
    ours_commit: HashId,
    theirs_commit: HashId,
) -> QueryResult<bool> {
    let base_record = storage::load_commit(connection, base_commit)?;
    let ours_record = storage::load_commit(connection, ours_commit)?;
    let theirs_record = storage::load_commit(connection, theirs_commit)?;
    Ok(base_record.schema_hash == ours_record.schema_hash
        && base_record.schema_hash == theirs_record.schema_hash)
}

fn touched_property_slots(
    ours_changes: &BTreeMap<PropertySlot, Option<Value>>,
    theirs_changes: &BTreeMap<PropertySlot, Option<Value>>,
) -> BTreeSet<PropertySlot> {
    let mut touched = BTreeSet::new();
    touched.extend(ours_changes.keys().copied());
    touched.extend(theirs_changes.keys().copied());
    touched
}

fn sparse_property_key_names(
    connection: &Connection,
    schema: &SchemaState,
    touched: &BTreeSet<PropertySlot>,
) -> QueryResult<Option<BTreeMap<PropertyKeyId, String>>> {
    let constrained_properties = schema
        .constraints
        .values()
        .flat_map(|constraint| constraint.properties.iter().cloned())
        .collect::<BTreeSet<_>>();
    let mut key_names = BTreeMap::<PropertyKeyId, String>::new();
    for (_, _, key_id) in touched {
        if key_names.contains_key(key_id) {
            continue;
        }
        let key_name = storage::property_key_name(connection, *key_id)?.ok_or_else(|| {
            QueryError::new(
                QueryErrorKind::Storage,
                format!("PropertyKeyId {key_id} is missing"),
            )
        })?;
        if constrained_properties.contains(&key_name) {
            return Ok(None);
        }
        key_names.insert(*key_id, key_name);
    }
    Ok(Some(key_names))
}

fn sparse_property_slots(
    connection: &Connection,
    base_commit: HashId,
    touched: &BTreeSet<PropertySlot>,
    key_names: &BTreeMap<PropertyKeyId, String>,
    ours_changes: &BTreeMap<PropertySlot, Option<Value>>,
    theirs_changes: &BTreeMap<PropertySlot, Option<Value>>,
) -> QueryResult<BTreeMap<String, SparsePropertySlot>> {
    let base_snapshot = storage::Snapshot::resolve(connection, base_commit)?;
    let mut slots = BTreeMap::new();
    for property_slot @ (owner, owner_id, key_id) in touched.iter().copied() {
        let key_name = &key_names[&key_id];
        let base = base_snapshot
            .property(owner, owner_id, key_id)?
            .map(crate::query::graph::property_value)
            .transpose()?;
        let ours = ours_changes
            .get(&property_slot)
            .cloned()
            .unwrap_or_else(|| base.clone());
        let theirs = theirs_changes
            .get(&property_slot)
            .cloned()
            .unwrap_or_else(|| base.clone());
        let slot = match owner {
            OwnerKind::Node => format!("node/{owner_id}/property/{key_name}"),
            OwnerKind::Relationship => {
                format!("relationship/{owner_id}/property/{key_name}")
            }
        };
        slots.insert(
            slot,
            SparsePropertySlot {
                owner,
                owner_id,
                key_id,
                base,
                ours,
                theirs,
            },
        );
    }
    Ok(slots)
}

fn sparse_property_changes(
    layer: &LayerBuilder,
) -> QueryResult<BTreeMap<PropertySlot, Option<Value>>> {
    layer
        .property_changes()
        .map(|(owner, owner_id, key_id, value)| {
            let value = value
                .cloned()
                .map(crate::query::graph::property_value)
                .transpose()?;
            Ok(((owner, owner_id, key_id), value))
        })
        .collect()
}

impl SparsePropertyMerge<'_> {
    fn slot_decision(
        &self,
        slot: &str,
        value: &SparsePropertySlot,
    ) -> QueryResult<(Option<Value>, Option<MergeConflict>)> {
        direct_slot_decision(
            &self.identity,
            slot.to_owned(),
            VirtualValue::Known(value.base.clone()),
            value.ours.clone(),
            value.theirs.clone(),
        )
    }

    pub(super) fn unresolved_count(&self) -> QueryResult<usize> {
        let mut unresolved = 0_usize;
        for (slot, value) in &self.slots {
            if self
                .slot_decision(slot, value)?
                .1
                .is_some_and(|conflict| conflict.resolution.is_none())
            {
                unresolved += 1;
            }
        }
        Ok(unresolved)
    }

    fn candidate_layer(&self) -> QueryResult<LayerBuilder> {
        let mut layer = LayerBuilder::default();
        for (slot, property_slot) in &self.slots {
            let decision = self.slot_decision(slot, property_slot)?.0;
            if decision == property_slot.ours {
                continue;
            }
            match decision {
                Some(value) => {
                    let property =
                        crate::query::mutation::property_from_value(value)?.ok_or_else(|| {
                            QueryError::invalid_argument("property resolution cannot be null")
                        })?;
                    layer.set_property(
                        property_slot.owner,
                        property_slot.owner_id,
                        property_slot.key_id,
                        property,
                    )?;
                }
                None => layer.remove_property(
                    property_slot.owner,
                    property_slot.owner_id,
                    property_slot.key_id,
                )?,
            }
        }
        Ok(layer)
    }

    pub(super) fn conflict_page(&self, offset: usize, limit: usize) -> QueryResult<ConflictPage> {
        let mut builder = ConflictPageBuilder::new(offset, limit);
        for (slot, value) in &self.slots {
            if let Some(conflict) = self.slot_decision(slot, value)?.1
                && builder.push(conflict)
            {
                break;
            }
        }
        builder.finish()
    }

    pub(super) fn conflicts_for_ids(
        &self,
        requested: &BTreeSet<HashId>,
    ) -> QueryResult<BTreeMap<HashId, MergeConflict>> {
        let mut found = BTreeMap::new();
        for (slot, value) in &self.slots {
            if let Some(conflict) = self.slot_decision(slot, value)?.1
                && requested.contains(&conflict.id)
            {
                found.insert(conflict.id, conflict);
                if found.len() == requested.len() {
                    break;
                }
            }
        }
        Ok(found)
    }
}
