//! Bounded, cancellable copy from a complete first-parent checkpoint.
//!
//! Each INSERT/SELECT reads at most one indexed page of the source. In
//! particular, never execute one uninterruptible graph-sized self-INSERT.

use rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value as SqlValue};

use super::{HashId, StorageResult};

const COPY_BATCH_ROWS: usize = 512;

// Every key is a positive INTEGER by the existing checkpoint storage contract.
// Cursor keys are in primary-key order after the immutable commit prefix.
const COPY_TABLES: &[(&str, &str, &[&str])] = &[
    ("_lithograph_cp_nodes", "node_id", &["node_id"]),
    (
        "_lithograph_cp_labels",
        "node_id,label_id",
        &["node_id", "label_id"],
    ),
    (
        "_lithograph_cp_relationships",
        "relationship_id,source_id,type_id,target_id",
        &["relationship_id"],
    ),
    (
        "_lithograph_cp_properties",
        "owner_kind,owner_id,key_id,type_tag,int_value,real_value,text_value,blob_value,aux_value",
        &["owner_kind", "owner_id", "key_id"],
    ),
];

pub(super) fn copy_checkpoint(
    connection: &Connection,
    source: HashId,
    destination: HashId,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    for &(table, columns, keys) in COPY_TABLES {
        copy_table(connection, source, destination, table, columns, keys, check)?;
    }
    Ok(())
}

fn copy_table(
    connection: &Connection,
    source: HashId,
    destination: HashId,
    table: &str,
    columns: &str,
    keys: &[&str],
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    let key_tuple = keys.join(",");
    let placeholders = |first| {
        (first..first + keys.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let next_sql = format!(
        "SELECT {key_tuple} FROM main.{table} WHERE commit_id=?1 AND \
         ({key_tuple})>({}) ORDER BY {key_tuple} LIMIT 1 OFFSET {}",
        placeholders(2),
        COPY_BATCH_ROWS - 1
    );
    let copy_sql = format!(
        "INSERT INTO main.{table}(commit_id,{columns}) \
         SELECT ?1,{columns} FROM main.{table} WHERE commit_id=?2 AND \
         ({key_tuple})>({}) ORDER BY {key_tuple} LIMIT {COPY_BATCH_ROWS}",
        placeholders(3)
    );
    let source_id = SqlValue::Blob(source.as_bytes().to_vec());
    let destination_id = SqlValue::Blob(destination.as_bytes().to_vec());
    let mut last_key = vec![0_i64; keys.len()];
    let mut next = connection.prepare(&next_sql)?;

    loop {
        check()?;
        let mut selector = vec![source_id.clone()];
        selector.extend(last_key.iter().copied().map(SqlValue::Integer));
        let boundary = next
            .query_row(params_from_iter(selector.iter()), |row| {
                (0..keys.len())
                    .map(|column| row.get::<_, i64>(column))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .optional()?;
        let mut values = vec![destination_id.clone(), source_id.clone()];
        values.extend(last_key.iter().copied().map(SqlValue::Integer));
        let copied = connection.execute(&copy_sql, params_from_iter(values.iter()))?;
        #[cfg(feature = "test-support")]
        crate::performance::record_checkpoint_copy(copied);
        #[cfg(not(feature = "test-support"))]
        let _ = copied;
        check()?;
        let Some(boundary) = boundary else {
            break;
        };
        last_key = boundary;
    }
    Ok(())
}
