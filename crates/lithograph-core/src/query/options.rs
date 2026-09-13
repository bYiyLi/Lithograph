use super::{QueryError, QueryResult};
use crate::storage;
use rusqlite::Connection;
use serde_json::{Map, Value as JsonValue};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotSelector {
    Current,
    Branch(String),
    Commit(String),
    Tag(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeSessionSelector {
    pub id: String,
    pub revision: i64,
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
    pub merge_session: Option<MergeSessionSelector>,
    pub(crate) write_branch: Option<String>,
    pub(crate) author: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) author_present: bool,
    pub(crate) message_present: bool,
}
impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            snapshot: SnapshotSelector::Current,
            graph_view: GraphViewSelector::default(),
            merge_session: None,
            write_branch: None,
            author: None,
            message: None,
            author_present: false,
            message_present: false,
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
        let merge_session = parse_merge_session(object)?;
        if merge_session.is_some()
            && (object.contains_key("branch")
                || object.contains_key("at")
                || object.contains_key("author")
                || object.contains_key("message"))
        {
            return Err(QueryError::invalid_argument(
                "options.mergeSession is mutually exclusive with branch, at, author, and message",
            ));
        }
        Ok(Self {
            snapshot,
            graph_view,
            merge_session,
            write_branch,
            author,
            message,
            author_present: object.contains_key("author"),
            message_present: object.contains_key("message"),
        })
    }
}

pub(super) fn writable_branch(
    connection: &Connection,
    options: &ExecutionOptions,
) -> QueryResult<String> {
    match &options.snapshot {
        SnapshotSelector::Current => storage::active_branch(connection).map_err(Into::into),
        SnapshotSelector::Branch(snapshot_branch) => match options.write_branch.as_ref() {
            Some(write_branch) if write_branch == snapshot_branch => Ok(write_branch.clone()),
            Some(_) => Err(QueryError::invalid_argument(
                "execution options contain inconsistent Branch snapshot and write target",
            )),
            None => Err(QueryError::read_only_snapshot(
                "mutating queries cannot execute against options.at historical snapshots",
            )),
        },
        SnapshotSelector::Commit(_) | SnapshotSelector::Tag(_) => {
            Err(QueryError::read_only_snapshot(
                "mutating queries cannot execute against options.at historical snapshots",
            ))
        }
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
        (SnapshotSelector::Current, None)
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

fn parse_merge_session(
    object: &Map<String, JsonValue>,
) -> QueryResult<Option<MergeSessionSelector>> {
    let Some(value) = object.get("mergeSession") else {
        return Ok(None);
    };
    let map = value
        .as_object()
        .ok_or_else(|| QueryError::invalid_argument("options.mergeSession must be an object"))?;
    for key in map.keys() {
        if !matches!(key.as_str(), "id" | "revision") {
            return Err(QueryError::invalid_argument(format!(
                "unknown options.mergeSession member {key}"
            )));
        }
    }
    let id = map
        .get("id")
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            QueryError::invalid_argument("options.mergeSession.id must be a non-empty string")
        })?;
    let revision = map
        .get("revision")
        .and_then(JsonValue::as_i64)
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            QueryError::invalid_argument("options.mergeSession.revision must be a positive integer")
        })?;
    Ok(Some(MergeSessionSelector {
        id: id.to_owned(),
        revision,
    }))
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
            "branch" | "at" | "author" | "message" | "graphView" | "mergeSession"
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
