use serde_json::{Map, Value};

use super::{
    AccessPolicy, Arc, BTreeSet, PropertyRules, QueryError, QueryResult, ReadAccess, Regex,
    WriteAccess,
};

const PROPERTY_KEYS: &[&str] = &[
    "denyProperties",
    "denyPropertyPrefixes",
    "denyPropertyPatterns",
];

impl AccessPolicy {
    pub(crate) fn parse(options: &Map<String, Value>) -> QueryResult<Self> {
        Ok(Self {
            read: options
                .get("readAccess")
                .map(parse_read)
                .transpose()?
                .unwrap_or_default(),
            write: options
                .get("writeAccess")
                .map(parse_write)
                .transpose()?
                .unwrap_or_default(),
        })
    }
}

fn parse_read(value: &Value) -> QueryResult<ReadAccess> {
    let object = object(value, "readAccess")?;
    validate_keys(
        object,
        "readAccess",
        &[
            "query",
            "result",
            "denyFunctions",
            "allowNamedProcedures",
            "allowShow",
            "allowExplain",
            "allowProfile",
            "allowExternalIO",
        ],
    )?;
    let mut deny_functions = BTreeSet::new();
    for name in strings(object, "denyFunctions", "readAccess")? {
        if !crate::query::registry::is_function(&name) {
            return Err(QueryError::invalid_argument(format!(
                "options.readAccess.denyFunctions contains an unknown function {name}"
            )));
        }
        deny_functions.insert(name.to_ascii_lowercase());
    }
    Ok(ReadAccess {
        query: optional_rules(object, "query")?,
        result: optional_rules(object, "result")?,
        deny_functions,
        allow_named_procedures: boolean(object, "allowNamedProcedures", "readAccess")?,
        allow_show: boolean(object, "allowShow", "readAccess")?,
        allow_explain: boolean(object, "allowExplain", "readAccess")?,
        allow_profile: boolean(object, "allowProfile", "readAccess")?,
        allow_external_io: boolean(object, "allowExternalIO", "readAccess")?,
    })
}

fn parse_write(value: &Value) -> QueryResult<WriteAccess> {
    let object = object(value, "writeAccess")?;
    let mut allowed_keys = PROPERTY_KEYS.to_vec();
    allowed_keys.extend([
        "allowCreate",
        "allowDelete",
        "allowLabelChanges",
        "allowSchemaChanges",
        "allowVersionChanges",
        "allowMaintenance",
    ]);
    validate_keys(object, "writeAccess", &allowed_keys)?;
    Ok(WriteAccess {
        properties: parse_rules(object, "writeAccess")?,
        allow_create: boolean(object, "allowCreate", "writeAccess")?,
        allow_delete: boolean(object, "allowDelete", "writeAccess")?,
        allow_label_changes: boolean(object, "allowLabelChanges", "writeAccess")?,
        allow_schema_changes: boolean(object, "allowSchemaChanges", "writeAccess")?,
        allow_version_changes: boolean(object, "allowVersionChanges", "writeAccess")?,
        allow_maintenance: boolean(object, "allowMaintenance", "writeAccess")?,
    })
}

fn optional_rules(object: &Map<String, Value>, key: &str) -> QueryResult<PropertyRules> {
    match object.get(key) {
        None => Ok(PropertyRules::default()),
        Some(value) => {
            let path = format!("readAccess.{key}");
            let object = self::object(value, &path)?;
            validate_keys(object, &path, PROPERTY_KEYS)?;
            parse_rules(object, &path)
        }
    }
}

fn parse_rules(object: &Map<String, Value>, path: &str) -> QueryResult<PropertyRules> {
    let mut patterns = Vec::new();
    for source in strings(object, "denyPropertyPatterns", path)? {
        let regex = Regex::new(&format!(r"\A(?:{source})\z")).map_err(|_| {
            QueryError::invalid_argument(format!(
                "options.{path}.denyPropertyPatterns contains an invalid regular expression"
            ))
        })?;
        patterns.push((source, Arc::new(regex)));
    }
    Ok(PropertyRules {
        names: strings(object, "denyProperties", path)?,
        prefixes: strings(object, "denyPropertyPrefixes", path)?,
        patterns,
    })
}

fn strings(object: &Map<String, Value>, key: &str, path: &str) -> QueryResult<BTreeSet<String>> {
    let Some(value) = object.get(key) else {
        return Ok(BTreeSet::new());
    };
    let array = value.as_array().ok_or_else(|| {
        QueryError::invalid_argument(format!("options.{path}.{key} must be a string array"))
    })?;
    array
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    QueryError::invalid_argument(format!(
                        "options.{path}.{key} must contain non-empty strings"
                    ))
                })
        })
        .collect()
}

fn boolean(object: &Map<String, Value>, key: &str, path: &str) -> QueryResult<bool> {
    match object.get(key) {
        None => Ok(true),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(QueryError::invalid_argument(format!(
            "options.{path}.{key} must be a Boolean"
        ))),
    }
}

fn object<'a>(value: &'a Value, path: &str) -> QueryResult<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| QueryError::invalid_argument(format!("options.{path} must be an object")))
}

fn validate_keys(object: &Map<String, Value>, path: &str, allowed: &[&str]) -> QueryResult<()> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(QueryError::invalid_argument(format!(
                "unknown options.{path} member {key}"
            )));
        }
    }
    Ok(())
}
