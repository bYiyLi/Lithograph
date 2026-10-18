//! Bounded copies of query-owned Schema state, after the caller reserves it.

use super::*;
use crate::storage::property::copy_text_checked as text;

type Check<'a> = &'a dyn Fn() -> StorageResult<()>;

impl SchemaState {
    pub(crate) fn clone_checked(&self, check: Check<'_>) -> StorageResult<Self> {
        check()?;
        Ok(Self {
            graph_nodes: clone_map(&self.graph_nodes, check, |value| graph_node(value, check))?,
            graph_relationships: clone_map(&self.graph_relationships, check, |value| {
                graph_relationship(value, check)
            })?,
            constraints: clone_map(&self.constraints, check, |value| constraint(value, check))?,
            indexes: clone_map(&self.indexes, check, |value| index(value, check))?,
        })
    }
}

fn graph_node(value: &GraphNodeType, check: Check<'_>) -> StorageResult<GraphNodeType> {
    let mut implied_labels = BTreeSet::new();
    for label in &value.implied_labels {
        implied_labels.insert(text(label, check)?);
    }
    Ok(GraphNodeType {
        label: text(&value.label, check)?,
        implied_labels,
        properties: clone_map(&value.properties, check, |rule| property_rule(rule, check))?,
    })
}

fn graph_relationship(
    value: &GraphRelationshipType,
    check: Check<'_>,
) -> StorageResult<GraphRelationshipType> {
    Ok(GraphRelationshipType {
        relationship_type: text(&value.relationship_type, check)?,
        source_label: optional_text(value.source_label.as_deref(), check)?,
        source_identifying: value.source_identifying,
        target_label: optional_text(value.target_label.as_deref(), check)?,
        target_identifying: value.target_identifying,
        properties: clone_map(&value.properties, check, |rule| property_rule(rule, check))?,
    })
}

fn constraint(
    value: &ConstraintDefinition,
    check: Check<'_>,
) -> StorageResult<ConstraintDefinition> {
    let kind = match &value.kind {
        ConstraintDefinitionKind::Type { rule } => ConstraintDefinitionKind::Type {
            rule: property_rule(rule, check)?,
        },
        kind => kind.clone(),
    };
    Ok(ConstraintDefinition {
        name: text(&value.name, check)?,
        target: schema_target(&value.target, check)?,
        properties: texts(&value.properties, check)?,
        kind,
        origin: optional_text(value.origin.as_deref(), check)?,
    })
}

fn index(value: &IndexDefinition, check: Check<'_>) -> StorageResult<IndexDefinition> {
    Ok(IndexDefinition {
        name: text(&value.name, check)?,
        kind: value.kind,
        target: index_target(&value.target, check)?,
        owning_constraint: optional_text(value.owning_constraint.as_deref(), check)?,
        labels_or_types: texts(&value.labels_or_types, check)?,
        additional_properties: texts(&value.additional_properties, check)?,
        configuration: value
            .configuration
            .as_ref()
            .map(|value| configuration(value, check))
            .transpose()?,
    })
}

fn clone_map<T>(
    values: &BTreeMap<String, T>,
    check: Check<'_>,
    clone: impl Fn(&T) -> StorageResult<T>,
) -> StorageResult<BTreeMap<String, T>> {
    let mut output = BTreeMap::new();
    for (key, value) in values {
        check()?;
        output.insert(text(key, check)?, clone(value)?);
    }
    Ok(output)
}

fn property_rule(value: &PropertyRule, check: Check<'_>) -> StorageResult<PropertyRule> {
    Ok(PropertyRule {
        required: value.required,
        property_type: property_type(&value.property_type, check)?,
    })
}

fn property_type(value: &PropertyType, check: Check<'_>) -> StorageResult<PropertyType> {
    check()?;
    match value {
        PropertyType::Vector {
            coordinate,
            dimension,
        } => Ok(PropertyType::Vector {
            coordinate: text(coordinate, check)?,
            dimension: *dimension,
        }),
        PropertyType::List { element } => Ok(PropertyType::List {
            element: Box::new(property_type(element, check)?),
        }),
        PropertyType::Union { members } => {
            let mut output = Vec::with_capacity(members.len());
            for member in members {
                output.push(property_type(member, check)?);
            }
            Ok(PropertyType::Union { members: output })
        }
        value => Ok(value.clone()),
    }
}

fn schema_target(value: &SchemaTarget, check: Check<'_>) -> StorageResult<SchemaTarget> {
    match value {
        SchemaTarget::Node { label } => Ok(SchemaTarget::Node {
            label: text(label, check)?,
        }),
        SchemaTarget::Relationship { relationship_type } => Ok(SchemaTarget::Relationship {
            relationship_type: text(relationship_type, check)?,
        }),
    }
}

fn index_target(value: &IndexTarget, check: Check<'_>) -> StorageResult<IndexTarget> {
    match value {
        IndexTarget::NodeProperties { label, properties } => Ok(IndexTarget::NodeProperties {
            label: text(label, check)?,
            properties: texts(properties, check)?,
        }),
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => Ok(IndexTarget::RelationshipProperties {
            relationship_type: text(relationship_type, check)?,
            properties: texts(properties, check)?,
        }),
        value => Ok(value.clone()),
    }
}

fn configuration(
    value: &IndexConfiguration,
    check: Check<'_>,
) -> StorageResult<IndexConfiguration> {
    check()?;
    match value {
        IndexConfiguration::FullText {
            analyzer,
            eventually_consistent,
        } => Ok(IndexConfiguration::FullText {
            analyzer: text(analyzer, check)?,
            eventually_consistent: *eventually_consistent,
        }),
        IndexConfiguration::Vector {
            dimensions,
            similarity_function,
            quantization_type,
            default_search_expansion_factor,
            hnsw_m,
            hnsw_ef_construction,
        } => Ok(IndexConfiguration::Vector {
            dimensions: *dimensions,
            similarity_function: text(similarity_function, check)?,
            quantization_type: text(quantization_type, check)?,
            default_search_expansion_factor: text(default_search_expansion_factor, check)?,
            hnsw_m: *hnsw_m,
            hnsw_ef_construction: *hnsw_ef_construction,
        }),
        IndexConfiguration::Semantic {
            provider,
            provider_config,
            dimensions,
            similarity_function,
        } => Ok(IndexConfiguration::Semantic {
            provider: text(provider, check)?,
            provider_config: json(provider_config, check)?,
            dimensions: *dimensions,
            similarity_function: text(similarity_function, check)?,
        }),
    }
}

fn json(value: &JsonValue, check: Check<'_>) -> StorageResult<JsonValue> {
    check()?;
    match value {
        JsonValue::String(value) => Ok(JsonValue::String(text(value, check)?)),
        JsonValue::Array(values) => {
            let mut output = Vec::with_capacity(values.len());
            for value in values {
                output.push(json(value, check)?);
            }
            Ok(JsonValue::Array(output))
        }
        JsonValue::Object(values) => {
            let mut output = serde_json::Map::new();
            for (key, value) in values {
                output.insert(text(key, check)?, json(value, check)?);
            }
            Ok(JsonValue::Object(output))
        }
        value => Ok(value.clone()),
    }
}

fn optional_text(value: Option<&str>, check: Check<'_>) -> StorageResult<Option<String>> {
    value.map(|value| text(value, check)).transpose()
}

fn texts(values: &[String], check: Check<'_>) -> StorageResult<Vec<String>> {
    check()?;
    let mut output = Vec::with_capacity(values.len());
    for value in values {
        output.push(text(value, check)?);
    }
    Ok(output)
}
