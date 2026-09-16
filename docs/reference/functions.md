# Built-in Functions — v0.1.0

来自 v0.1.0 `SHOW FUNCTIONS YIELD *`：172 条签名，169 个名称（重载分别列出）。
范围固定为 `CY25-2026.08`。签名中的 `::` 是 introspection 类型说明，不是调用时要输入的字符。

在 Cypher 中通过 `RETURN functionName(...)` 调用；聚合函数在分组上下文工作。
签名不取消具体值的类型/维度/时区等校验；以 [Values](values.md) 和运行错误为准。
`id()` 的替代接口是 `elementId()`。`PROPERTY_EXISTS` 是语言 predicate，不作为函数出现在本清单。

```cypher
SHOW FUNCTIONS YIELD name, signature, category
RETURN name, signature, category ORDER BY name
```

| 签名 | 分类 | Deprecated / replacement |
| --- | --- | --- |
| `abs(input :: INTEGER \| FLOAT) :: INTEGER \| FLOAT` | Numeric | — |
| `acos(input :: FLOAT) :: FLOAT` | Trigonometric | — |
| `all(variable :: ANY, list :: LIST<ANY>, predicate :: ANY) :: BOOLEAN` | Predicate | — |
| `allReduce(accumulator = initial, stepVariable IN list \| reductionFunction, predicate) :: BOOLEAN` | Predicate | — |
| `any(variable :: ANY, list :: LIST<ANY>, predicate :: ANY) :: BOOLEAN` | Predicate | — |
| `asin(input :: FLOAT) :: FLOAT` | Trigonometric | — |
| `atan(input :: FLOAT) :: FLOAT` | Trigonometric | — |
| `atan2(y :: FLOAT, x :: FLOAT) :: FLOAT` | Trigonometric | — |
| `avg(input :: INTEGER \| FLOAT \| DURATION) :: INTEGER \| FLOAT \| DURATION` | Aggregating | — |
| `btrim(input :: STRING[, trimCharacterString :: STRING]) :: STRING` | String | — |
| `cardinality(input :: MAP \| LIST<ANY> \| PATH) :: INTEGER` | Scalar | — |
| `ceil(input :: FLOAT) :: FLOAT` | Numeric | — |
| `ceiling(input :: FLOAT) :: FLOAT` | Numeric | — |
| `char_length(input :: STRING) :: INTEGER` | Scalar | — |
| `character_length(input :: STRING) :: INTEGER` | Scalar | — |
| `coalesce(input :: ANY) :: ANY` | Scalar | — |
| `coll.distinct(list :: LIST<ANY>) :: LIST<ANY>` | List | — |
| `coll.flatten(list :: LIST<ANY>, depth = 1 :: INTEGER) :: LIST<ANY>` | List | — |
| `coll.indexOf(list :: LIST<ANY>, value :: ANY) :: INTEGER` | List | — |
| `coll.insert(list :: LIST<ANY>, index :: INTEGER, value :: ANY) :: LIST<ANY>` | List | — |
| `coll.max(list :: LIST<ANY>) :: ANY` | List | — |
| `coll.min(list :: LIST<ANY>) :: ANY` | List | — |
| `coll.remove(list :: LIST<ANY>, index :: INTEGER) :: LIST<ANY>` | List | — |
| `coll.sort(list :: LIST<ANY>) :: LIST<ANY>` | List | — |
| `collect(input :: ANY) :: LIST<ANY>` | Aggregating | — |
| `collect_list(input :: ANY) :: LIST<ANY>` | Aggregating | — |
| `cos(input :: FLOAT) :: FLOAT` | Trigonometric | — |
| `cosh(input :: FLOAT) :: FLOAT` | Trigonometric | — |
| `cot(input :: FLOAT) :: FLOAT` | Trigonometric | — |
| `coth(input :: FLOAT) :: FLOAT` | Trigonometric | — |
| `count(input :: ANY) :: INTEGER` | Aggregating | — |
| `date(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: DATE` | Temporal | — |
| `date.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: DATE` | Temporal | — |
| `date.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: DATE` | Temporal | — |
| `date.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: DATE` | Temporal | — |
| `date.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: DATE` | Temporal | — |
| `datetime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: ZONED DATETIME` | Temporal | — |
| `datetime.fromEpoch(seconds :: INTEGER \| FLOAT, nanoseconds :: INTEGER \| FLOAT) :: ZONED DATETIME` | Temporal | — |
| `datetime.fromEpochMillis(milliseconds :: INTEGER \| FLOAT) :: ZONED DATETIME` | Temporal | — |
| `datetime.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED DATETIME` | Temporal | — |
| `datetime.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED DATETIME` | Temporal | — |
| `datetime.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED DATETIME` | Temporal | — |
| `datetime.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: ZONED DATETIME` | Temporal | — |
| `db.nameFromElementId(elementId :: STRING) :: STRING` | Database | — |
| `degrees(input :: FLOAT) :: FLOAT` | Trigonometric | — |
| `duration(input :: ANY[, pattern :: STRING]) :: DURATION` | Temporal | — |
| `duration.between(from :: ANY, to :: ANY) :: DURATION` | Temporal | — |
| `duration.inDays(from :: ANY, to :: ANY) :: DURATION` | Temporal | — |
| `duration.inMonths(from :: ANY, to :: ANY) :: DURATION` | Temporal | — |
| `duration.inSeconds(from :: ANY, to :: ANY) :: DURATION` | Temporal | — |
| `duration_between(from :: ANY, to :: ANY) :: DURATION` | Temporal | — |
| `e() :: FLOAT` | Logarithmic | — |
| `elementId(input :: NODE \| RELATIONSHIP) :: STRING` | Scalar | — |
| `endNode(input :: RELATIONSHIP) :: NODE` | Scalar | — |
| `exists(input :: ANY) :: BOOLEAN` | Predicate | — |
| `exp(input :: FLOAT) :: FLOAT` | Logarithmic | — |
| `file() :: STRING` | Scalar | — |
| `floor(input :: FLOAT) :: FLOAT` | Numeric | — |
| `format(value :: DATE \| LOCAL TIME \| ZONED TIME \| LOCAL DATETIME \| ZONED DATETIME \| DURATION[, pattern :: STRING]) :: STRING` | Temporal | — |
| `haversin(input :: FLOAT) :: FLOAT` | Trigonometric | — |
| `head(list :: LIST<ANY>) :: ANY` | Scalar | — |
| `id(input :: NODE \| RELATIONSHIP) :: INTEGER` | Scalar | elementId |
| `isEmpty(input :: LIST<ANY> \| MAP \| STRING) :: BOOLEAN` | Predicate | — |
| `isNaN(input :: INTEGER \| FLOAT) :: BOOLEAN` | Numeric | — |
| `keys(input :: NODE \| RELATIONSHIP \| MAP) :: LIST<STRING>` | List | — |
| `labels(input :: NODE) :: LIST<STRING>` | List | — |
| `last(list :: LIST<ANY>) :: ANY` | Scalar | — |
| `left(original :: STRING, length :: INTEGER) :: STRING` | String | — |
| `length(input :: PATH) :: INTEGER` | Scalar | — |
| `linenumber() :: INTEGER` | Scalar | — |
| `ln(input :: FLOAT) :: FLOAT` | Logarithmic | — |
| `local_datetime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: LOCAL DATETIME` | Temporal | — |
| `local_time(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: LOCAL TIME` | Temporal | — |
| `localdatetime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: LOCAL DATETIME` | Temporal | — |
| `localdatetime.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL DATETIME` | Temporal | — |
| `localdatetime.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL DATETIME` | Temporal | — |
| `localdatetime.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL DATETIME` | Temporal | — |
| `localdatetime.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: LOCAL DATETIME` | Temporal | — |
| `localtime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: LOCAL TIME` | Temporal | — |
| `localtime.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL TIME` | Temporal | — |
| `localtime.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL TIME` | Temporal | — |
| `localtime.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL TIME` | Temporal | — |
| `localtime.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: LOCAL TIME` | Temporal | — |
| `log(input :: FLOAT) :: FLOAT` | Logarithmic | — |
| `log10(input :: FLOAT) :: FLOAT` | Logarithmic | — |
| `lower(input :: STRING) :: STRING` | String | — |
| `ltrim(input :: STRING[, trimCharacterString :: STRING]) :: STRING` | String | — |
| `max(input :: ANY) :: ANY` | Aggregating | — |
| `min(input :: ANY) :: ANY` | Aggregating | — |
| `nodes(input :: PATH) :: LIST<NODE>` | List | — |
| `none(variable :: ANY, list :: LIST<ANY>, predicate :: ANY) :: BOOLEAN` | Predicate | — |
| `normalize(input :: STRING [, normalForm = NFC :: [NFC, NFD, NFKC, NFKD]]) :: STRING` | String | — |
| `nullIf(v1 :: ANY, v2 :: ANY) :: ANY` | Scalar | — |
| `path_length(input :: PATH) :: INTEGER` | Scalar | — |
| `percentile_cont(input :: FLOAT, percentile :: FLOAT) :: FLOAT` | Aggregating | — |
| `percentile_disc(input :: INTEGER \| FLOAT, percentile :: FLOAT) :: INTEGER \| FLOAT` | Aggregating | — |
| `percentileCont(input :: FLOAT, percentile :: FLOAT) :: FLOAT` | Aggregating | — |
| `percentileDisc(input :: INTEGER \| FLOAT, percentile :: FLOAT) :: INTEGER \| FLOAT` | Aggregating | — |
| `pi() :: FLOAT` | Trigonometric | — |
| `point(input :: MAP) :: POINT` | Spatial | — |
| `point.distance(from :: POINT, to :: POINT) :: FLOAT` | Spatial | — |
| `point.withinBBox(point :: POINT, lowerLeft :: POINT, upperRight :: POINT) :: BOOLEAN` | Spatial | — |
| `properties(input :: NODE \| RELATIONSHIP \| MAP) :: MAP` | Scalar | — |
| `radians(input :: FLOAT) :: FLOAT` | Trigonometric | — |
| `rand() :: FLOAT` | Numeric | — |
| `randomUUID() :: STRING` | Scalar | — |
| `range(start :: INTEGER, end :: INTEGER[, step :: INTEGER]) :: LIST<INTEGER>` | List | — |
| `reduce(accumulator :: VARIABLE = initial :: ANY, variable :: VARIABLE IN list :: LIST<ANY> expression :: ANY) :: ANY` | List | — |
| `relationships(input :: PATH) :: LIST<RELATIONSHIP>` | List | — |
| `replace(original :: STRING, search :: STRING, replace :: STRING[, limit :: INTEGER]) :: STRING` | String | — |
| `reverse(input :: LIST<ANY>) :: LIST<ANY>` | List | — |
| `reverse(input :: STRING) :: STRING` | String | — |
| `right(original :: STRING, length :: INTEGER) :: STRING` | String | — |
| `round(input :: FLOAT [, precision :: INTEGER \| FLOAT, mode :: STRING]) :: FLOAT` | Numeric | — |
| `rtrim(input :: STRING[, trimCharacterString :: STRING]) :: STRING` | String | — |
| `sign(input :: INTEGER \| FLOAT) :: INTEGER` | Numeric | — |
| `sin(input :: FLOAT) :: FLOAT` | Trigonometric | — |
| `single(variable :: ANY, list :: LIST<ANY>, predicate :: ANY) :: BOOLEAN` | Predicate | — |
| `sinh(input :: FLOAT) :: FLOAT` | Trigonometric | — |
| `size(input :: STRING \| LIST<ANY> \| VECTOR) :: INTEGER` | Scalar | — |
| `split(original :: STRING, splitDelimiters :: STRING \| LIST<STRING>) :: LIST<STRING>` | String | — |
| `sqrt(input :: FLOAT) :: FLOAT` | Logarithmic | — |
| `startNode(input :: RELATIONSHIP) :: NODE` | Scalar | — |
| `stDev(input :: FLOAT) :: FLOAT` | Aggregating | — |
| `stdev_pop(input :: FLOAT) :: FLOAT` | Aggregating | — |
| `stdev_samp(input :: FLOAT) :: FLOAT` | Aggregating | — |
| `stDevP(input :: FLOAT) :: FLOAT` | Aggregating | — |
| `string.indexOf(input :: STRING, value :: STRING) :: INTEGER` | String | — |
| `string.join(input :: LIST<STRING>, delimiter :: STRING) :: STRING` | String | — |
| `string.regexReplace(original :: STRING, regex :: STRING, replacement :: STRING) :: STRING` | String | — |
| `substring(original :: STRING, start :: INTEGER[, length :: INTEGER]) :: STRING` | String | — |
| `sum(input :: INTEGER \| FLOAT \| DURATION) :: INTEGER \| FLOAT \| DURATION` | Aggregating | — |
| `tail(input :: LIST<ANY>) :: LIST<ANY>` | List | — |
| `tan(input :: FLOAT) :: FLOAT` | Trigonometric | — |
| `tanh(input :: FLOAT) :: FLOAT` | Trigonometric | — |
| `time(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: ZONED TIME` | Temporal | — |
| `time.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED TIME` | Temporal | — |
| `time.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED TIME` | Temporal | — |
| `time.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED TIME` | Temporal | — |
| `time.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: ZONED TIME` | Temporal | — |
| `timestamp() :: INTEGER` | Temporal | — |
| `toBoolean(input :: BOOLEAN \| STRING \| INTEGER) :: BOOLEAN` | Scalar | — |
| `toBooleanList(input :: LIST<ANY>) :: LIST<BOOLEAN>` | List | — |
| `toBooleanOrNull(input :: ANY) :: BOOLEAN` | Scalar | — |
| `toFloat(input :: STRING \| INTEGER \| FLOAT) :: FLOAT` | Scalar | — |
| `toFloatList(input :: VECTOR \| LIST<ANY>) :: LIST<FLOAT>` | List | — |
| `toFloatOrNull(input :: ANY) :: FLOAT` | Scalar | — |
| `toInteger(input :: BOOLEAN \| STRING \| INTEGER \| FLOAT) :: INTEGER` | Scalar | — |
| `toIntegerList(input :: VECTOR \| LIST<ANY>) :: LIST<INTEGER>` | List | — |
| `toIntegerOrNull(input :: ANY) :: INTEGER` | Scalar | — |
| `toLower(input :: STRING) :: STRING` | String | — |
| `toString(input :: ANY) :: STRING` | String | — |
| `toStringList(input :: LIST<ANY>) :: LIST<STRING>` | List | — |
| `toStringOrNull(input :: ANY) :: STRING` | String | — |
| `toUpper(input :: STRING) :: STRING` | String | — |
| `trim([[LEADING \| TRAILING \| BOTH] [trimCharacterString :: STRING] FROM] input :: STRING) :: STRING` | String | — |
| `type(input :: RELATIONSHIP) :: STRING` | Scalar | — |
| `upper(input :: STRING) :: STRING` | String | — |
| `uuid() :: UUID` | Scalar | — |
| `uuid(name :: STRING) :: UUID` | Scalar | — |
| `uuid(mostSigBits :: INTEGER, leastSigBits :: INTEGER) :: UUID` | Scalar | — |
| `uuid.leastSignificantBits(uuid :: UUID) :: INTEGER` | Scalar | — |
| `uuid.mostSignificantBits(uuid :: UUID) :: INTEGER` | Scalar | — |
| `valueType(input :: ANY) :: STRING` | Scalar | — |
| `vector(vectorValue :: STRING \| LIST<INTEGER \| FLOAT>, dimension :: INTEGER, coordinateType :: [INTEGER64, INTEGER32, INTEGER16, INTEGER8, FLOAT64, FLOAT32]) :: VECTOR` | Scalar | — |
| `vector.similarity.cosine(a :: VECTOR \| LIST<INTEGER \| FLOAT>, b :: VECTOR \| LIST<INTEGER \| FLOAT>) :: FLOAT` | Vector | — |
| `vector.similarity.euclidean(a :: VECTOR \| LIST<INTEGER \| FLOAT>, b :: VECTOR \| LIST<INTEGER \| FLOAT>) :: FLOAT` | Vector | — |
| `vector_dimension_count(vector :: VECTOR) :: INTEGER` | Vector | — |
| `vector_distance(vector1 :: VECTOR, vector2 :: VECTOR, vectorDistanceMetric :: [EUCLIDEAN, EUCLIDEAN_SQUARED, MANHATTAN, COSINE, DOT, HAMMING]) :: FLOAT` | Vector | — |
| `vector_norm(vector :: VECTOR, vectorDistanceMetric :: [EUCLIDEAN, MANHATTAN]) :: FLOAT` | Vector | — |
| `zoned_datetime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: ZONED DATETIME` | Temporal | — |
| `zoned_time(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: ZONED TIME` | Temporal | — |

依据：[v0.1.0 函数注册表](../../crates/lithograph-core/src/query/registry.rs)、发布制品 introspection。
更新方法见 [验证说明](../guide/examples/README.md)。不要按新版外部数据库清单直接扩充此版本。
