//! Frozen `CY25-2026.08` current-graph function inventory.

use crate::cypher::{AGGREGATING_FUNCTIONS, SCALAR_FUNCTIONS};

pub(crate) fn is_function(name: &str) -> bool {
    crate::cypher::is_builtin_function(name)
}

pub(crate) fn is_aggregating(name: &str) -> bool {
    crate::cypher::is_aggregating_function(name)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FunctionDefinition {
    pub(crate) name: &'static str,
    pub(crate) category: &'static str,
    pub(crate) description: &'static str,
    pub(crate) aggregating: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProcedureDefinition {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) mode: &'static str,
    pub(crate) works_on_system: bool,
    pub(crate) signature: &'static str,
    pub(crate) admin: bool,
    pub(crate) outputs: &'static [&'static str],
}

#[derive(Debug, Clone)]
pub(crate) struct FunctionArgumentDefinition {
    pub(crate) name: String,
    pub(crate) value_type: &'static str,
    pub(crate) optional: bool,
    pub(crate) description: &'static str,
}

impl FunctionDefinition {
    pub(crate) fn display_name(self) -> &'static str {
        canonical_function_name(self.name)
    }

    pub(crate) fn signature(self) -> String {
        let shape = function_shape(self.name);
        let arguments = function_arguments(shape)
            .into_iter()
            .map(|argument| {
                if argument.optional {
                    format!("{} = null :: {}", argument.name, argument.value_type)
                } else {
                    format!("{} :: {}", argument.name, argument.value_type)
                }
            })
            .collect::<Vec<_>>();
        let arguments = if shape.maximum.is_none() {
            format!("{}, ...", arguments.join(", "))
        } else {
            arguments.join(", ")
        };
        format!(
            "{}({arguments}) :: {}",
            self.display_name(),
            shape.return_type
        )
    }

    pub(crate) fn arguments(self) -> Vec<FunctionArgumentDefinition> {
        function_arguments(function_shape(self.name))
    }

    pub(crate) fn return_description(self) -> &'static str {
        function_shape(self.name).return_type
    }
}

fn canonical_function_name(name: &'static str) -> &'static str {
    match name {
        "allreduce" => "allReduce",
        "coll.indexof" => "coll.indexOf",
        "duration.indays" => "duration.inDays",
        "duration.inmonths" => "duration.inMonths",
        "duration.inseconds" => "duration.inSeconds",
        "db.namefromelementid" => "db.nameFromElementId",
        "elementid" => "elementId",
        "endnode" => "endNode",
        "isempty" => "isEmpty",
        "isnan" => "isNaN",
        "nullif" => "nullIf",
        "percentilecont" => "percentileCont",
        "percentiledisc" => "percentileDisc",
        "point.withinbbox" => "point.withinBBox",
        "randomuuid" => "randomUUID",
        "startnode" => "startNode",
        "stdev" => "stDev",
        "stdevp" => "stDevP",
        "string.indexof" => "string.indexOf",
        "string.regexreplace" => "string.regexReplace",
        "toboolean" => "toBoolean",
        "tobooleanlist" => "toBooleanList",
        "tobooleanornull" => "toBooleanOrNull",
        "tofloat" => "toFloat",
        "tofloatlist" => "toFloatList",
        "tofloatornull" => "toFloatOrNull",
        "tointeger" => "toInteger",
        "tointegerlist" => "toIntegerList",
        "tointegerornull" => "toIntegerOrNull",
        "tolower" => "toLower",
        "tostring" => "toString",
        "tostringlist" => "toStringList",
        "tostringornull" => "toStringOrNull",
        "toupper" => "toUpper",
        "uuid.leastsignificantbits" => "uuid.leastSignificantBits",
        "uuid.mostsignificantbits" => "uuid.mostSignificantBits",
        "valuetype" => "valueType",
        _ => name,
    }
}

const PROCEDURES: &[ProcedureDefinition] = &[
    ProcedureDefinition {
        name: "db.labels",
        description: "Lists labels present in the current graph view.",
        mode: "READ",
        works_on_system: false,
        signature: "db.labels() :: (label :: STRING)",
        admin: false,
        outputs: &["label"],
    },
    ProcedureDefinition {
        name: "db.propertyKeys",
        description: "Lists property keys present in the current graph view.",
        mode: "READ",
        works_on_system: false,
        signature: "db.propertyKeys() :: (propertyKey :: STRING)",
        admin: false,
        outputs: &["propertyKey"],
    },
    ProcedureDefinition {
        name: "db.relationshipTypes",
        description: "Lists relationship types present in the current graph view.",
        mode: "READ",
        works_on_system: false,
        signature: "db.relationshipTypes() :: (relationshipType :: STRING)",
        admin: false,
        outputs: &["relationshipType"],
    },
];

pub(crate) fn functions() -> impl Iterator<Item = FunctionDefinition> {
    AGGREGATING_FUNCTIONS
        .iter()
        .map(|name| FunctionDefinition {
            name,
            category: "Aggregating",
            description: "Built-in current-graph aggregating function.",
            aggregating: true,
        })
        .chain(SCALAR_FUNCTIONS.iter().map(|name| FunctionDefinition {
            name,
            category: function_category(name),
            description: "Built-in current-graph scalar function.",
            aggregating: false,
        }))
}

pub(crate) fn procedures() -> impl Iterator<Item = ProcedureDefinition> {
    PROCEDURES.iter().copied()
}

pub(crate) fn procedure(name: &str) -> Option<ProcedureDefinition> {
    PROCEDURES
        .iter()
        .find(|definition| definition.name.eq_ignore_ascii_case(name))
        .copied()
}

fn function_category(name: &str) -> &'static str {
    if is_temporal_function(name) {
        "Temporal"
    } else if name.starts_with("point") {
        "Spatial"
    } else if name.starts_with("vector_") || name.starts_with("vector.") {
        "Vector"
    } else if is_list_function(name) {
        "List"
    } else if is_predicate_function(name) {
        "Predicate"
    } else if is_numeric_function(name) {
        "Numeric"
    } else if is_string_function(name) {
        "String"
    } else if name.starts_with("db.") {
        "Database"
    } else {
        "Scalar"
    }
}

fn is_temporal_function(name: &str) -> bool {
    name.starts_with("date")
        || name.starts_with("time")
        || name.starts_with("local")
        || name.starts_with("duration")
        || matches!(
            name,
            "format" | "timestamp" | "zoned_datetime" | "zoned_time"
        )
}

fn is_list_function(name: &str) -> bool {
    name.starts_with("coll.")
        || matches!(
            name,
            "keys"
                | "labels"
                | "nodes"
                | "range"
                | "reduce"
                | "relationships"
                | "tail"
                | "tobooleanlist"
                | "tofloatlist"
                | "tointegerlist"
                | "tostringlist"
        )
}

fn is_predicate_function(name: &str) -> bool {
    matches!(
        name,
        "all" | "allreduce" | "any" | "exists" | "isempty" | "none" | "property_exists" | "single"
    )
}

fn is_numeric_function(name: &str) -> bool {
    matches!(
        name,
        "abs"
            | "acos"
            | "asin"
            | "atan"
            | "atan2"
            | "ceil"
            | "ceiling"
            | "cos"
            | "cosh"
            | "cot"
            | "coth"
            | "degrees"
            | "e"
            | "exp"
            | "floor"
            | "haversin"
            | "isnan"
            | "ln"
            | "log"
            | "log10"
            | "pi"
            | "radians"
            | "rand"
            | "round"
            | "sign"
            | "sin"
            | "sinh"
            | "sqrt"
            | "tan"
            | "tanh"
    )
}

fn is_string_function(name: &str) -> bool {
    name.starts_with("string.")
        || matches!(
            name,
            "btrim"
                | "left"
                | "lower"
                | "ltrim"
                | "normalize"
                | "replace"
                | "reverse"
                | "right"
                | "rtrim"
                | "split"
                | "substring"
                | "tolower"
                | "tostring"
                | "tostringornull"
                | "toupper"
                | "trim"
                | "upper"
        )
}

#[derive(Debug, Clone, Copy)]
struct FunctionShape {
    minimum: usize,
    maximum: Option<usize>,
    return_type: &'static str,
}

fn function_shape(name: &str) -> FunctionShape {
    let (minimum, maximum) = function_arity(name);
    FunctionShape {
        minimum,
        maximum,
        return_type: function_return_type(name),
    }
}

fn function_arguments(shape: FunctionShape) -> Vec<FunctionArgumentDefinition> {
    let count = shape.maximum.unwrap_or(shape.minimum.max(1));
    (0..count)
        .map(|index| FunctionArgumentDefinition {
            name: if shape.maximum.is_none() {
                "arguments".to_owned()
            } else if index == 0 {
                "input".to_owned()
            } else {
                format!("argument{}", index + 1)
            },
            value_type: "ANY",
            optional: index >= shape.minimum,
            description: if index >= shape.minimum {
                "Optional function argument."
            } else {
                "Function argument."
            },
        })
        .collect()
}

fn function_arity(name: &str) -> (usize, Option<usize>) {
    if matches!(
        name,
        "percentile_cont" | "percentile_disc" | "percentilecont" | "percentiledisc"
    ) {
        return (2, Some(2));
    }
    if is_aggregating(name) {
        return (1, Some(1));
    }
    match name {
        "e" | "pi" | "rand" | "randomuuid" | "timestamp" => (0, Some(0)),
        "coalesce" => (1, None),
        "date" | "datetime" | "local_datetime" | "local_time" | "localdatetime" | "localtime"
        | "time" | "zoned_datetime" | "zoned_time" => (0, Some(2)),
        "date.realtime"
        | "date.statement"
        | "date.transaction"
        | "datetime.realtime"
        | "datetime.statement"
        | "datetime.transaction"
        | "localdatetime.realtime"
        | "localdatetime.statement"
        | "localdatetime.transaction"
        | "localtime.realtime"
        | "localtime.statement"
        | "localtime.transaction"
        | "time.realtime"
        | "time.statement"
        | "time.transaction" => (0, Some(1)),
        "uuid" => (0, Some(2)),
        "round"
        | "date.truncate"
        | "datetime.truncate"
        | "localdatetime.truncate"
        | "localtime.truncate"
        | "time.truncate" => (1, Some(3)),
        "replace" => (3, Some(4)),
        "substring" | "range" => (2, Some(3)),
        "duration" | "format" | "coll.flatten" | "normalize" | "btrim" | "ltrim" | "rtrim"
        | "trim" => (1, Some(2)),
        "atan2"
        | "datetime.fromepoch"
        | "duration.between"
        | "duration.indays"
        | "duration.inmonths"
        | "duration.inseconds"
        | "duration_between"
        | "left"
        | "nullif"
        | "point.distance"
        | "property_exists"
        | "right"
        | "split"
        | "string.indexof"
        | "string.join"
        | "vector.similarity.cosine"
        | "vector.similarity.euclidean"
        | "vector_norm"
        | "coll.indexof"
        | "coll.remove" => (2, Some(2)),
        "coll.insert"
        | "point.withinbbox"
        | "string.regexreplace"
        | "vector"
        | "vector_distance" => (3, Some(3)),
        _ => (1, Some(1)),
    }
}

fn function_return_type(name: &str) -> &'static str {
    if name == "abs" {
        return "INTEGER | FLOAT";
    }
    if boolean_return(name) {
        return "BOOLEAN";
    }
    if integer_return(name) {
        return "INTEGER";
    }
    if float_return(name) {
        return "FLOAT";
    }
    if string_return(name) {
        return "STRING";
    }
    match name {
        "coll.distinct" | "coll.flatten" | "coll.insert" | "coll.remove" | "coll.sort"
        | "collect" | "collect_list" | "keys" | "labels" | "nodes" | "range" | "relationships"
        | "split" | "tail" | "tobooleanlist" | "tofloatlist" | "tointegerlist" | "tostringlist" => {
            "LIST<ANY>"
        }
        "properties" => "MAP",
        "date" | "date.realtime" | "date.statement" | "date.transaction" | "date.truncate" => {
            "DATE"
        }
        "localtime"
        | "local_time"
        | "localtime.realtime"
        | "localtime.statement"
        | "localtime.transaction"
        | "localtime.truncate" => "LOCAL TIME",
        "time" | "time.realtime" | "time.statement" | "time.transaction" | "time.truncate"
        | "zoned_time" => "ZONED TIME",
        "local_datetime"
        | "localdatetime"
        | "localdatetime.realtime"
        | "localdatetime.statement"
        | "localdatetime.transaction"
        | "localdatetime.truncate" => "LOCAL DATETIME",
        "datetime"
        | "datetime.fromepoch"
        | "datetime.fromepochmillis"
        | "datetime.realtime"
        | "datetime.statement"
        | "datetime.transaction"
        | "datetime.truncate"
        | "zoned_datetime" => "ZONED DATETIME",
        "duration" | "duration.between" | "duration.indays" | "duration.inmonths"
        | "duration.inseconds" | "duration_between" => "DURATION",
        "point" => "POINT",
        "uuid" => "UUID",
        "vector" => "VECTOR",
        _ => "ANY",
    }
}

fn boolean_return(name: &str) -> bool {
    matches!(
        name,
        "all"
            | "allreduce"
            | "any"
            | "exists"
            | "isempty"
            | "isnan"
            | "none"
            | "point.withinbbox"
            | "property_exists"
            | "single"
            | "toboolean"
            | "tobooleanornull"
    )
}

fn integer_return(name: &str) -> bool {
    matches!(
        name,
        "cardinality"
            | "char_length"
            | "character_length"
            | "count"
            | "id"
            | "length"
            | "path_length"
            | "sign"
            | "size"
            | "string.indexof"
            | "tointeger"
            | "tointegerornull"
            | "uuid.leastsignificantbits"
            | "uuid.mostsignificantbits"
            | "vector_dimension_count"
    )
}

fn float_return(name: &str) -> bool {
    matches!(
        name,
        "acos"
            | "asin"
            | "atan"
            | "atan2"
            | "avg"
            | "ceil"
            | "ceiling"
            | "cos"
            | "cosh"
            | "cot"
            | "coth"
            | "degrees"
            | "e"
            | "exp"
            | "floor"
            | "haversin"
            | "ln"
            | "log"
            | "log10"
            | "percentile_cont"
            | "percentile_disc"
            | "percentilecont"
            | "percentiledisc"
            | "pi"
            | "point.distance"
            | "radians"
            | "rand"
            | "round"
            | "sin"
            | "sinh"
            | "sqrt"
            | "stdev"
            | "stdev_pop"
            | "stdev_samp"
            | "stdevp"
            | "tan"
            | "tanh"
            | "tofloat"
            | "tofloatornull"
            | "vector.similarity.cosine"
            | "vector.similarity.euclidean"
            | "vector_distance"
            | "vector_norm"
    )
}

fn string_return(name: &str) -> bool {
    matches!(
        name,
        "btrim"
            | "db.namefromelementid"
            | "elementid"
            | "format"
            | "left"
            | "lower"
            | "ltrim"
            | "normalize"
            | "randomuuid"
            | "replace"
            | "right"
            | "rtrim"
            | "string.join"
            | "string.regexreplace"
            | "substring"
            | "tolower"
            | "tostring"
            | "tostringornull"
            | "toupper"
            | "trim"
            | "type"
            | "upper"
            | "valuetype"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_scalar_has_a_runtime_or_syntax_owner() {
        const CONTEXT_OWNED: &[&str] = &[
            "all",
            "allreduce",
            "any",
            "endnode",
            "exists",
            "none",
            "reduce",
            "single",
            "startnode",
        ];
        let missing = SCALAR_FUNCTIONS
            .iter()
            .copied()
            .filter(|name| {
                !CONTEXT_OWNED.contains(name)
                    && super::super::functions::evaluate(name, &[]).is_none()
            })
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "registered functions without runtime: {missing:?}"
        );
    }
}
