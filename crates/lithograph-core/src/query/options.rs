use super::{QueryError, QueryResult};
use serde_json::{Map, Value as JsonValue};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotSelector {
    Current,
    Branch(String),
    Commit(String),
    Tag(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphViewSelector {
    pub require_all_labels: BTreeSet<String>,
    pub exclude_any_labels: BTreeSet<String>,
}
impl GraphViewSelector {
    pub fn is_full_graph(&self) -> bool {
        self.require_all_labels.is_empty() && self.exclude_any_labels.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionOptions {
    pub snapshot: SnapshotSelector,
    pub graph_view: GraphViewSelector,
    pub(crate) write_branch: Option<String>,
    pub(crate) author: Option<String>,
    pub(crate) message: Option<String>,
}
impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            snapshot: SnapshotSelector::Current,
            graph_view: GraphViewSelector::default(),
            write_branch: Some("main".to_owned()),
            author: None,
            message: None,
        }
    }
}
impl ExecutionOptions {
    pub fn parse_text(text: &str) -> QueryResult<Self> {
        let value: JsonValue = serde_json::from_str(text)
            .map_err(|_| QueryError::invalid_argument("options must contain valid JSON"))?;
        Self::parse(&value)
    }
    pub fn parse(value: &JsonValue) -> QueryResult<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| QueryError::invalid_argument("options must be a JSON object"))?;
        validate_top_level_keys(object)?;
        let author = optional_nullable_string(object, "author")?;
        let message = optional_nullable_string(object, "message")?;
        let (snapshot, write_branch) = parse_snapshot_option(object)?;
        let graph_view = parse_graph_view_option(object)?;
        Ok(Self {
            snapshot,
            graph_view,
            write_branch,
            author,
            message,
        })
    }
}

fn parse_snapshot_option(
    object: &Map<String, JsonValue>,
) -> QueryResult<(SnapshotSelector, Option<String>)> {
    let branch = optional_nonempty_string(object, "branch")?;
    let at = optional_nonempty_string(object, "at")?;
    if branch.is_some() && at.is_some() {
        return Err(QueryError::invalid_argument(
            "options.branch and options.at are mutually exclusive",
        ));
    }
    let (snapshot, write_branch) = if let Some(branch) = branch {
        (SnapshotSelector::Branch(branch.clone()), Some(branch))
    } else if let Some(at) = at {
        (parse_at(&at)?, None)
    } else {
        (SnapshotSelector::Current, Some("main".to_owned()))
    };
    Ok((snapshot, write_branch))
}

fn parse_graph_view_option(object: &Map<String, JsonValue>) -> QueryResult<GraphViewSelector> {
    match object.get("graphView") {
        None => Ok(GraphViewSelector::default()),
        Some(JsonValue::Object(value)) => parse_graph_view(value),
        Some(_) => Err(QueryError::invalid_argument(
            "options.graphView must be an object when present",
        )),
    }
}

fn optional_nullable_string(
    object: &Map<String, JsonValue>,
    key: &str,
) -> QueryResult<Option<String>> {
    match object.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(QueryError::invalid_argument(format!(
            "options.{key} must be a string or null"
        ))),
    }
}

fn validate_top_level_keys(object: &Map<String, JsonValue>) -> QueryResult<()> {
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "branch" | "at" | "author" | "message" | "graphView"
        ) {
            return Err(QueryError::invalid_argument(format!(
                "unknown execution option {key}"
            )));
        }
    }
    Ok(())
}

fn optional_nonempty_string(
    object: &Map<String, JsonValue>,
    key: &str,
) -> QueryResult<Option<String>> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    let text = value.as_str().ok_or_else(|| {
        QueryError::invalid_argument(format!("options.{key} must be a non-empty string"))
    })?;
    if text.is_empty() {
        return Err(QueryError::invalid_argument(format!(
            "options.{key} must be a non-empty string"
        )));
    }
    Ok(Some(text.to_owned()))
}

fn parse_at(value: &str) -> QueryResult<SnapshotSelector> {
    let Some((kind, name)) = value.split_once('/') else {
        return Err(QueryError::invalid_argument(
            "options.at must use commit/<id>, branch/<name>, or tag/<name>",
        ));
    };
    if name.is_empty() {
        return Err(QueryError::invalid_argument(
            "options.at must contain a non-empty selector value",
        ));
    }
    match kind {
        "commit" if !name.contains('/') => Ok(SnapshotSelector::Commit(name.to_owned())),
        "commit" => Err(QueryError::invalid_argument(
            "options.at Commit selector must contain exactly one id",
        )),
        "branch" => Ok(SnapshotSelector::Branch(name.to_owned())),
        "tag" => Ok(SnapshotSelector::Tag(name.to_owned())),
        _ => Err(QueryError::invalid_argument(
            "options.at must use commit/<id>, branch/<name>, or tag/<name>",
        )),
    }
}

fn parse_graph_view(object: &Map<String, JsonValue>) -> QueryResult<GraphViewSelector> {
    for key in object.keys() {
        if !matches!(key.as_str(), "requireAllLabels" | "excludeAnyLabels") {
            return Err(QueryError::invalid_argument(format!(
                "unknown options.graphView member {key}"
            )));
        }
    }
    let require_all_labels = label_set(object, "requireAllLabels")?;
    let exclude_any_labels = label_set(object, "excludeAnyLabels")?;
    if let Some(label) = require_all_labels.intersection(&exclude_any_labels).next() {
        return Err(QueryError::invalid_argument(format!(
            "graphView label {label} cannot be both required and excluded"
        )));
    }
    Ok(GraphViewSelector {
        require_all_labels,
        exclude_any_labels,
    })
}

fn label_set(object: &Map<String, JsonValue>, key: &str) -> QueryResult<BTreeSet<String>> {
    let Some(value) = object.get(key) else {
        return Ok(BTreeSet::new());
    };
    let array = value.as_array().ok_or_else(|| {
        QueryError::invalid_argument(format!("options.graphView.{key} must be a string array"))
    })?;
    let mut labels = BTreeSet::new();
    for value in array {
        let label = value.as_str().ok_or_else(|| {
            QueryError::invalid_argument(format!(
                "options.graphView.{key} must contain only strings"
            ))
        })?;
        labels.insert(label.to_owned());
    }
    Ok(labels)
}
