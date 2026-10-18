//! Compiled, execution-local graph access policy.

use std::collections::BTreeSet;
use std::sync::Arc;

use regex::Regex;

use super::{QueryError, QueryResult};
use crate::cypher::Value;

mod parse;

/// Property-name rules shared by all graph elements in one execution.
#[derive(Debug, Clone, Default)]
pub struct PropertyRules {
    names: BTreeSet<String>,
    prefixes: BTreeSet<String>,
    patterns: Vec<(String, Arc<Regex>)>,
}

impl PartialEq for PropertyRules {
    fn eq(&self, other: &Self) -> bool {
        self.names == other.names
            && self.prefixes == other.prefixes
            && self
                .patterns
                .iter()
                .map(|(source, _)| source)
                .eq(other.patterns.iter().map(|(source, _)| source))
    }
}

impl Eq for PropertyRules {}

impl PropertyRules {
    /// Matches an exact graph property name without normalization.
    pub fn denies(&self, name: &str) -> bool {
        self.names.contains(name)
            || self.prefixes.iter().any(|prefix| name.starts_with(prefix))
            || self.patterns.iter().any(|(_, regex)| regex.is_match(name))
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.prefixes.is_empty() && self.patterns.is_empty()
    }

    /// Filters graph objects while retaining ordinary Map keys and scalar values.
    pub fn filter_graph_properties(&self, value: &Value) -> Value {
        let mut filtered = value.clone();
        self.restrict_graph_properties(&mut filtered);
        filtered
    }

    pub fn restrict_graph_properties(&self, value: &mut Value) {
        if self.is_empty() {
            return;
        }
        match value {
            Value::Node(node) => node.properties.retain(|key, _| !self.denies(key)),
            Value::Relationship(relationship) => {
                relationship.properties.retain(|key, _| !self.denies(key));
            }
            Value::Path(path) => {
                for node in &mut path.nodes {
                    node.properties.retain(|key, _| !self.denies(key));
                }
                for relationship in &mut path.relationships {
                    relationship.properties.retain(|key, _| !self.denies(key));
                }
            }
            Value::List(values) => {
                for value in values {
                    self.restrict_graph_properties(value);
                }
            }
            Value::Map(values) => {
                for value in values.values_mut() {
                    self.restrict_graph_properties(value);
                }
            }
            _ => {}
        }
    }
}

/// Read restrictions. All capability switches default to the existing behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadAccess {
    pub query: PropertyRules,
    pub result: PropertyRules,
    pub deny_functions: BTreeSet<String>,
    pub allow_named_procedures: bool,
    pub allow_show: bool,
    pub allow_explain: bool,
    pub allow_profile: bool,
    pub allow_external_io: bool,
}

impl Default for ReadAccess {
    fn default() -> Self {
        Self {
            query: PropertyRules::default(),
            result: PropertyRules::default(),
            deny_functions: BTreeSet::new(),
            allow_named_procedures: true,
            allow_show: true,
            allow_explain: true,
            allow_profile: true,
            allow_external_io: true,
        }
    }
}

impl ReadAccess {
    fn is_unrestricted(&self) -> bool {
        self.query.is_empty() && self.result.is_empty() && self.ast_is_unrestricted()
    }

    pub(crate) fn ast_is_unrestricted(&self) -> bool {
        self.deny_functions.is_empty()
            && self.allow_named_procedures
            && self.allow_show
            && self.allow_explain
            && self.allow_profile
            && self.allow_external_io
    }
}

/// Write restrictions; graph property rules do not apply to metadata map keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteAccess {
    pub properties: PropertyRules,
    pub allow_create: bool,
    pub allow_delete: bool,
    pub allow_label_changes: bool,
    pub allow_schema_changes: bool,
    pub allow_version_changes: bool,
    pub allow_maintenance: bool,
}

impl Default for WriteAccess {
    fn default() -> Self {
        Self {
            properties: PropertyRules::default(),
            allow_create: true,
            allow_delete: true,
            allow_label_changes: true,
            allow_schema_changes: true,
            allow_version_changes: true,
            allow_maintenance: true,
        }
    }
}

impl WriteAccess {
    fn is_unrestricted(&self) -> bool {
        self.properties.is_empty()
            && self.allow_create
            && self.allow_delete
            && self.allow_label_changes
            && self.ast_is_unrestricted()
    }

    pub(crate) fn ast_is_unrestricted(&self) -> bool {
        self.allow_schema_changes && self.allow_version_changes && self.allow_maintenance
    }
}

/// An immutable policy compiled once while decoding execution options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessPolicy {
    pub read: ReadAccess,
    pub write: WriteAccess,
}

impl AccessPolicy {
    pub fn is_unrestricted(&self) -> bool {
        self.read.is_unrestricted() && self.write.is_unrestricted()
    }

    pub fn check_write_property(&self, name: &str) -> QueryResult<()> {
        if self.write.properties.denies(name) {
            return Err(QueryError::access_denied(
                "writeAccess forbids writing a protected graph property",
            ));
        }
        Ok(())
    }

    pub fn check_external_io(&self) -> QueryResult<()> {
        require(self.read.allow_external_io, "readAccess.allowExternalIO")
    }
}

pub(crate) fn require(allowed: bool, permission: &str) -> QueryResult<()> {
    if allowed {
        Ok(())
    } else {
        Err(QueryError::access_denied(format!(
            "{permission}=false forbids this operation"
        )))
    }
}
