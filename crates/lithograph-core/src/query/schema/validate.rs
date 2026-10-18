use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::storage::{
    self, ConstraintDefinition, ConstraintDefinitionKind, GraphNodeType, GraphRelationshipType,
    OwnerKind, PropertyRule, PropertyValue, SchemaState, SchemaTarget, Snapshot,
};

use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::equality::property_equality_key;

mod controlled;
pub(crate) use controlled::{
    validate_layer_against_commit_schema_controlled, validate_schema_transition_controlled,
    validation_conflicts_controlled,
};

fn resolve_optional_label<'a>(
    connection: &Connection,
    label: Option<&'a str>,
) -> QueryResult<Option<(&'a str, Option<i64>)>> {
    label
        .map(|label| Ok((label, storage::find_label(connection, label)?)))
        .transpose()
}

fn with_constraint_uniqueness_state(
    connection: &Connection,
    constraint: &ConstraintDefinition,
    validate: impl FnOnce() -> QueryResult<()>,
) -> QueryResult<()> {
    if !matches!(
        constraint.kind,
        ConstraintDefinitionKind::Key | ConstraintDefinitionKind::Unique
    ) {
        return validate();
    }
    connection.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS _lithograph_constraint_validation_seen(\
             key BLOB PRIMARY KEY, owner_id INTEGER NOT NULL\
         ) WITHOUT ROWID;\
         DELETE FROM temp._lithograph_constraint_validation_seen;",
    )?;
    let result = validate();
    // A rows execution runs inside an active SQLite virtual-table statement.
    // DROP would require a TEMP schema lock that this statement itself holds.
    // Keep the fixed connection-local scratch table and clear its contents;
    // both successful and failed validations must start the next check empty.
    let cleanup =
        connection.execute_batch("DELETE FROM temp._lithograph_constraint_validation_seen");
    match result {
        Ok(()) => {
            cleanup?;
            Ok(())
        }
        Err(error) => {
            let _ = cleanup;
            Err(error)
        }
    }
}

fn record_uniqueness_key(
    connection: &Connection,
    constraint: &ConstraintDefinition,
    element: &str,
    element_id: i64,
    key: &[u8],
) -> QueryResult<()> {
    let inserted = connection.execute(
        "INSERT OR IGNORE INTO temp._lithograph_constraint_validation_seen(key, owner_id) VALUES(?1, ?2)",
        rusqlite::params![key, element_id],
    )?;
    if inserted == 1 {
        return Ok(());
    }
    let existing: i64 = connection.query_row(
        "SELECT owner_id FROM temp._lithograph_constraint_validation_seen WHERE key = ?1",
        [key],
        |row| row.get(0),
    )?;
    Err(QueryError::constraint(format!(
        "constraint {} conflicts between {element} {existing} and {element} {element_id}",
        constraint.name
    )))
}

fn validate_property_rule(
    owner: &str,
    element: &str,
    element_id: i64,
    property: &str,
    rule: &storage::PropertyRule,
    value: Option<&PropertyValue>,
) -> QueryResult<()> {
    let Some(value) = value else {
        if rule.required {
            return Err(QueryError::constraint(format!(
                "{owner} requires {element} {element_id} property {property}"
            )));
        }
        return Ok(());
    };
    if !rule.accepts(value) {
        return Err(QueryError::constraint(format!(
            "{owner} rejects the type of {element} {element_id} property {property}"
        )));
    }
    Ok(())
}
