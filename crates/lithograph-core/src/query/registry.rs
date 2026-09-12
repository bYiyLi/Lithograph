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
    overload: u8,
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

    pub(crate) fn signature(self) -> Option<String> {
        function_signature(self.name, self.overload)
    }

    pub(crate) fn arguments(self) -> Option<Vec<FunctionArgumentDefinition>> {
        function_arguments(self.name, self.overload)
    }

    pub(crate) fn return_description(self) -> Option<&'static str> {
        function_return_type(self.name, self.overload)
    }

    pub(crate) fn is_deprecated(self) -> bool {
        self.name == "id"
    }

    pub(crate) fn deprecated_by(self) -> Option<&'static str> {
        (self.name == "id").then_some("elementId")
    }
}

fn canonical_function_name(name: &'static str) -> &'static str {
    match name {
        "allreduce" => "allReduce",
        "coll.indexof" => "coll.indexOf",
        "datetime.fromepoch" => "datetime.fromEpoch",
        "datetime.fromepochmillis" => "datetime.fromEpochMillis",
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
    let mut definitions = Vec::new();
    definitions.extend(AGGREGATING_FUNCTIONS.iter().map(|name| FunctionDefinition {
        name,
        category: "Aggregating",
        description: "Built-in current-graph aggregating function.",
        aggregating: true,
        overload: 0,
    }));
    for name in SCALAR_FUNCTIONS {
        // PROPERTY_EXISTS is a GQL predicate, not a function exposed by SHOW FUNCTIONS.
        if *name == "property_exists" {
            continue;
        }
        let overload_count = function_overload_count(name);
        definitions.extend((0..overload_count).map(|overload| FunctionDefinition {
            name,
            category: function_category(name, overload),
            description: "Built-in current-graph scalar function.",
            aggregating: false,
            overload,
        }));
    }
    definitions.into_iter()
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

fn function_overload_count(name: &str) -> u8 {
    match name {
        "uuid" => 3,
        "reverse" => 2,
        _ => 1,
    }
}

fn function_category(name: &str, overload: u8) -> &'static str {
    if name == "reverse" {
        return if overload == 0 { "List" } else { "String" };
    }
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
    } else if is_logarithmic_function(name) {
        "Logarithmic"
    } else if is_trigonometric_function(name) {
        "Trigonometric"
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

fn is_logarithmic_function(name: &str) -> bool {
    matches!(name, "e" | "exp" | "ln" | "log" | "log10" | "sqrt")
}

fn is_trigonometric_function(name: &str) -> bool {
    matches!(
        name,
        "acos"
            | "asin"
            | "atan"
            | "atan2"
            | "cos"
            | "cosh"
            | "cot"
            | "coth"
            | "degrees"
            | "haversin"
            | "pi"
            | "radians"
            | "sin"
            | "sinh"
            | "tan"
            | "tanh"
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

fn function_signature(name: &'static str, overload: u8) -> Option<String> {
    let display_name = canonical_function_name(name);
    match (name, overload) {
        ("allreduce", _) => return Some("allReduce(accumulator = initial, stepVariable IN list | reductionFunction, predicate) :: BOOLEAN".to_owned()),
        ("reduce", _) => return Some("reduce(accumulator :: VARIABLE = initial :: ANY, variable :: VARIABLE IN list :: LIST<ANY> expression :: ANY) :: ANY".to_owned()),
        ("trim", _) => return Some("trim([[LEADING | TRAILING | BOTH] [trimCharacterString :: STRING] FROM] input :: STRING) :: STRING".to_owned()),
        ("coll.flatten", _) => return Some("coll.flatten(list :: LIST<ANY>, depth = 1 :: INTEGER) :: LIST<ANY>".to_owned()),
        ("normalize", _) => return Some("normalize(input :: STRING [, normalForm = NFC :: [NFC, NFD, NFKC, NFKD]]) :: STRING".to_owned()),
        ("round", _) => return Some("round(input :: FLOAT [, precision :: INTEGER | FLOAT, mode :: STRING]) :: FLOAT".to_owned()),
        ("date.truncate", _) | ("datetime.truncate", _) | ("localdatetime.truncate", _) | ("localtime.truncate", _) | ("time.truncate", _) => {
            return Some(format!("{display_name}(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: {}", function_return_type(name, overload)?));
        }
        ("date.realtime", _) | ("date.statement", _) | ("date.transaction", _)
        | ("datetime.realtime", _) | ("datetime.statement", _) | ("datetime.transaction", _)
        | ("localdatetime.realtime", _) | ("localdatetime.statement", _) | ("localdatetime.transaction", _)
        | ("localtime.realtime", _) | ("localtime.statement", _) | ("localtime.transaction", _)
        | ("time.realtime", _) | ("time.statement", _) | ("time.transaction", _) => {
            return Some(format!("{display_name}(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: {}", function_return_type(name, overload)?));
        }
        ("date", _) | ("datetime", _) | ("local_datetime", _) | ("local_time", _)
        | ("localdatetime", _) | ("localtime", _) | ("time", _) | ("zoned_datetime", _)
        | ("zoned_time", _) => {
            return Some(format!("{display_name}(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: {}", function_return_type(name, overload)?));
        }
        _ => {}
    }

    let arguments = function_arguments(name, overload)?;
    let required = arguments
        .iter()
        .filter(|argument| !argument.optional)
        .count();
    let mut rendered = arguments
        .iter()
        .take(required)
        .map(|argument| format!("{} :: {}", argument.name, argument.value_type))
        .collect::<Vec<_>>()
        .join(", ");
    for argument in arguments.iter().skip(required) {
        rendered.push_str(&format!("[, {} :: {}]", argument.name, argument.value_type));
    }
    Some(format!(
        "{display_name}({rendered}) :: {}",
        function_return_type(name, overload)?
    ))
}

type ArgumentSpec = (&'static str, &'static str, bool);

fn function_arguments(name: &str, overload: u8) -> Option<Vec<FunctionArgumentDefinition>> {
    let specs = aggregate_predicate_numeric_arguments(name, overload)
        .or_else(|| collection_arguments(name, overload))
        .or_else(|| string_conversion_arguments(name, overload))
        .or_else(|| graph_temporal_arguments(name, overload))
        .or_else(|| uuid_vector_arguments(name, overload))?;
    Some(
        specs
            .iter()
            .map(|(name, value_type, optional)| FunctionArgumentDefinition {
                name: (*name).to_owned(),
                value_type,
                optional: *optional,
                description: if *optional {
                    "Optional function argument."
                } else {
                    "Function argument."
                },
            })
            .collect(),
    )
}

fn aggregate_predicate_numeric_arguments(
    name: &str,
    overload: u8,
) -> Option<&'static [ArgumentSpec]> {
    match (name, overload) {
        ("e" | "pi" | "rand" | "randomuuid" | "timestamp", _) | ("uuid", 0) => Some(&[]),
        ("avg" | "sum", _) => Some(&[("input", "INTEGER | FLOAT | DURATION", false)]),
        ("collect" | "collect_list" | "count" | "max" | "min", _) => {
            Some(&[("input", "ANY", false)])
        }
        ("percentile_cont" | "percentilecont", _) => {
            Some(&[("input", "FLOAT", false), ("percentile", "FLOAT", false)])
        }
        ("percentile_disc" | "percentiledisc", _) => Some(&[
            ("input", "INTEGER | FLOAT", false),
            ("percentile", "FLOAT", false),
        ]),
        ("stdev" | "stdev_pop" | "stdev_samp" | "stdevp", _) => Some(&[("input", "FLOAT", false)]),
        ("all" | "any" | "none" | "single", _) => Some(&[
            ("variable", "ANY", false),
            ("list", "LIST<ANY>", false),
            ("predicate", "ANY", false),
        ]),
        ("allreduce", _) => Some(&[
            ("initial", "ANY", false),
            ("list", "LIST<ANY>", false),
            ("reductionFunction", "ANY", false),
            ("predicate", "ANY", false),
        ]),
        ("exists", _) => Some(&[("input", "ANY", false)]),
        ("isempty", _) => Some(&[("input", "LIST<ANY> | MAP | STRING", false)]),
        ("abs" | "isnan" | "sign", _) => Some(&[("input", "INTEGER | FLOAT", false)]),
        (
            "acos" | "asin" | "atan" | "ceil" | "ceiling" | "cos" | "cosh" | "cot" | "coth"
            | "degrees" | "exp" | "floor" | "haversin" | "ln" | "log" | "log10" | "radians" | "sin"
            | "sinh" | "sqrt" | "tan" | "tanh",
            _,
        ) => Some(&[("input", "FLOAT", false)]),
        ("atan2", _) => Some(&[("y", "FLOAT", false), ("x", "FLOAT", false)]),
        ("round", _) => Some(&[
            ("input", "FLOAT", false),
            ("precision", "INTEGER | FLOAT", true),
            ("mode", "STRING", true),
        ]),
        _ => None,
    }
}

fn collection_arguments(name: &str, overload: u8) -> Option<&'static [ArgumentSpec]> {
    match (name, overload) {
        ("cardinality", _) => Some(&[("input", "MAP | LIST<ANY> | PATH", false)]),
        ("size", _) => Some(&[("input", "STRING | LIST<ANY> | VECTOR", false)]),
        ("head" | "last", _) => Some(&[("list", "LIST<ANY>", false)]),
        ("tail", _) => Some(&[("input", "LIST<ANY>", false)]),
        ("reverse", 0) => Some(&[("input", "LIST<ANY>", false)]),
        ("range", _) => Some(&[
            ("start", "INTEGER", false),
            ("end", "INTEGER", false),
            ("step", "INTEGER", true),
        ]),
        ("reduce", _) => Some(&[
            ("initial", "ANY", false),
            ("list", "LIST<ANY>", false),
            ("expression", "ANY", false),
        ]),
        ("coll.distinct" | "coll.max" | "coll.min" | "coll.sort", _) => {
            Some(&[("list", "LIST<ANY>", false)])
        }
        ("coll.flatten", _) => Some(&[("list", "LIST<ANY>", false), ("depth", "INTEGER", true)]),
        ("coll.indexof", _) => Some(&[("list", "LIST<ANY>", false), ("value", "ANY", false)]),
        ("coll.insert", _) => Some(&[
            ("list", "LIST<ANY>", false),
            ("index", "INTEGER", false),
            ("value", "ANY", false),
        ]),
        ("coll.remove", _) => Some(&[("list", "LIST<ANY>", false), ("index", "INTEGER", false)]),
        ("keys" | "properties", _) => Some(&[("input", "NODE | RELATIONSHIP | MAP", false)]),
        ("labels", _) => Some(&[("input", "NODE", false)]),
        ("nodes" | "relationships" | "length" | "path_length", _) => {
            Some(&[("input", "PATH", false)])
        }
        ("tobooleanlist" | "tostringlist", _) => Some(&[("input", "LIST<ANY>", false)]),
        ("tofloatlist" | "tointegerlist", _) => Some(&[("input", "VECTOR | LIST<ANY>", false)]),
        _ => None,
    }
}

fn string_conversion_arguments(name: &str, overload: u8) -> Option<&'static [ArgumentSpec]> {
    match (name, overload) {
        ("char_length" | "character_length" | "lower" | "tolower" | "upper" | "toupper", _) => {
            Some(&[("input", "STRING", false)])
        }
        ("reverse", 1) => Some(&[("input", "STRING", false)]),
        ("btrim" | "ltrim" | "rtrim", _) => Some(&[
            ("input", "STRING", false),
            ("trimCharacterString", "STRING", true),
        ]),
        ("trim", _) => Some(&[
            ("trimSpecification", "[LEADING, TRAILING, BOTH]", true),
            ("trimCharacterString", "STRING", true),
            ("input", "STRING", false),
        ]),
        ("left" | "right", _) => {
            Some(&[("original", "STRING", false), ("length", "INTEGER", false)])
        }
        ("substring", _) => Some(&[
            ("original", "STRING", false),
            ("start", "INTEGER", false),
            ("length", "INTEGER", true),
        ]),
        ("replace", _) => Some(&[
            ("original", "STRING", false),
            ("search", "STRING", false),
            ("replace", "STRING", false),
            ("limit", "INTEGER", true),
        ]),
        ("split", _) => Some(&[
            ("original", "STRING", false),
            ("splitDelimiters", "STRING | LIST<STRING>", false),
        ]),
        ("string.indexof", _) => Some(&[("input", "STRING", false), ("value", "STRING", false)]),
        ("string.join", _) => Some(&[
            ("input", "LIST<STRING>", false),
            ("delimiter", "STRING", false),
        ]),
        ("string.regexreplace", _) => Some(&[
            ("original", "STRING", false),
            ("regex", "STRING", false),
            ("replacement", "STRING", false),
        ]),
        ("normalize", _) => Some(&[
            ("input", "STRING", false),
            ("normalForm", "[NFC, NFD, NFKC, NFKD]", true),
        ]),
        ("toboolean", _) => Some(&[("input", "BOOLEAN | STRING | INTEGER", false)]),
        ("tofloat", _) => Some(&[("input", "STRING | INTEGER | FLOAT", false)]),
        ("tointeger", _) => Some(&[("input", "BOOLEAN | STRING | INTEGER | FLOAT", false)]),
        (
            "tobooleanornull" | "tofloatornull" | "tointegerornull" | "tostring" | "tostringornull"
            | "valuetype",
            _,
        ) => Some(&[("input", "ANY", false)]),
        ("nullif", _) => Some(&[("v1", "ANY", false), ("v2", "ANY", false)]),
        ("coalesce", _) => Some(&[("input", "ANY", false)]),
        _ => None,
    }
}

fn graph_temporal_arguments(name: &str, overload: u8) -> Option<&'static [ArgumentSpec]> {
    match (name, overload) {
        ("elementid" | "id", _) => Some(&[("input", "NODE | RELATIONSHIP", false)]),
        ("endnode" | "startnode" | "type", _) => Some(&[("input", "RELATIONSHIP", false)]),
        ("db.namefromelementid", _) => Some(&[("elementId", "STRING", false)]),
        ("point", _) => Some(&[("input", "MAP", false)]),
        ("point.distance", _) => Some(&[("from", "POINT", false), ("to", "POINT", false)]),
        ("point.withinbbox", _) => Some(&[
            ("point", "POINT", false),
            ("lowerLeft", "POINT", false),
            ("upperRight", "POINT", false),
        ]),
        ("duration", _) => Some(&[("input", "ANY", false), ("pattern", "STRING", true)]),
        (
            "duration.between" | "duration.indays" | "duration.inmonths" | "duration.inseconds"
            | "duration_between",
            _,
        ) => Some(&[("from", "ANY", false), ("to", "ANY", false)]),
        (
            "date" | "datetime" | "local_datetime" | "local_time" | "localdatetime" | "localtime"
            | "time" | "zoned_datetime" | "zoned_time",
            _,
        ) => Some(&[("input", "ANY", true), ("pattern", "STRING", true)]),
        (
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
            | "time.transaction",
            _,
        ) => Some(&[("timezone", "ANY", true)]),
        (
            "date.truncate"
            | "datetime.truncate"
            | "localdatetime.truncate"
            | "localtime.truncate"
            | "time.truncate",
            _,
        ) => Some(&[
            ("unit", "STRING", false),
            ("input", "ANY", true),
            ("fields", "MAP", true),
        ]),
        ("datetime.fromepoch", _) => Some(&[
            ("seconds", "INTEGER | FLOAT", false),
            ("nanoseconds", "INTEGER | FLOAT", false),
        ]),
        ("datetime.fromepochmillis", _) => Some(&[("milliseconds", "INTEGER | FLOAT", false)]),
        ("format", _) => Some(&[
            (
                "value",
                "DATE | LOCAL TIME | ZONED TIME | LOCAL DATETIME | ZONED DATETIME | DURATION",
                false,
            ),
            ("pattern", "STRING", true),
        ]),
        _ => None,
    }
}

fn uuid_vector_arguments(name: &str, overload: u8) -> Option<&'static [ArgumentSpec]> {
    match (name, overload) {
        ("uuid", 1) => Some(&[("name", "STRING", false)]),
        ("uuid", 2) => Some(&[
            ("mostSigBits", "INTEGER", false),
            ("leastSigBits", "INTEGER", false),
        ]),
        ("uuid.leastsignificantbits" | "uuid.mostsignificantbits", _) => {
            Some(&[("uuid", "UUID", false)])
        }
        ("vector", _) => Some(&[
            ("vectorValue", "STRING | LIST<INTEGER | FLOAT>", false),
            ("dimension", "INTEGER", false),
            (
                "coordinateType",
                "[INTEGER64, INTEGER32, INTEGER16, INTEGER8, FLOAT64, FLOAT32]",
                false,
            ),
        ]),
        ("vector.similarity.cosine" | "vector.similarity.euclidean", _) => Some(&[
            ("a", "VECTOR | LIST<INTEGER | FLOAT>", false),
            ("b", "VECTOR | LIST<INTEGER | FLOAT>", false),
        ]),
        ("vector_dimension_count", _) => Some(&[("vector", "VECTOR", false)]),
        ("vector_distance", _) => Some(&[
            ("vector1", "VECTOR", false),
            ("vector2", "VECTOR", false),
            (
                "vectorDistanceMetric",
                "[EUCLIDEAN, EUCLIDEAN_SQUARED, MANHATTAN, COSINE, DOT, HAMMING]",
                false,
            ),
        ]),
        ("vector_norm", _) => Some(&[
            ("vector", "VECTOR", false),
            ("vectorDistanceMetric", "[EUCLIDEAN, MANHATTAN]", false),
        ]),
        _ => None,
    }
}

fn function_return_type(name: &str, _overload: u8) -> Option<&'static str> {
    if name == "reverse" {
        return Some(if _overload == 0 {
            "LIST<ANY>"
        } else {
            "STRING"
        });
    }
    aggregate_collection_return_type(name)
        .or_else(|| boolean_integer_return_type(name))
        .or_else(|| numeric_return_type(name))
        .or_else(|| string_structural_return_type(name))
        .or_else(|| temporal_return_type(name))
}

fn aggregate_collection_return_type(name: &str) -> Option<&'static str> {
    match name {
        "avg" | "sum" => Some("INTEGER | FLOAT | DURATION"),
        "abs" | "percentile_disc" | "percentiledisc" => Some("INTEGER | FLOAT"),
        "collect" | "collect_list" | "coll.distinct" | "coll.flatten" | "coll.insert"
        | "coll.remove" | "coll.sort" | "tail" => Some("LIST<ANY>"),
        "keys" | "labels" | "split" => Some("LIST<STRING>"),
        "nodes" => Some("LIST<NODE>"),
        "range" => Some("LIST<INTEGER>"),
        "relationships" => Some("LIST<RELATIONSHIP>"),
        "tobooleanlist" => Some("LIST<BOOLEAN>"),
        "tofloatlist" => Some("LIST<FLOAT>"),
        "tointegerlist" => Some("LIST<INTEGER>"),
        "tostringlist" => Some("LIST<STRING>"),
        "reverse" => Some("STRING | LIST<ANY>"),
        "coalesce" | "coll.max" | "coll.min" | "head" | "last" | "max" | "min" | "nullif"
        | "reduce" => Some("ANY"),
        _ => None,
    }
}

fn boolean_integer_return_type(name: &str) -> Option<&'static str> {
    if matches!(
        name,
        "all"
            | "allreduce"
            | "any"
            | "exists"
            | "isempty"
            | "none"
            | "point.withinbbox"
            | "property_exists"
            | "single"
            | "toboolean"
            | "tobooleanornull"
            | "isnan"
    ) {
        return Some("BOOLEAN");
    }
    if matches!(
        name,
        "cardinality"
            | "char_length"
            | "character_length"
            | "count"
            | "coll.indexof"
            | "id"
            | "length"
            | "path_length"
            | "sign"
            | "size"
            | "string.indexof"
            | "timestamp"
            | "tointeger"
            | "tointegerornull"
            | "uuid.leastsignificantbits"
            | "uuid.mostsignificantbits"
            | "vector_dimension_count"
    ) {
        return Some("INTEGER");
    }
    None
}

fn numeric_return_type(name: &str) -> Option<&'static str> {
    if matches!(
        name,
        "acos"
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
            | "ln"
            | "log"
            | "log10"
            | "percentile_cont"
            | "percentilecont"
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
    ) {
        return Some("FLOAT");
    }
    None
}

fn string_structural_return_type(name: &str) -> Option<&'static str> {
    if matches!(
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
    ) {
        return Some("STRING");
    }
    match name {
        "endnode" | "startnode" => Some("NODE"),
        "properties" => Some("MAP"),
        "point" => Some("POINT"),
        "uuid" => Some("UUID"),
        "vector" => Some("VECTOR"),
        _ => None,
    }
}

fn temporal_return_type(name: &str) -> Option<&'static str> {
    if matches!(
        name,
        "date" | "date.realtime" | "date.statement" | "date.transaction" | "date.truncate"
    ) {
        return Some("DATE");
    }
    if matches!(
        name,
        "localtime"
            | "local_time"
            | "localtime.realtime"
            | "localtime.statement"
            | "localtime.transaction"
            | "localtime.truncate"
    ) {
        return Some("LOCAL TIME");
    }
    if matches!(
        name,
        "time"
            | "time.realtime"
            | "time.statement"
            | "time.transaction"
            | "time.truncate"
            | "zoned_time"
    ) {
        return Some("ZONED TIME");
    }
    if matches!(
        name,
        "local_datetime"
            | "localdatetime"
            | "localdatetime.realtime"
            | "localdatetime.statement"
            | "localdatetime.transaction"
            | "localdatetime.truncate"
    ) {
        return Some("LOCAL DATETIME");
    }
    if matches!(
        name,
        "datetime"
            | "datetime.fromepoch"
            | "datetime.fromepochmillis"
            | "datetime.realtime"
            | "datetime.statement"
            | "datetime.transaction"
            | "datetime.truncate"
            | "zoned_datetime"
    ) {
        return Some("ZONED DATETIME");
    }
    if matches!(
        name,
        "duration"
            | "duration.between"
            | "duration.indays"
            | "duration.inmonths"
            | "duration.inseconds"
            | "duration_between"
    ) {
        return Some("DURATION");
    }
    None
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

    #[test]
    fn reverse_registry_preserves_both_frozen_overloads() {
        let definitions = functions()
            .filter(|definition| definition.name == "reverse")
            .map(|definition| (definition.category, definition.signature()))
            .collect::<Vec<_>>();
        assert_eq!(
            definitions,
            vec![
                (
                    "List",
                    Some("reverse(input :: LIST<ANY>) :: LIST<ANY>".to_owned())
                ),
                (
                    "String",
                    Some("reverse(input :: STRING) :: STRING".to_owned())
                ),
            ]
        );
    }
}
