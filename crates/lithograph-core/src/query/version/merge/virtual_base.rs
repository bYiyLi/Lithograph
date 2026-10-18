//! Budgeted virtual merge bases for criss-cross history.

use std::collections::BTreeMap;

use super::{
    Connection, QueryResult, Value, VersionOwned, VersionSlots, logical_slots, merge_slots,
};
use crate::query::resources::{checked_add, map_entry_bytes, value_bytes_controlled};
use crate::query::version::canonical::same_optional_value_controlled;
use crate::storage::HashId;

#[derive(Debug)]
pub(super) enum VirtualValue {
    Known(Option<Value>),
    Unknown,
}

type VirtualSlots = VersionOwned<BTreeMap<String, VirtualValue>>;

pub(super) fn from_logical_slots(
    connection: &Connection,
    slots: VersionSlots,
) -> QueryResult<VirtualSlots> {
    let (slots, mut input_memory) = slots.into_parts();
    let mut output = empty(connection)?;
    for (slot, value) in slots {
        connection.checkpoint()?;
        let payload = checked_add(
            slot.capacity() as u64,
            value_bytes_controlled(&value, connection.control())?,
        )?;
        if payload > input_memory.bytes() {
            input_memory.grow(payload - input_memory.bytes())?;
        }
        output.memory.merge(input_memory.split(payload)?)?;
        output.memory.grow(map_entry_bytes::<VirtualValue>()?)?;
        output.value.insert(slot, VirtualValue::Known(Some(value)));
    }
    Ok(output)
}

pub(super) fn virtual_base(connection: &Connection, bases: &[HashId]) -> QueryResult<VirtualSlots> {
    let Some(first) = bases.first() else {
        return empty(connection);
    };
    if bases.len() == 1 {
        return from_logical_slots(connection, load_slots(connection, *first)?);
    }
    let mut current = virtual_merge_commits(connection, *first, bases[1])?;
    for commit in &bases[2..] {
        connection.checkpoint()?;
        let right = load_slots(connection, *commit)?;
        let keys = merge_slots(connection, &current, &right, &BTreeMap::new())?;
        let mut next = empty(connection)?;
        for key in keys.iter() {
            connection.checkpoint()?;
            let left = current.get(key).unwrap_or(&VirtualValue::Known(None));
            if same_known_value(connection, left, right.get(key))? {
                insert_virtual(connection, &mut next, key, left)?;
            } else {
                insert_virtual(connection, &mut next, key, &VirtualValue::Unknown)?;
            }
        }
        current = next;
    }
    Ok(current)
}

fn virtual_merge_commits(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
) -> QueryResult<VirtualSlots> {
    connection.checkpoint()?;
    let bases = super::topology::best_common_ancestors(connection, ours_commit, theirs_commit)?;
    let base = virtual_base(connection, &bases)?;
    let ours = load_slots(connection, ours_commit)?;
    let theirs = load_slots(connection, theirs_commit)?;
    let keys = merge_slots(connection, &base, &ours, &theirs)?;
    let mut merged = empty(connection)?;
    for slot in keys.iter() {
        connection.checkpoint()?;
        let selection = select_value(connection, base.get(slot), ours.get(slot), theirs.get(slot))?;
        insert_selection(connection, &mut merged, slot, selection)?;
    }
    Ok(merged)
}

enum Selection<'a> {
    Known(Option<&'a Value>),
    Unknown,
}

fn select_value<'a>(
    connection: &Connection,
    base: Option<&VirtualValue>,
    ours: Option<&'a Value>,
    theirs: Option<&'a Value>,
) -> QueryResult<Selection<'a>> {
    if same_optional_value_controlled(connection, ours, theirs)? {
        return Ok(Selection::Known(ours));
    }
    match base.unwrap_or(&VirtualValue::Known(None)) {
        VirtualValue::Known(base)
            if same_optional_value_controlled(connection, ours, base.as_ref())? =>
        {
            Ok(Selection::Known(theirs))
        }
        VirtualValue::Known(base)
            if same_optional_value_controlled(connection, theirs, base.as_ref())? =>
        {
            Ok(Selection::Known(ours))
        }
        _ => Ok(Selection::Unknown),
    }
}

fn same_known_value(
    connection: &Connection,
    left: &VirtualValue,
    right: Option<&Value>,
) -> QueryResult<bool> {
    match left {
        VirtualValue::Known(left) => {
            same_optional_value_controlled(connection, left.as_ref(), right)
        }
        VirtualValue::Unknown => Ok(false),
    }
}

fn insert_virtual(
    connection: &Connection,
    output: &mut VirtualSlots,
    key: &str,
    value: &VirtualValue,
) -> QueryResult<()> {
    let selection = match value {
        VirtualValue::Known(value) => Selection::Known(value.as_ref()),
        VirtualValue::Unknown => Selection::Unknown,
    };
    insert_selection(connection, output, key, selection)
}

fn insert_selection(
    connection: &Connection,
    output: &mut VirtualSlots,
    key: &str,
    selection: Selection<'_>,
) -> QueryResult<()> {
    if matches!(selection, Selection::Known(None)) {
        return Ok(());
    }
    output.memory.grow(checked_add(
        map_entry_bytes::<VirtualValue>()?,
        key.len() as u64,
    )?)?;
    let key = crate::query::expression::clone_string_controlled(key, Some(connection.control()))?;
    let value = match selection {
        Selection::Known(Some(value)) => {
            let (value, memory) = connection.copy_value(value)?;
            output.memory.merge(memory)?;
            VirtualValue::Known(Some(value))
        }
        Selection::Known(None) => unreachable!("absent slot returned before allocation"),
        Selection::Unknown => VirtualValue::Unknown,
    };
    output.value.insert(key, value);
    Ok(())
}

fn empty(connection: &Connection) -> QueryResult<VirtualSlots> {
    Ok(VersionOwned {
        value: BTreeMap::new(),
        memory: connection.reserve(0)?,
    })
}

fn load_slots(connection: &Connection, commit: HashId) -> QueryResult<VersionSlots> {
    let state = connection.load_state(commit)?;
    logical_slots(connection, &state)
}
