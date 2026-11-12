//! Bounded replay of immutable Layer deltas into a derived checkpoint.

use rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value as SqlValue};

use super::{HashId, StorageResult};

const APPLY_BATCH_ROWS: usize = 512;

struct DeltaTable {
    source: &'static str,
    target: &'static str,
    columns: &'static str,
    keys: &'static [&'static str],
    insert_conflict: &'static str,
}

const DELTA_TABLES: &[DeltaTable] = &[
    DeltaTable {
        source: "_lithograph_node_delta",
        target: "_lithograph_cp_nodes",
        columns: "node_id",
        keys: &["node_id"],
        insert_conflict: "IGNORE",
    },
    DeltaTable {
        source: "_lithograph_label_delta",
        target: "_lithograph_cp_labels",
        columns: "node_id,label_id",
        keys: &["node_id", "label_id"],
        insert_conflict: "IGNORE",
    },
    DeltaTable {
        source: "_lithograph_rel_delta",
        target: "_lithograph_cp_relationships",
        columns: "relationship_id,source_id,type_id,target_id",
        keys: &["relationship_id"],
        insert_conflict: "REPLACE",
    },
    DeltaTable {
        source: "_lithograph_property_delta",
        target: "_lithograph_cp_properties",
        columns: "owner_kind,owner_id,key_id,type_tag,int_value,real_value,text_value,blob_value,aux_value",
        keys: &["owner_kind", "owner_id", "key_id"],
        insert_conflict: "REPLACE",
    },
];

pub(super) fn apply_layer(
    connection: &Connection,
    commit: HashId,
    layer_id: i64,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    for table in DELTA_TABLES {
        check()?;
        // Empty Layers are common: do not turn a 10k-Commit history into
        // eight no-op writes per historical Layer when building a checkpoint.
        let sql = format!(
            "SELECT EXISTS(SELECT 1 FROM main.{} WHERE layer_id=?1)",
            table.source
        );
        let present: bool = connection.query_row(&sql, [layer_id], |row| row.get(0))?;
        if !present {
            continue;
        }
        apply_operation(connection, commit, layer_id, table, 2, check)?;
        apply_operation(connection, commit, layer_id, table, 1, check)?;
    }
    Ok(())
}

fn apply_operation(
    connection: &Connection,
    commit: HashId,
    layer_id: i64,
    table: &DeltaTable,
    op: i64,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    let key_tuple = table.keys.join(",");
    let placeholders = (3..3 + table.keys.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let where_clause = format!("layer_id=?2 AND op={op} AND ({key_tuple})>({placeholders})");
    let next_sql = format!(
        "SELECT {key_tuple} FROM main.{} WHERE {where_clause} \
         ORDER BY {key_tuple} LIMIT 1 OFFSET {}",
        table.source,
        APPLY_BATCH_ROWS - 1
    );
    let sql = if op == 2 {
        format!(
            "DELETE FROM main.{} WHERE commit_id=?1 AND ({key_tuple}) IN \
             (SELECT {key_tuple} FROM main.{} WHERE {where_clause} \
              ORDER BY {key_tuple} LIMIT {APPLY_BATCH_ROWS})",
            table.target, table.source
        )
    } else {
        format!(
            "INSERT OR {} INTO main.{}(commit_id,{}) \
             SELECT ?1,{} FROM main.{} WHERE {where_clause} \
             ORDER BY {key_tuple} LIMIT {APPLY_BATCH_ROWS}",
            table.insert_conflict, table.target, table.columns, table.columns, table.source
        )
    };
    let mut last_key = vec![0_i64; table.keys.len()];
    let mut next = connection.prepare(&next_sql)?;
    loop {
        check()?;
        let mut values = vec![
            SqlValue::Blob(commit.as_bytes().to_vec()),
            SqlValue::Integer(layer_id),
        ];
        values.extend(last_key.iter().copied().map(SqlValue::Integer));
        let boundary = next
            .query_row(params_from_iter(values.iter()), |row| {
                (0..table.keys.len())
                    .map(|column| row.get::<_, i64>(column))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .optional()?;
        connection.execute(&sql, params_from_iter(values.iter()))?;
        check()?;
        let Some(boundary) = boundary else {
            break;
        };
        last_key = boundary;
    }
    Ok(())
}
