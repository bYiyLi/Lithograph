//! Explicit ownership of version-operation working memory.

use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

use rusqlite::Connection;

use crate::cypher::Value;
use crate::storage::{self, HashId, LayerBuilder, SnapshotState, StorageError};

use super::super::resources::{
    ExecutionControl, MemoryReservation, checked_add, value_bytes_controlled,
};
use super::super::{QueryError, QueryResult};

pub(crate) struct VersionExecution<'connection, 'control> {
    connection: &'connection Connection,
    control: &'control ExecutionControl<'control>,
}

impl<'connection, 'control> VersionExecution<'connection, 'control> {
    pub(crate) fn new(
        connection: &'connection Connection,
        control: &'control ExecutionControl<'control>,
    ) -> Self {
        Self {
            connection,
            control,
        }
    }

    pub(crate) fn control(&self) -> &ExecutionControl<'_> {
        self.control
    }

    pub(crate) fn checkpoint(&self) -> QueryResult<()> {
        self.control.work(1)
    }

    pub(crate) fn reserve(&self, bytes: u64) -> QueryResult<MemoryReservation> {
        self.control.reserve(bytes)
    }

    pub(crate) fn load_state(&self, commit: HashId) -> QueryResult<VersionState> {
        let check = || self.control.check().map_err(storage_error);
        let (state, base_memory) = storage::load_snapshot_state_observed(
            self.connection,
            commit,
            super::super::graph::snapshot_observer(self.control.resources()),
            &check,
        )?;
        Ok(VersionState {
            state,
            memory: self.reserve(0)?,
            _base_memory: Some(base_memory),
        })
    }

    pub(crate) fn clone_state(&self, state: &SnapshotState) -> QueryResult<VersionState> {
        let check = || self.control.check().map_err(storage_error);
        let memory = self.reserve(storage::snapshot_state_memory_bytes(state, &check)?)?;
        self.control.check()?;
        let state = self.clone_state_values(state)?;
        self.control.check()?;
        Ok(VersionState {
            state,
            memory,
            _base_memory: None,
        })
    }

    fn clone_state_values(&self, state: &SnapshotState) -> QueryResult<SnapshotState> {
        let check = || self.control.check().map_err(storage_error);
        let mut output = SnapshotState {
            nodes: Default::default(),
            labels: Default::default(),
            relationships: Default::default(),
            properties: Default::default(),
            schema: state.schema.clone_checked(&check)?,
        };
        for node in &state.nodes {
            self.checkpoint()?;
            output.nodes.insert(*node);
        }
        for label in &state.labels {
            self.checkpoint()?;
            output.labels.insert(*label);
        }
        for (id, relationship) in &state.relationships {
            self.checkpoint()?;
            output.relationships.insert(*id, *relationship);
        }
        for (slot, property) in &state.properties {
            self.checkpoint()?;
            output.properties.insert(
                *slot,
                storage::clone_property_value_checked(property, &check)?,
            );
        }
        Ok(output)
    }

    pub(crate) fn empty_layer(&self) -> QueryResult<VersionLayer> {
        Ok(VersionLayer {
            layer: LayerBuilder::default(),
            memory: self.reserve(0)?,
            base_memory: None,
        })
    }

    pub(crate) fn load_schema(&self, commit: HashId) -> QueryResult<VersionSchema> {
        let check = || self.control.check().map_err(storage_error);
        let observer = super::super::graph::snapshot_observer(self.control.resources());
        let (schema, base_memory) = storage::SchemaState::load_observed(
            self.connection,
            commit,
            observer.as_ref(),
            &check,
        )?;
        Ok(VersionSchema {
            schema,
            memory: self.reserve(0)?,
            base_memory: Some(base_memory),
        })
    }

    pub(crate) fn clone_schema(&self, schema: &storage::SchemaState) -> QueryResult<VersionSchema> {
        let check = || self.control.check().map_err(storage_error);
        let memory = self.reserve(storage::schema_state_memory_bytes(schema, &check)?)?;
        self.control.check()?;
        let schema = schema.clone_checked(&check)?;
        self.control.check()?;
        Ok(VersionSchema {
            schema,
            memory,
            base_memory: None,
        })
    }

    pub(crate) fn layer_between_commits(
        &self,
        base: HashId,
        descendant: HashId,
    ) -> QueryResult<VersionLayer> {
        self.observed_layer_between(base, descendant, false)
    }

    pub(crate) fn touched_layer_between_commits(
        &self,
        base: HashId,
        descendant: HashId,
    ) -> QueryResult<VersionLayer> {
        self.observed_layer_between(base, descendant, true)
    }

    fn observed_layer_between(
        &self,
        base: HashId,
        descendant: HashId,
        touched: bool,
    ) -> QueryResult<VersionLayer> {
        let check = || self.control.check().map_err(storage_error);
        let load = if touched {
            storage::touched_layer_between_commits_observed
        } else {
            storage::layer_between_commits_observed
        };
        let (layer, base_memory) = load(
            self.connection,
            base,
            descendant,
            super::super::graph::snapshot_observer(self.control.resources()),
            &check,
        )?;
        Ok(VersionLayer {
            layer,
            memory: self.reserve(0)?,
            base_memory: Some(base_memory),
        })
    }

    pub(crate) fn snapshot(&self, commit: HashId) -> QueryResult<storage::Snapshot<'connection>> {
        super::super::graph::resolve_snapshot_with_control(self.connection, commit, self.control)
    }

    pub(crate) fn with_layer_snapshot(
        &self,
        commit: HashId,
        layer: &LayerBuilder,
        schema: &storage::SchemaState,
    ) -> QueryResult<storage::Snapshot<'connection>> {
        let check = || self.control.check().map_err(storage_error);
        let _schema_copy = self.reserve(storage::schema_state_memory_bytes(schema, &check)?)?;
        let schema = schema.clone_checked(&check)?;
        super::super::graph::resolve_snapshot_with_layer_and_schema_controlled(
            self.connection,
            commit,
            layer,
            schema,
            self.control,
        )
    }

    pub(crate) fn layer_between(
        &self,
        before: &SnapshotState,
        after: &SnapshotState,
    ) -> QueryResult<VersionLayer> {
        let check = || self.control.check().map_err(storage_error);
        let bytes = checked_add(
            storage::snapshot_state_memory_bytes(before, &check)?,
            storage::snapshot_state_memory_bytes(after, &check)?,
        )?;
        // One allowance holds the changed output slots; another holds the
        // simultaneous canonical comparison encodings of the input values.
        let memory = self.reserve(bytes)?;
        let _comparison = self.reserve(bytes)?;
        self.control.check()?;
        let layer = storage::layer_between_checked(before, after, &check)?;
        self.control.check()?;
        Ok(VersionLayer {
            layer,
            memory,
            base_memory: None,
        })
    }

    pub(crate) fn commit_layer_with_schema(
        &self,
        branch: &str,
        expected_head: HashId,
        second_parent: Option<HashId>,
        layer: &LayerBuilder,
        schema_hash: HashId,
        metadata: &storage::CommitMetadata,
    ) -> storage::StorageResult<HashId> {
        storage::commit_layer_with_schema_observed(
            self.connection,
            branch,
            expected_head,
            second_parent,
            (layer, schema_hash),
            metadata,
            storage::CommitRead::observed(
                super::super::graph::snapshot_observer(self.control.resources()),
                &storage_check(self.control),
            ),
        )
    }

    pub(crate) fn copy_value(&self, value: &Value) -> QueryResult<(Value, MemoryReservation)> {
        let memory = self.reserve(value_bytes_controlled(value, self.control)?)?;
        let value =
            super::super::expression::clone_value_unreserved_controlled(value, Some(self.control))?;
        Ok((value, memory))
    }

    pub(crate) fn property_value(
        &self,
        value: &storage::PropertyValue,
    ) -> QueryResult<(Value, MemoryReservation)> {
        super::super::graph::borrowed_property_with_control(value, self.control)
    }
}

impl Deref for VersionExecution<'_, '_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.connection
    }
}

pub(crate) fn storage_error(error: QueryError) -> StorageError {
    error.into_storage_error()
}

#[derive(Debug)]
pub(crate) struct VersionState {
    pub(crate) state: SnapshotState,
    pub(crate) memory: MemoryReservation,
    _base_memory: Option<Rc<dyn Debug>>,
}

#[derive(Debug)]
pub(crate) struct VersionLayer {
    pub(crate) layer: LayerBuilder,
    pub(crate) memory: MemoryReservation,
    base_memory: Option<Rc<dyn Debug>>,
}

#[derive(Debug)]
pub(crate) struct VersionSchema {
    pub(crate) schema: storage::SchemaState,
    pub(crate) memory: MemoryReservation,
    base_memory: Option<Rc<dyn Debug>>,
}

#[derive(Debug)]
pub(crate) struct VersionOwned<T> {
    pub(crate) value: T,
    pub(crate) memory: MemoryReservation,
}

pub(crate) type VersionValue = VersionOwned<Value>;
pub(crate) type VersionValues = VersionOwned<Vec<Value>>;
pub(crate) type VersionRows = VersionOwned<Vec<super::ProcedureRow>>;

impl<T> VersionOwned<T> {
    pub(crate) fn into_parts(self) -> (T, MemoryReservation) {
        (self.value, self.memory)
    }
}

impl<T> Deref for VersionOwned<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for VersionOwned<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

/// Keeps storage-origin opaque leases alive after their values are moved out of
/// a wrapper. Additional checked allocations remain in the same ownership unit.
#[derive(Debug)]
pub(crate) struct VersionOwnedMemory {
    _base: Option<Rc<dyn Debug>>,
    _memory: MemoryReservation,
}

impl Deref for VersionState {
    type Target = SnapshotState;

    fn deref(&self) -> &SnapshotState {
        &self.state
    }
}

impl DerefMut for VersionState {
    fn deref_mut(&mut self) -> &mut SnapshotState {
        &mut self.state
    }
}

impl VersionLayer {
    pub(crate) fn into_parts(self) -> (LayerBuilder, VersionOwnedMemory) {
        (
            self.layer,
            VersionOwnedMemory {
                _base: self.base_memory,
                _memory: self.memory,
            },
        )
    }
}

impl Deref for VersionLayer {
    type Target = LayerBuilder;

    fn deref(&self) -> &LayerBuilder {
        &self.layer
    }
}

impl DerefMut for VersionLayer {
    fn deref_mut(&mut self) -> &mut LayerBuilder {
        &mut self.layer
    }
}

impl VersionSchema {
    pub(crate) fn into_parts(self) -> (storage::SchemaState, VersionOwnedMemory) {
        (
            self.schema,
            VersionOwnedMemory {
                _base: self.base_memory,
                _memory: self.memory,
            },
        )
    }
}

impl Deref for VersionSchema {
    type Target = storage::SchemaState;
    fn deref(&self) -> &storage::SchemaState {
        &self.schema
    }
}

impl DerefMut for VersionSchema {
    fn deref_mut(&mut self) -> &mut storage::SchemaState {
        &mut self.schema
    }
}

pub(crate) fn storage_check<'a>(
    control: &'a ExecutionControl<'_>,
) -> impl Fn() -> storage::StorageResult<()> + 'a {
    move || control.check().map_err(storage_error)
}
