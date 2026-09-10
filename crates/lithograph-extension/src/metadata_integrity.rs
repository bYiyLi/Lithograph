use super::*;

pub(super) fn has_any_internal_object(connection: &Connection) -> LithographResult<bool> {
    Ok(!internal_schema_objects(connection)?.is_empty())
}

#[derive(Debug, PartialEq, Eq)]
struct MetadataColumn {
    name: String,
    declared_type: String,
    not_null: i64,
    default_value: Option<String>,
    primary_key: i64,
    hidden: i64,
}

#[derive(Debug, PartialEq, Eq)]
struct InternalSchemaObject {
    object_type: String,
    name: String,
    table_name: String,
}

pub(super) fn metadata_integrity_json(connection: &Connection) -> LithographResult<Value> {
    let object_type = metadata_object_type(connection)?;

    let Some(object_type) = object_type else {
        if has_any_internal_object(connection)? {
            let error = LithographError::storage(
                "reserved _lithograph_ schema evidence exists without valid metadata",
            );
            return Ok(json!({
                "ok": false,
                "errors": [error.to_json_value()],
                "checked": ["metadata"],
            }));
        }
        return Err(LithographError::not_initialized());
    };

    let errors = if object_type == "table" {
        current_metadata_integrity_errors(connection)?
    } else {
        vec![LithographError::storage(
            "Lithograph metadata has an invalid storage object type",
        )]
    };
    if let Some(error) = errors
        .iter()
        .find(|error| error.category == ErrorCategory::FormatTooNew)
    {
        return Err(error.clone());
    }
    let error_values: Vec<_> = errors
        .into_iter()
        .map(|error| error.to_json_value())
        .collect();

    Ok(json!({
        "ok": error_values.is_empty(),
        "errors": error_values,
        "checked": ["metadata", "storage", "history", "checkpoints"],
    }))
}

pub(super) fn metadata_object_type(connection: &Connection) -> LithographResult<Option<String>> {
    let mut statement = connection
        .prepare("SELECT type, name FROM main.sqlite_schema")
        .map_err(|error| map_sqlite_error(error, "failed to inspect Lithograph metadata"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| map_sqlite_error(error, "failed to inspect Lithograph metadata"))?;
    let mut non_table_match = None;
    for row in rows {
        let (object_type, name) =
            row.map_err(|error| map_sqlite_error(error, "failed to inspect Lithograph metadata"))?;
        if name.eq_ignore_ascii_case(META_TABLE) {
            if object_type == "table" {
                return Ok(Some(object_type));
            }
            non_table_match.get_or_insert(object_type);
        }
    }
    Ok(non_table_match)
}

pub(super) fn query_metadata_marker(
    connection: &Connection,
) -> SqliteResult<Option<(String, String, i64)>> {
    connection
        .query_row(
            "SELECT magic, database_id, storage_format FROM main._lithograph_meta WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
}

pub(super) fn is_phase01_metadata_bootstrap(connection: &Connection) -> LithographResult<bool> {
    let objects = internal_schema_objects(connection)?;
    let metadata_only = objects.as_slice()
        == [InternalSchemaObject {
            object_type: "table".to_owned(),
            name: META_TABLE.to_owned(),
            table_name: META_TABLE.to_owned(),
        }];
    if !metadata_only {
        return Ok(false);
    }

    let mut errors = Vec::new();
    check_temp_internal_triggers(connection, &mut errors)?;
    check_metadata_schema(connection, &mut errors)?;
    check_metadata_columns(connection, &mut errors)?;
    check_metadata_row_count(connection, &mut errors)?;
    check_metadata_marker(connection, &mut errors)?;
    Ok(errors.is_empty())
}

pub(super) fn ensure_current_metadata_integrity(connection: &Connection) -> LithographResult<()> {
    if let Some(error) = current_metadata_integrity_errors(connection)?
        .into_iter()
        .next()
    {
        return Err(error);
    }
    Ok(())
}

fn current_metadata_integrity_errors(
    connection: &Connection,
) -> LithographResult<Vec<LithographError>> {
    let mut errors = Vec::new();

    check_temp_internal_triggers(connection, &mut errors)?;
    check_metadata_schema(connection, &mut errors)?;
    check_metadata_columns(connection, &mut errors)?;
    check_metadata_row_count(connection, &mut errors)?;
    check_metadata_marker(connection, &mut errors)?;
    check_storage_integrity(connection, &mut errors)?;

    Ok(errors)
}

fn check_temp_internal_triggers(
    connection: &Connection,
    errors: &mut Vec<LithographError>,
) -> LithographResult<()> {
    let mut statement = connection
        .prepare("SELECT tbl_name FROM temp.sqlite_schema WHERE type = 'trigger'")
        .map_err(|error| map_sqlite_error(error, "failed to inspect TEMP trigger inventory"))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| map_sqlite_error(error, "failed to inspect TEMP trigger inventory"))?;
    for row in rows {
        let table_name = row
            .map_err(|error| map_sqlite_error(error, "failed to inspect TEMP trigger inventory"))?;
        if has_internal_prefix(&table_name) {
            errors.push(LithographError::storage(
                "TEMP trigger targets the reserved Lithograph internal namespace",
            ));
            break;
        }
    }
    Ok(())
}

fn internal_schema_objects(connection: &Connection) -> LithographResult<Vec<InternalSchemaObject>> {
    let mut statement = connection
        .prepare(
            "SELECT type, name, tbl_name FROM main.sqlite_schema ORDER BY type, name, tbl_name",
        )
        .map_err(|error| map_sqlite_error(error, "failed to inspect internal schema inventory"))?;
    let rows = statement
        .query_map([], |row| {
            Ok(InternalSchemaObject {
                object_type: row.get(0)?,
                name: row.get(1)?,
                table_name: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        })
        .map_err(|error| map_sqlite_error(error, "failed to inspect internal schema inventory"))?;

    let objects = rows
        .collect::<SqliteResult<Vec<_>>>()
        .map_err(|error| map_sqlite_error(error, "failed to inspect internal schema inventory"))?;
    Ok(objects
        .into_iter()
        .filter(|object| {
            has_internal_prefix(&object.name) || has_internal_prefix(&object.table_name)
        })
        .collect())
}

fn has_internal_prefix(value: &str) -> bool {
    value
        .as_bytes()
        .get(..INTERNAL_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(INTERNAL_PREFIX.as_bytes()))
}

fn check_storage_integrity(
    connection: &Connection,
    errors: &mut Vec<LithographError>,
) -> LithographResult<()> {
    let findings = storage::integrity_check(connection)
        .map_err(|error| map_storage_error(error, "failed to verify Lithograph storage"))?;
    for finding in findings {
        errors.push(LithographError::storage(format!(
            "{}: {}",
            finding.code, finding.message
        )));
    }
    Ok(())
}

fn check_metadata_schema(
    connection: &Connection,
    errors: &mut Vec<LithographError>,
) -> LithographResult<()> {
    let schema_sql = connection
        .query_row(
            "SELECT sql FROM main.sqlite_schema WHERE type = 'table' AND name = ?1",
            [META_TABLE],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|error| map_sqlite_error(error, "failed to inspect Lithograph metadata schema"))?
        .flatten();
    if !schema_sql
        .as_deref()
        .is_some_and(metadata_schema_sql_matches)
    {
        errors.push(LithographError::storage(
            "Lithograph metadata table schema does not match storage format 1",
        ));
    }

    Ok(())
}

fn check_metadata_columns(
    connection: &Connection,
    errors: &mut Vec<LithographError>,
) -> LithographResult<()> {
    let columns = metadata_columns(connection)?;
    let expected = [
        ("id", "INTEGER", 0, None, 1, 0),
        ("magic", "TEXT", 1, None, 0, 0),
        ("database_id", "TEXT", 1, None, 0, 0),
        ("storage_format", "INTEGER", 1, None, 0, 0),
    ];
    if columns.len() != expected.len()
        || columns.iter().zip(expected).any(|(actual, expected)| {
            actual.name != expected.0
                || actual.declared_type != expected.1
                || actual.not_null != expected.2
                || actual.default_value.as_deref() != expected.3
                || actual.primary_key != expected.4
                || actual.hidden != expected.5
        })
    {
        errors.push(LithographError::storage(
            "Lithograph metadata columns do not match storage format 1",
        ));
    }

    Ok(())
}

fn check_metadata_row_count(
    connection: &Connection,
    errors: &mut Vec<LithographError>,
) -> LithographResult<()> {
    let row_count = connection
        .query_row("SELECT count(*) FROM main._lithograph_meta", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| map_sqlite_error(error, "failed to inspect Lithograph metadata rows"))?;
    if row_count != 1 {
        errors.push(LithographError::storage(
            "Lithograph metadata must contain exactly one marker row",
        ));
    }

    Ok(())
}

fn check_metadata_marker(
    connection: &Connection,
    errors: &mut Vec<LithographError>,
) -> LithographResult<()> {
    let marker = query_metadata_marker(connection);

    match marker {
        Ok(Some((magic, database_id, storage_format))) => {
            validate_metadata_marker(&magic, &database_id, storage_format, errors);
        }
        Ok(None) => errors.push(LithographError::storage(
            "Lithograph metadata is missing its marker row",
        )),
        Err(error) => {
            let mapped = map_sqlite_error(error, "failed to inspect Lithograph metadata marker");
            if matches!(
                mapped.category,
                ErrorCategory::Busy | ErrorCategory::Resource | ErrorCategory::Io
            ) {
                return Err(mapped);
            }
            errors.push(LithographError::storage(
                "Lithograph metadata marker does not match storage format 1",
            ));
        }
    }

    Ok(())
}

fn validate_metadata_marker(
    magic: &str,
    database_id: &str,
    storage_format: i64,
    errors: &mut Vec<LithographError>,
) {
    if magic != MAGIC {
        errors.push(LithographError::storage(
            "Lithograph metadata magic marker is invalid",
        ));
    }
    if !is_canonical_uuid(database_id) {
        errors.push(LithographError::storage(
            "Lithograph databaseId is not a canonical RFC 9562 UUID v4",
        ));
    }
    if storage_format > STORAGE_FORMAT_MAX {
        errors.push(format_too_new_error(storage_format));
    } else if storage_format < STORAGE_FORMAT_MIN {
        errors.push(LithographError::storage(format!(
            "database storage format {storage_format} is below supported minimum {STORAGE_FORMAT_MIN}"
        )));
    }
}

fn metadata_columns(connection: &Connection) -> LithographResult<Vec<MetadataColumn>> {
    let mut statement = connection
        .prepare("PRAGMA main.table_xinfo('_lithograph_meta')")
        .map_err(|error| {
            map_sqlite_error(error, "failed to inspect Lithograph metadata columns")
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok(MetadataColumn {
                name: row.get(1)?,
                declared_type: row.get(2)?,
                not_null: row.get(3)?,
                default_value: row.get(4)?,
                primary_key: row.get(5)?,
                hidden: row.get(6)?,
            })
        })
        .map_err(|error| {
            map_sqlite_error(error, "failed to inspect Lithograph metadata columns")
        })?;

    rows.collect::<SqliteResult<Vec<_>>>()
        .map_err(|error| map_sqlite_error(error, "failed to inspect Lithograph metadata columns"))
}

fn metadata_schema_sql_matches(sql: &str) -> bool {
    let normalized: String = sql
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    normalized
        == "createtable_lithograph_meta(idintegerprimarykeycheck(id=1),magictextnotnull,database_idtextnotnull,storage_formatintegernotnull)"
        || normalized
            == "createtablemain._lithograph_meta(idintegerprimarykeycheck(id=1),magictextnotnull,database_idtextnotnull,storage_formatintegernotnull)"
}
