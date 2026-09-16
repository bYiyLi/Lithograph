//! Test-only large Search corpus construction for Phase 11.

use rusqlite::{Connection, Statement, params};

use super::encoding::RecordHasher;
use super::fixture_support::{
    checked_fixture_identity, finalize_fixture_commit, hash_fixture_node, insert_fixture_label,
    insert_fixture_property, report_fixture_progress,
};
use super::identity::allocate_layer_id;
use super::{
    CommitMetadata, HashId, PropertyValue, StorageError, StorageResult, VectorCoordinateType,
    VectorValue, allocate_node_id_range, branch_head, intern_label, intern_property_key,
    root_commit,
};

#[derive(Debug, Clone, Copy)]
pub struct SearchScaleFixtureSpec {
    pub document_count: u64,
    pub high_dimension_count: u64,
    pub low_dimension: u64,
    pub high_dimension: u64,
    pub progress_interval: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchScaleFixture {
    pub root: HashId,
    pub commit: HashId,
    pub first_node: i64,
    pub document_label: i64,
    pub visible_label: i64,
}

#[derive(Debug, Clone, Copy)]
struct SearchSeedIds {
    first_node: i64,
    document_label: i64,
    high_dimension_label: i64,
    visible_label: i64,
    id_key: i64,
    text_key: i64,
    low_embedding_key: i64,
    high_embedding_key: i64,
    layer_id: i64,
}

struct SearchSeedContext<'a, P> {
    connection: &'a Connection,
    ids: SearchSeedIds,
    spec: SearchScaleFixtureSpec,
    hasher: &'a mut RecordHasher,
    progress: &'a mut P,
}

impl<P: FnMut(&str, u64, u64)> SearchSeedContext<'_, P> {
    fn node_id(&self, offset: u64) -> StorageResult<i64> {
        checked_search_identity(self.ids.first_node, offset)
    }

    fn report(&mut self, phase: &str, offset: u64) {
        report_fixture_progress(
            self.progress,
            phase,
            offset,
            self.spec.document_count,
            self.spec.progress_interval,
        );
    }
}

pub fn seed_search_scale_fixture(
    connection: &Connection,
    spec: SearchScaleFixtureSpec,
    mut progress: impl FnMut(&str, u64, u64),
) -> StorageResult<SearchScaleFixture> {
    validate_search_spec(spec)?;
    let root = require_fresh_root(connection)?;
    connection.execute_batch("SAVEPOINT lithograph_phase11_search_seed")?;
    let result = seed_search_scale_inner(connection, spec, root, &mut progress);
    match result {
        Ok(fixture) => {
            connection.execute_batch("RELEASE lithograph_phase11_search_seed")?;
            Ok(fixture)
        }
        Err(error) => {
            connection.execute_batch(
                "ROLLBACK TO lithograph_phase11_search_seed; RELEASE lithograph_phase11_search_seed",
            )?;
            Err(error)
        }
    }
}

fn validate_search_spec(spec: SearchScaleFixtureSpec) -> StorageResult<()> {
    if spec.document_count == 0
        || spec.high_dimension_count == 0
        || spec.high_dimension_count > spec.document_count
        || spec.low_dimension < 2
        || spec.high_dimension < 2
    {
        return Err(StorageError::corrupt(
            "Search scale fixture requires positive corpus sizes and dimensions >= 2",
        ));
    }
    Ok(())
}

fn require_fresh_root(connection: &Connection) -> StorageResult<HashId> {
    let root = root_commit(connection)?;
    if branch_head(connection, "main")? != root {
        return Err(StorageError::corrupt(
            "Search scale fixture requires main to reference Root Commit",
        ));
    }
    let commits: i64 =
        connection.query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })?;
    if commits != 1 {
        return Err(StorageError::corrupt(
            "Search scale fixture requires a freshly initialized database",
        ));
    }
    Ok(root)
}

fn seed_search_scale_inner(
    connection: &Connection,
    spec: SearchScaleFixtureSpec,
    root: HashId,
    progress: &mut impl FnMut(&str, u64, u64),
) -> StorageResult<SearchScaleFixture> {
    let ids = prepare_search_ids(connection, spec)?;
    let mut hasher = search_layer_hasher(spec)?;
    {
        let mut context = SearchSeedContext {
            connection,
            ids,
            spec,
            hasher: &mut hasher,
            progress,
        };
        seed_search_nodes(&mut context)?;
        seed_search_labels(&mut context)?;
        seed_search_properties(&mut context)?;
    }
    let commit = finalize_search_commit(connection, ids.layer_id, root, spec, hasher.finish())?;
    Ok(SearchScaleFixture {
        root,
        commit,
        first_node: ids.first_node,
        document_label: ids.document_label,
        visible_label: ids.visible_label,
    })
}

fn prepare_search_ids(
    connection: &Connection,
    spec: SearchScaleFixtureSpec,
) -> StorageResult<SearchSeedIds> {
    Ok(SearchSeedIds {
        first_node: allocate_node_id_range(connection, spec.document_count)?,
        document_label: intern_label(connection, "Phase11SearchDocument")?,
        high_dimension_label: intern_label(connection, "Phase11SearchHighDimension")?,
        visible_label: intern_label(connection, "Phase11SearchVisible")?,
        id_key: intern_property_key(connection, "scaleId")?,
        text_key: intern_property_key(connection, "text")?,
        low_embedding_key: intern_property_key(connection, "embedding128")?,
        high_embedding_key: intern_property_key(connection, "embedding1536")?,
        layer_id: allocate_layer_id(connection)?,
    })
}

fn search_layer_hasher(spec: SearchScaleFixtureSpec) -> StorageResult<RecordHasher> {
    let visible_count = spec.document_count.div_ceil(2);
    let field_count = spec
        .document_count
        .checked_mul(5)
        .and_then(|count| count.checked_add(visible_count))
        .and_then(|count| count.checked_add(spec.high_dimension_count.checked_mul(2)?))
        .ok_or_else(|| StorageError::corrupt("Search scale Layer field count overflow"))?;
    Ok(RecordHasher::new(
        "LAYER",
        usize::try_from(field_count)
            .map_err(|_| StorageError::corrupt("Search scale fixture is too large"))?,
    ))
}

fn seed_search_nodes<P: FnMut(&str, u64, u64)>(
    context: &mut SearchSeedContext<'_, P>,
) -> StorageResult<()> {
    let mut statement = context.connection.prepare(
        "INSERT INTO main._lithograph_node_delta(layer_id, node_id, op) VALUES(?1, ?2, 1)",
    )?;
    for offset in 0..context.spec.document_count {
        let node_id = context.node_id(offset)?;
        statement.execute(params![context.ids.layer_id, node_id])?;
        hash_search_node(context.hasher, node_id);
        context.report("nodes", offset);
    }
    Ok(())
}

fn seed_search_labels<P: FnMut(&str, u64, u64)>(
    context: &mut SearchSeedContext<'_, P>,
) -> StorageResult<()> {
    let mut statement = context.connection.prepare(
        "INSERT INTO main._lithograph_label_delta(layer_id, node_id, label_id, op) VALUES(?1, ?2, ?3, 1)",
    )?;
    for offset in 0..context.spec.document_count {
        let node_id = context.node_id(offset)?;
        insert_search_label(
            &mut statement,
            context.hasher,
            context.ids.layer_id,
            node_id,
            context.ids.document_label,
        )?;
        if offset < context.spec.high_dimension_count {
            insert_search_label(
                &mut statement,
                context.hasher,
                context.ids.layer_id,
                node_id,
                context.ids.high_dimension_label,
            )?;
        }
        if offset.is_multiple_of(2) {
            insert_search_label(
                &mut statement,
                context.hasher,
                context.ids.layer_id,
                node_id,
                context.ids.visible_label,
            )?;
        }
        context.report("labels", offset);
    }
    Ok(())
}

fn insert_search_label(
    statement: &mut Statement<'_>,
    hasher: &mut RecordHasher,
    layer_id: i64,
    node_id: i64,
    label_id: i64,
) -> StorageResult<()> {
    insert_fixture_label(statement, hasher, layer_id, node_id, label_id)
}

fn seed_search_properties<P: FnMut(&str, u64, u64)>(
    context: &mut SearchSeedContext<'_, P>,
) -> StorageResult<()> {
    let mut statement = context.connection.prepare(
        "INSERT INTO main._lithograph_property_delta(layer_id, owner_kind, owner_id, key_id, op, type_tag, int_value, real_value, text_value, blob_value, aux_value) VALUES(?1, 1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for offset in 0..context.spec.document_count {
        let node_id = context.node_id(offset)?;
        insert_search_property(
            &mut statement,
            context.hasher,
            context.ids.layer_id,
            node_id,
            context.ids.id_key,
            PropertyValue::Integer(
                i64::try_from(offset + 1)
                    .map_err(|_| StorageError::corrupt("Search scaleId exceeds INTEGER64"))?,
            ),
        )?;
        insert_search_property(
            &mut statement,
            context.hasher,
            context.ids.layer_id,
            node_id,
            context.ids.text_key,
            PropertyValue::String(search_document_text(offset)),
        )?;
        insert_search_property(
            &mut statement,
            context.hasher,
            context.ids.layer_id,
            node_id,
            context.ids.low_embedding_key,
            search_vector(
                offset,
                context.spec.document_count,
                context.spec.low_dimension,
            )?,
        )?;
        if offset < context.spec.high_dimension_count {
            insert_search_property(
                &mut statement,
                context.hasher,
                context.ids.layer_id,
                node_id,
                context.ids.high_embedding_key,
                search_vector(
                    offset,
                    context.spec.high_dimension_count,
                    context.spec.high_dimension,
                )?,
            )?;
        }
        context.report("properties", offset);
    }
    Ok(())
}

fn search_document_text(offset: u64) -> String {
    format!(
        "phase eleven search document {offset} common bucket{} needle{offset}",
        offset % 1_000
    )
}

fn search_vector(offset: u64, count: u64, dimension: u64) -> StorageResult<PropertyValue> {
    let bytes = dimension
        .checked_mul(4)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| StorageError::corrupt("Search vector dimension is too large"))?;
    let mut packed = vec![0_u8; bytes];
    packed[0..4].copy_from_slice(&1.0_f32.to_le_bytes());
    let denominator = count.saturating_sub(1).max(1) as f32;
    let second = offset as f32 / denominator;
    packed[4..8].copy_from_slice(&second.to_le_bytes());
    Ok(PropertyValue::Vector(VectorValue {
        coordinate_type: VectorCoordinateType::F32,
        dimension,
        packed,
    }))
}

fn insert_search_property(
    statement: &mut Statement<'_>,
    hasher: &mut RecordHasher,
    layer_id: i64,
    owner_id: i64,
    key_id: i64,
    value: PropertyValue,
) -> StorageResult<()> {
    insert_fixture_property(statement, hasher, layer_id, owner_id, key_id, value)
}

fn hash_search_node(hasher: &mut RecordHasher, node_id: i64) {
    hash_fixture_node(hasher, node_id);
}

fn checked_search_identity(first: i64, offset: u64) -> StorageResult<i64> {
    checked_fixture_identity(first, offset, "Search NodeId")
}

fn finalize_search_commit(
    connection: &Connection,
    layer_id: i64,
    root: HashId,
    spec: SearchScaleFixtureSpec,
    layer_hash: HashId,
) -> StorageResult<HashId> {
    let metadata = CommitMetadata {
        author: Some("phase11-search-scale".to_owned()),
        message: Some(format!(
            "seed {} search documents / {} high-dimension vectors",
            spec.document_count, spec.high_dimension_count
        )),
        committed_at: 11,
    };
    finalize_fixture_commit(connection, layer_id, root, layer_hash, &metadata)
}
