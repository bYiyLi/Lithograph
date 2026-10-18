//! Allocation preflight for frozen SHOW FUNCTIONS / SHOW PROCEDURES metadata.

use super::*;
use crate::query::registry::{FunctionArgumentDefinition, FunctionDefinition, ProcedureDefinition};
use crate::query::resources::{allocation_bytes, checked_add, checked_mul, map_entry_bytes};

// These are bounds of the frozen builtin inventory, verified for every
// definition by the tests below. They are per definition, never a query-sized
// or whole-registry allowance.
const MAX_OVERLOADS: usize = 3; // uuid has three, reverse has two.
const MAX_ARGUMENTS: usize = 4; // allReduce and replace.
const MAX_ARGUMENT_NAME: usize = "vectorDistanceMetric".len();
const MAX_ARGUMENT_TYPE: usize =
    "DATE | LOCAL TIME | ZONED TIME | LOCAL DATETIME | ZONED DATETIME | DURATION".len();
const MAX_ARGUMENT_DESCRIPTION: usize = "Optional function argument.".len();
const FUNCTION_FIELDS: &[&str] = &[
    "name",
    "category",
    "description",
    "signature",
    "isBuiltIn",
    "argumentDescription",
    "returnDescription",
    "aggregating",
    "rolesExecution",
    "rolesBoostedExecution",
    "isDeprecated",
    "deprecatedBy",
];
const PROCEDURE_FIELDS: &[&str] = &[
    "name",
    "description",
    "mode",
    "worksOnSystem",
    "signature",
    "argumentDescription",
    "returnDescription",
    "admin",
    "rolesExecution",
    "rolesBoostedExecution",
    "isDeprecated",
    "deprecatedBy",
    "option",
];
const ARGUMENT_FIELDS: &[&str] = &["name", "type", "default", "isDeprecated", "description"];
const RETURN_FIELDS: &[&str] = &["name", "type", "isDeprecated", "description"];

pub(crate) fn function_registry_rows(
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<BindingRow>, MemoryReservation)> {
    let maximum_definitions = crate::cypher::AGGREGATING_FUNCTIONS
        .len()
        .saturating_add(crate::cypher::SCALAR_FUNCTIONS.len())
        .saturating_mul(MAX_OVERLOADS);
    let longest_name = crate::cypher::AGGREGATING_FUNCTIONS
        .iter()
        .chain(crate::cypher::SCALAR_FUNCTIONS.iter())
        .map(|name| name.len())
        .max()
        .unwrap_or(0);
    let _definitions_memory = control.reserve(checked_add(
        allocation_bytes::<FunctionDefinition>(maximum_definitions.saturating_mul(4))?,
        checked_mul(longest_name as u64, 2)?,
    )?)?;
    let mut definitions = crate::query::registry::functions().collect::<Vec<_>>();
    definitions.sort_by_key(|definition| definition.name.to_ascii_lowercase());
    let mut memory = control.reserve(allocation_bytes::<BindingRow>(definitions.len())?)?;
    let mut rows = Vec::with_capacity(definitions.len());
    for definition in definitions {
        control.checkpoint(1)?;
        let bound = function_metadata_bytes(definition)?;
        let mut row_memory = control.reserve(bound)?;
        let row = function_registry_row(definition)?;
        retain_metadata_row(&row, bound, &mut row_memory, control)?;
        memory.merge(row_memory)?;
        rows.push(row);
    }
    Ok((rows, memory))
}

pub(crate) fn procedure_registry_rows(
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<BindingRow>, MemoryReservation)> {
    let count = crate::query::registry::procedures().count();
    let mut memory = control.reserve(allocation_bytes::<BindingRow>(count)?)?;
    let mut rows = Vec::with_capacity(count);
    for definition in crate::query::registry::procedures() {
        control.checkpoint(1)?;
        let bound = procedure_metadata_bytes(definition)?;
        let mut row_memory = control.reserve(bound)?;
        let row = procedure_registry_row(definition);
        retain_metadata_row(&row, bound, &mut row_memory, control)?;
        memory.merge(row_memory)?;
        rows.push(row);
    }
    Ok((rows, memory))
}

fn retain_metadata_row(
    row: &BindingRow,
    bound: u64,
    memory: &mut MemoryReservation,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let retained = expression::binding_row_bytes_controlled(row, Some(control))?;
    if retained > bound {
        return Err(QueryError::internal(
            "frozen registry metadata exceeded its preflight bound",
        ));
    }
    memory.resize(retained)
}

// Every BindingRow field owns a map key, a scalar slot, and a second key in
// its Vec order. Sixteen order slots cover the twelve/thirteen fixed fields.
fn metadata_row_bytes(fields: &[&str]) -> QueryResult<u64> {
    let entries = checked_mul(
        fields.len() as u64,
        checked_add(
            map_entry_bytes::<BindingValue>()?,
            std::mem::size_of::<Value>() as u64,
        )?,
    )?;
    let keys = fields.iter().try_fold(0, |bytes, key| {
        checked_add(bytes, checked_mul(key.len() as u64, 2)?)
    })?;
    checked_add(
        checked_add(entries, keys)?,
        checked_add(
            allocation_bytes::<BindingRow>(1)?,
            allocation_bytes::<String>(16)?,
        )?,
    )
}

fn metadata_map_bytes(fields: &[&str]) -> QueryResult<u64> {
    fields.iter().try_fold(0, |bytes, field| {
        checked_add(
            bytes,
            checked_add(map_entry_bytes::<Value>()?, field.len() as u64)?,
        )
    })
}

fn signature_bound(definition: FunctionDefinition) -> QueryResult<u64> {
    let argument = MAX_ARGUMENT_NAME + MAX_ARGUMENT_TYPE + "[,  :: ]".len();
    let arguments = checked_mul(MAX_ARGUMENTS as u64, argument as u64)?;
    let returned = definition.return_description().unwrap_or("").len() as u64;
    checked_add(
        arguments,
        checked_add(
            definition.display_name().len() as u64 + "() :: ".len() as u64,
            returned,
        )?,
    )
}

fn function_metadata_bytes(definition: FunctionDefinition) -> QueryResult<u64> {
    let signature = signature_bound(definition)?;
    let argument_strings =
        (MAX_ARGUMENT_NAME + MAX_ARGUMENT_TYPE + MAX_ARGUMENT_DESCRIPTION) as u64;
    let arguments = checked_mul(
        MAX_ARGUMENTS as u64,
        checked_add(metadata_map_bytes(ARGUMENT_FIELDS)?, argument_strings)?,
    )?;
    let strings = (definition.display_name().len()
        + definition.category.len()
        + definition.description.len()
        + definition.return_description().unwrap_or("").len()
        + definition.deprecated_by().unwrap_or("").len()) as u64;
    let retained = checked_add(
        metadata_row_bytes(FUNCTION_FIELDS)?,
        checked_add(arguments, checked_add(strings, checked_mul(signature, 2)?)?)?,
    )?;
    checked_add(retained, function_temporary_bytes(signature)?)
}

fn function_temporary_bytes(signature: u64) -> QueryResult<u64> {
    let metadata_temporary = checked_add(
        allocation_bytes::<FunctionArgumentDefinition>(MAX_ARGUMENTS.saturating_mul(2))?,
        checked_mul(MAX_ARGUMENTS as u64, MAX_ARGUMENT_NAME as u64 * 2)?,
    )?;
    let rendering_temporary = checked_add(
        allocation_bytes::<String>(MAX_ARGUMENTS)?,
        checked_mul(signature, 8)?,
    )?;
    checked_add(
        allocation_bytes::<Value>(MAX_ARGUMENTS)?,
        checked_add(metadata_temporary, rendering_temporary)?,
    )
}

fn procedure_metadata_bytes(definition: ProcedureDefinition) -> QueryResult<u64> {
    let mut returns = allocation_bytes::<Value>(definition.outputs.len())?;
    for name in definition.outputs {
        returns = checked_add(
            returns,
            checked_add(
                metadata_map_bytes(RETURN_FIELDS)?,
                (name.len() + "STRING".len() + "Procedure output field.".len()) as u64,
            )?,
        )?;
    }
    let strings = (definition.name.len()
        + definition.description.len()
        + definition.mode.len()
        + definition.signature.len()) as u64;
    checked_add(
        metadata_row_bytes(PROCEDURE_FIELDS)?,
        checked_add(
            strings,
            checked_add(returns, metadata_map_bytes(&["deprecated"])?)?,
        )?,
    )
}

fn function_registry_row(definition: FunctionDefinition) -> QueryResult<BindingRow> {
    let arguments = definition.arguments().ok_or_else(|| {
        QueryError::internal(format!(
            "registered function {} is missing frozen argument metadata",
            definition.display_name()
        ))
    })?;
    let arguments = arguments
        .into_iter()
        .map(|argument| {
            Value::Map(BTreeMap::from([
                ("name".to_owned(), Value::String(argument.name)),
                (
                    "type".to_owned(),
                    Value::String(argument.value_type.to_owned()),
                ),
                ("default".to_owned(), Value::Null),
                ("isDeprecated".to_owned(), Value::Boolean(false)),
                (
                    "description".to_owned(),
                    Value::String(argument.description.to_owned()),
                ),
            ]))
        })
        .collect();
    let signature = definition.signature().ok_or_else(|| {
        QueryError::internal(format!(
            "registered function {} is missing frozen signature metadata",
            definition.display_name()
        ))
    })?;
    let return_description = definition.return_description().ok_or_else(|| {
        QueryError::internal(format!(
            "registered function {} is missing frozen return metadata",
            definition.display_name()
        ))
    })?;
    Ok(registry_binding([
        ("name", Value::String(definition.display_name().to_owned())),
        ("category", Value::String(definition.category.to_owned())),
        (
            "description",
            Value::String(definition.description.to_owned()),
        ),
        ("signature", Value::String(signature)),
        ("isBuiltIn", Value::Boolean(true)),
        ("argumentDescription", Value::List(arguments)),
        (
            "returnDescription",
            Value::String(return_description.to_owned()),
        ),
        ("aggregating", Value::Boolean(definition.aggregating)),
        ("rolesExecution", Value::Null),
        ("rolesBoostedExecution", Value::Null),
        ("isDeprecated", Value::Boolean(definition.is_deprecated())),
        (
            "deprecatedBy",
            definition
                .deprecated_by()
                .map_or(Value::Null, |name| Value::String(name.to_owned())),
        ),
    ]))
}

fn procedure_registry_row(definition: ProcedureDefinition) -> BindingRow {
    let returned = definition
        .outputs
        .iter()
        .map(|name| {
            Value::Map(BTreeMap::from([
                ("name".to_owned(), Value::String((*name).to_owned())),
                ("type".to_owned(), Value::String("STRING".to_owned())),
                ("isDeprecated".to_owned(), Value::Boolean(false)),
                (
                    "description".to_owned(),
                    Value::String("Procedure output field.".to_owned()),
                ),
            ]))
        })
        .collect();
    registry_binding([
        ("name", Value::String(definition.name.to_owned())),
        (
            "description",
            Value::String(definition.description.to_owned()),
        ),
        ("mode", Value::String(definition.mode.to_owned())),
        ("worksOnSystem", Value::Boolean(definition.works_on_system)),
        ("signature", Value::String(definition.signature.to_owned())),
        ("argumentDescription", Value::List(Vec::new())),
        ("returnDescription", Value::List(returned)),
        ("admin", Value::Boolean(definition.admin)),
        ("rolesExecution", Value::Null),
        ("rolesBoostedExecution", Value::Null),
        ("isDeprecated", Value::Boolean(false)),
        ("deprecatedBy", Value::Null),
        (
            "option",
            Value::Map(BTreeMap::from([(
                "deprecated".to_owned(),
                Value::Boolean(false),
            )])),
        ),
    ])
}

#[cfg(test)]
mod tests;
