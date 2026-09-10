//! Loading helpers for persisted Layer rows.

use rusqlite::Connection;

use super::layer::{DeltaOp, LayerBuilder, PropertyDelta, RelationshipDelta, RelationshipRecord};
use super::property::PropertyColumns;
use super::{OwnerKind, PropertyValue, StorageError, StorageResult};

pub(super) fn load_relationship_deltas(
    connection: &Connection,
    layer_id: i64,
    layer: &mut LayerBuilder,
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "SELECT relationship_id, source_id, type_id, target_id, op FROM main._lithograph_rel_delta WHERE layer_id = ?1 ORDER BY relationship_id",
    )?;
    let rows = statement.query_map([layer_id], |row| {
        Ok((
            RelationshipRecord {
                id: row.get(0)?,
                source: row.get(1)?,
                type_id: row.get(2)?,
                target: row.get(3)?,
            },
            row.get::<_, i64>(4)?,
        ))
    })?;
    for row in rows {
        let (record, op) = row?;
        layer.relationships.insert(
            record.id,
            RelationshipDelta {
                op: DeltaOp::from_i64(op)?,
                record,
            },
        );
    }
    Ok(())
}

pub(super) fn load_property_deltas(
    connection: &Connection,
    layer_id: i64,
    layer: &mut LayerBuilder,
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "SELECT owner_kind, owner_id, key_id, op, type_tag, int_value, real_value, text_value, blob_value, aux_value FROM main._lithograph_property_delta WHERE layer_id = ?1 ORDER BY owner_kind, owner_id, key_id",
    )?;
    let mut rows = statement.query([layer_id])?;
    while let Some(row) = rows.next()? {
        let owner_kind = OwnerKind::from_i64(row.get(0)?)?;
        let owner_id = row.get::<_, i64>(1)?;
        let key_id = row.get::<_, i64>(2)?;
        let op = DeltaOp::from_i64(row.get(3)?)?;
        let value = property_value_from_row(row, op)?;
        layer
            .properties
            .insert((owner_kind, owner_id, key_id), PropertyDelta { op, value });
    }
    Ok(())
}

fn property_value_from_row(
    row: &rusqlite::Row<'_>,
    op: DeltaOp,
) -> StorageResult<Option<PropertyValue>> {
    let type_tag = row.get::<_, Option<i64>>(4)?;
    if op == DeltaOp::Remove {
        ensure_remove_payload_is_empty(row, type_tag)?;
        return Ok(None);
    }
    let columns = PropertyColumns {
        type_tag: type_tag
            .ok_or_else(|| StorageError::corrupt("set property row is missing type tag"))?,
        int_value: row.get(5)?,
        real_value: row.get(6)?,
        text_value: row.get(7)?,
        blob_value: row.get(8)?,
        aux_value: row.get(9)?,
    };
    Ok(Some(columns.to_value()?))
}

fn ensure_remove_payload_is_empty(
    row: &rusqlite::Row<'_>,
    type_tag: Option<i64>,
) -> StorageResult<()> {
    let payload_present = type_tag.is_some()
        || row.get::<_, Option<i64>>(5)?.is_some()
        || row.get::<_, Option<f64>>(6)?.is_some()
        || row.get::<_, Option<String>>(7)?.is_some()
        || row.get::<_, Option<Vec<u8>>>(8)?.is_some()
        || row.get::<_, Option<Vec<u8>>>(9)?.is_some();
    if payload_present {
        Err(StorageError::corrupt(
            "removed property row must not retain a typed payload",
        ))
    } else {
        Ok(())
    }
}
