# 内置函数索引

适用版本：Lithograph v0.3.2 · [产品手册](../guide/README.md) · [Cypher 手册](../cypher/index.md)

本表由目标扩展的 `SHOW FUNCTIONS YIELD *` 生成：169 个名称，172 条签名。重载分别列出；分类保持运行时登记值。
Cypher Profile 为 `CY25-2026.08`。`::`、可选方括号和默认值是签名说明，实际调用形式、参数语义、null/type/error 和示例见每行的语言参考。

签名描述运行时登记信息，不能代替具体类型和维度检查；宿主参数与返回值编码见 [Values](values.md)。`id()` 的替代接口是 `elementId()`。

<!-- generated: functions -->
| 签名 | 分类 | Deprecated / replacement | 使用说明 |
| --- | --- | --- | --- |
| `abs(input :: INTEGER \| FLOAT) :: INTEGER \| FLOAT` | Numeric | — | [abs](../cypher/functions/mathematical.md#fn-abs) |
| `acos(input :: FLOAT) :: FLOAT` | Trigonometric | — | [acos](../cypher/functions/mathematical.md#fn-acos) |
| `all(variable :: ANY, list :: LIST<ANY>, predicate :: ANY) :: BOOLEAN` | Predicate | — | [all](../cypher/functions/predicate.md#fn-all) |
| `allReduce(accumulator = initial, stepVariable IN list \| reductionFunction, predicate) :: BOOLEAN` | Predicate | — | [allReduce](../cypher/functions/predicate.md#fn-allreduce) |
| `any(variable :: ANY, list :: LIST<ANY>, predicate :: ANY) :: BOOLEAN` | Predicate | — | [any](../cypher/functions/predicate.md#fn-any) |
| `asin(input :: FLOAT) :: FLOAT` | Trigonometric | — | [asin](../cypher/functions/mathematical.md#fn-asin) |
| `atan(input :: FLOAT) :: FLOAT` | Trigonometric | — | [atan](../cypher/functions/mathematical.md#fn-atan) |
| `atan2(y :: FLOAT, x :: FLOAT) :: FLOAT` | Trigonometric | — | [atan2](../cypher/functions/mathematical.md#fn-atan2) |
| `avg(input :: INTEGER \| FLOAT \| DURATION) :: INTEGER \| FLOAT \| DURATION` | Aggregating | — | [avg](../cypher/functions/aggregation.md#fn-avg) |
| `btrim(input :: STRING[, trimCharacterString :: STRING]) :: STRING` | String | — | [btrim](../cypher/functions/string.md#fn-btrim) |
| `cardinality(input :: MAP \| LIST<ANY> \| PATH) :: INTEGER` | Scalar | — | [cardinality](../cypher/functions/scalar.md#fn-cardinality) |
| `ceil(input :: FLOAT) :: FLOAT` | Numeric | — | [ceil](../cypher/functions/mathematical.md#fn-ceil) |
| `ceiling(input :: FLOAT) :: FLOAT` | Numeric | — | [ceiling](../cypher/functions/mathematical.md#fn-ceiling) |
| `char_length(input :: STRING) :: INTEGER` | Scalar | — | [char_length](../cypher/functions/scalar.md#fn-char_length) |
| `character_length(input :: STRING) :: INTEGER` | Scalar | — | [character_length](../cypher/functions/scalar.md#fn-character_length) |
| `coalesce(input :: ANY) :: ANY` | Scalar | — | [coalesce](../cypher/functions/scalar.md#fn-coalesce) |
| `coll.distinct(list :: LIST<ANY>) :: LIST<ANY>` | List | — | [coll.distinct](../cypher/functions/list.md#fn-coll-distinct) |
| `coll.flatten(list :: LIST<ANY>, depth = 1 :: INTEGER) :: LIST<ANY>` | List | — | [coll.flatten](../cypher/functions/list.md#fn-coll-flatten) |
| `coll.indexOf(list :: LIST<ANY>, value :: ANY) :: INTEGER` | List | — | [coll.indexOf](../cypher/functions/list.md#fn-coll-indexof) |
| `coll.insert(list :: LIST<ANY>, index :: INTEGER, value :: ANY) :: LIST<ANY>` | List | — | [coll.insert](../cypher/functions/list.md#fn-coll-insert) |
| `coll.max(list :: LIST<ANY>) :: ANY` | List | — | [coll.max](../cypher/functions/list.md#fn-coll-max) |
| `coll.min(list :: LIST<ANY>) :: ANY` | List | — | [coll.min](../cypher/functions/list.md#fn-coll-min) |
| `coll.remove(list :: LIST<ANY>, index :: INTEGER) :: LIST<ANY>` | List | — | [coll.remove](../cypher/functions/list.md#fn-coll-remove) |
| `coll.sort(list :: LIST<ANY>) :: LIST<ANY>` | List | — | [coll.sort](../cypher/functions/list.md#fn-coll-sort) |
| `collect(input :: ANY) :: LIST<ANY>` | Aggregating | — | [collect](../cypher/functions/aggregation.md#fn-collect) |
| `collect_list(input :: ANY) :: LIST<ANY>` | Aggregating | — | [collect_list](../cypher/functions/aggregation.md#fn-collect_list) |
| `cos(input :: FLOAT) :: FLOAT` | Trigonometric | — | [cos](../cypher/functions/mathematical.md#fn-cos) |
| `cosh(input :: FLOAT) :: FLOAT` | Trigonometric | — | [cosh](../cypher/functions/mathematical.md#fn-cosh) |
| `cot(input :: FLOAT) :: FLOAT` | Trigonometric | — | [cot](../cypher/functions/mathematical.md#fn-cot) |
| `coth(input :: FLOAT) :: FLOAT` | Trigonometric | — | [coth](../cypher/functions/mathematical.md#fn-coth) |
| `count(input :: ANY) :: INTEGER` | Aggregating | — | [count](../cypher/functions/aggregation.md#fn-count) |
| `date(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: DATE` | Temporal | — | [date](../cypher/functions/temporal.md#fn-date) |
| `date.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: DATE` | Temporal | — | [date.realtime](../cypher/functions/temporal.md#fn-date-realtime) |
| `date.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: DATE` | Temporal | — | [date.statement](../cypher/functions/temporal.md#fn-date-statement) |
| `date.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: DATE` | Temporal | — | [date.transaction](../cypher/functions/temporal.md#fn-date-transaction) |
| `date.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: DATE` | Temporal | — | [date.truncate](../cypher/functions/temporal.md#fn-date-truncate) |
| `datetime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: ZONED DATETIME` | Temporal | — | [datetime](../cypher/functions/temporal.md#fn-datetime) |
| `datetime.fromEpoch(seconds :: INTEGER \| FLOAT, nanoseconds :: INTEGER \| FLOAT) :: ZONED DATETIME` | Temporal | — | [datetime.fromEpoch](../cypher/functions/temporal.md#fn-datetime-fromepoch) |
| `datetime.fromEpochMillis(milliseconds :: INTEGER \| FLOAT) :: ZONED DATETIME` | Temporal | — | [datetime.fromEpochMillis](../cypher/functions/temporal.md#fn-datetime-fromepochmillis) |
| `datetime.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED DATETIME` | Temporal | — | [datetime.realtime](../cypher/functions/temporal.md#fn-datetime-realtime) |
| `datetime.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED DATETIME` | Temporal | — | [datetime.statement](../cypher/functions/temporal.md#fn-datetime-statement) |
| `datetime.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED DATETIME` | Temporal | — | [datetime.transaction](../cypher/functions/temporal.md#fn-datetime-transaction) |
| `datetime.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: ZONED DATETIME` | Temporal | — | [datetime.truncate](../cypher/functions/temporal.md#fn-datetime-truncate) |
| `db.nameFromElementId(elementId :: STRING) :: STRING` | Database | — | [db.nameFromElementId](../cypher/functions/scalar.md#fn-db-namefromelementid) |
| `degrees(input :: FLOAT) :: FLOAT` | Trigonometric | — | [degrees](../cypher/functions/mathematical.md#fn-degrees) |
| `duration(input :: ANY[, pattern :: STRING]) :: DURATION` | Temporal | — | [duration](../cypher/functions/temporal.md#fn-duration) |
| `duration.between(from :: ANY, to :: ANY) :: DURATION` | Temporal | — | [duration.between](../cypher/functions/temporal.md#fn-duration-between) |
| `duration.inDays(from :: ANY, to :: ANY) :: DURATION` | Temporal | — | [duration.inDays](../cypher/functions/temporal.md#fn-duration-indays) |
| `duration.inMonths(from :: ANY, to :: ANY) :: DURATION` | Temporal | — | [duration.inMonths](../cypher/functions/temporal.md#fn-duration-inmonths) |
| `duration.inSeconds(from :: ANY, to :: ANY) :: DURATION` | Temporal | — | [duration.inSeconds](../cypher/functions/temporal.md#fn-duration-inseconds) |
| `duration_between(from :: ANY, to :: ANY) :: DURATION` | Temporal | — | [duration_between](../cypher/functions/temporal.md#fn-duration_between) |
| `e() :: FLOAT` | Logarithmic | — | [e](../cypher/functions/mathematical.md#fn-e) |
| `elementId(input :: NODE \| RELATIONSHIP) :: STRING` | Scalar | — | [elementId](../cypher/functions/scalar.md#fn-elementid) |
| `endNode(input :: RELATIONSHIP) :: NODE` | Scalar | — | [endNode](../cypher/functions/scalar.md#fn-endnode) |
| `exists(input :: ANY) :: BOOLEAN` | Predicate | — | [exists](../cypher/functions/predicate.md#fn-exists) |
| `exp(input :: FLOAT) :: FLOAT` | Logarithmic | — | [exp](../cypher/functions/mathematical.md#fn-exp) |
| `file() :: STRING` | Scalar | — | [file](../cypher/functions/scalar.md#fn-file) |
| `floor(input :: FLOAT) :: FLOAT` | Numeric | — | [floor](../cypher/functions/mathematical.md#fn-floor) |
| `format(value :: DATE \| LOCAL TIME \| ZONED TIME \| LOCAL DATETIME \| ZONED DATETIME \| DURATION[, pattern :: STRING]) :: STRING` | Temporal | — | [format](../cypher/functions/temporal.md#fn-format) |
| `haversin(input :: FLOAT) :: FLOAT` | Trigonometric | — | [haversin](../cypher/functions/mathematical.md#fn-haversin) |
| `head(list :: LIST<ANY>) :: ANY` | Scalar | — | [head](../cypher/functions/scalar.md#fn-head) |
| `id(input :: NODE \| RELATIONSHIP) :: INTEGER` | Scalar | elementId | [id](../cypher/functions/scalar.md#fn-id) |
| `isEmpty(input :: LIST<ANY> \| MAP \| STRING) :: BOOLEAN` | Predicate | — | [isEmpty](../cypher/functions/predicate.md#fn-isempty) |
| `isNaN(input :: INTEGER \| FLOAT) :: BOOLEAN` | Numeric | — | [isNaN](../cypher/functions/mathematical.md#fn-isnan) |
| `keys(input :: NODE \| RELATIONSHIP \| MAP) :: LIST<STRING>` | List | — | [keys](../cypher/functions/list.md#fn-keys) |
| `labels(input :: NODE) :: LIST<STRING>` | List | — | [labels](../cypher/functions/list.md#fn-labels) |
| `last(list :: LIST<ANY>) :: ANY` | Scalar | — | [last](../cypher/functions/scalar.md#fn-last) |
| `left(original :: STRING, length :: INTEGER) :: STRING` | String | — | [left](../cypher/functions/string.md#fn-left) |
| `length(input :: PATH) :: INTEGER` | Scalar | — | [length](../cypher/functions/scalar.md#fn-length) |
| `linenumber() :: INTEGER` | Scalar | — | [linenumber](../cypher/functions/scalar.md#fn-linenumber) |
| `ln(input :: FLOAT) :: FLOAT` | Logarithmic | — | [ln](../cypher/functions/mathematical.md#fn-ln) |
| `local_datetime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: LOCAL DATETIME` | Temporal | — | [local_datetime](../cypher/functions/temporal.md#fn-local_datetime) |
| `local_time(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: LOCAL TIME` | Temporal | — | [local_time](../cypher/functions/temporal.md#fn-local_time) |
| `localdatetime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: LOCAL DATETIME` | Temporal | — | [localdatetime](../cypher/functions/temporal.md#fn-localdatetime) |
| `localdatetime.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL DATETIME` | Temporal | — | [localdatetime.realtime](../cypher/functions/temporal.md#fn-localdatetime-realtime) |
| `localdatetime.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL DATETIME` | Temporal | — | [localdatetime.statement](../cypher/functions/temporal.md#fn-localdatetime-statement) |
| `localdatetime.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL DATETIME` | Temporal | — | [localdatetime.transaction](../cypher/functions/temporal.md#fn-localdatetime-transaction) |
| `localdatetime.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: LOCAL DATETIME` | Temporal | — | [localdatetime.truncate](../cypher/functions/temporal.md#fn-localdatetime-truncate) |
| `localtime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: LOCAL TIME` | Temporal | — | [localtime](../cypher/functions/temporal.md#fn-localtime) |
| `localtime.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL TIME` | Temporal | — | [localtime.realtime](../cypher/functions/temporal.md#fn-localtime-realtime) |
| `localtime.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL TIME` | Temporal | — | [localtime.statement](../cypher/functions/temporal.md#fn-localtime-statement) |
| `localtime.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL TIME` | Temporal | — | [localtime.transaction](../cypher/functions/temporal.md#fn-localtime-transaction) |
| `localtime.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: LOCAL TIME` | Temporal | — | [localtime.truncate](../cypher/functions/temporal.md#fn-localtime-truncate) |
| `log(input :: FLOAT) :: FLOAT` | Logarithmic | — | [log](../cypher/functions/mathematical.md#fn-log) |
| `log10(input :: FLOAT) :: FLOAT` | Logarithmic | — | [log10](../cypher/functions/mathematical.md#fn-log10) |
| `lower(input :: STRING) :: STRING` | String | — | [lower](../cypher/functions/string.md#fn-lower) |
| `ltrim(input :: STRING[, trimCharacterString :: STRING]) :: STRING` | String | — | [ltrim](../cypher/functions/string.md#fn-ltrim) |
| `max(input :: ANY) :: ANY` | Aggregating | — | [max](../cypher/functions/aggregation.md#fn-max) |
| `min(input :: ANY) :: ANY` | Aggregating | — | [min](../cypher/functions/aggregation.md#fn-min) |
| `nodes(input :: PATH) :: LIST<NODE>` | List | — | [nodes](../cypher/functions/list.md#fn-nodes) |
| `none(variable :: ANY, list :: LIST<ANY>, predicate :: ANY) :: BOOLEAN` | Predicate | — | [none](../cypher/functions/predicate.md#fn-none) |
| `normalize(input :: STRING [, normalForm = NFC :: [NFC, NFD, NFKC, NFKD]]) :: STRING` | String | — | [normalize](../cypher/functions/string.md#fn-normalize) |
| `nullIf(v1 :: ANY, v2 :: ANY) :: ANY` | Scalar | — | [nullIf](../cypher/functions/scalar.md#fn-nullif) |
| `path_length(input :: PATH) :: INTEGER` | Scalar | — | [path_length](../cypher/functions/scalar.md#fn-path_length) |
| `percentile_cont(input :: FLOAT, percentile :: FLOAT) :: FLOAT` | Aggregating | — | [percentile_cont](../cypher/functions/aggregation.md#fn-percentile_cont) |
| `percentile_disc(input :: INTEGER \| FLOAT, percentile :: FLOAT) :: INTEGER \| FLOAT` | Aggregating | — | [percentile_disc](../cypher/functions/aggregation.md#fn-percentile_disc) |
| `percentileCont(input :: FLOAT, percentile :: FLOAT) :: FLOAT` | Aggregating | — | [percentileCont](../cypher/functions/aggregation.md#fn-percentilecont) |
| `percentileDisc(input :: INTEGER \| FLOAT, percentile :: FLOAT) :: INTEGER \| FLOAT` | Aggregating | — | [percentileDisc](../cypher/functions/aggregation.md#fn-percentiledisc) |
| `pi() :: FLOAT` | Trigonometric | — | [pi](../cypher/functions/mathematical.md#fn-pi) |
| `point(input :: MAP) :: POINT` | Spatial | — | [point](../cypher/functions/spatial.md#fn-point) |
| `point.distance(from :: POINT, to :: POINT) :: FLOAT` | Spatial | — | [point.distance](../cypher/functions/spatial.md#fn-point-distance) |
| `point.withinBBox(point :: POINT, lowerLeft :: POINT, upperRight :: POINT) :: BOOLEAN` | Spatial | — | [point.withinBBox](../cypher/functions/spatial.md#fn-point-withinbbox) |
| `properties(input :: NODE \| RELATIONSHIP \| MAP) :: MAP` | Scalar | — | [properties](../cypher/functions/scalar.md#fn-properties) |
| `radians(input :: FLOAT) :: FLOAT` | Trigonometric | — | [radians](../cypher/functions/mathematical.md#fn-radians) |
| `rand() :: FLOAT` | Numeric | — | [rand](../cypher/functions/mathematical.md#fn-rand) |
| `randomUUID() :: STRING` | Scalar | — | [randomUUID](../cypher/functions/scalar.md#fn-randomuuid) |
| `range(start :: INTEGER, end :: INTEGER[, step :: INTEGER]) :: LIST<INTEGER>` | List | — | [range](../cypher/functions/list.md#fn-range) |
| `reduce(accumulator :: VARIABLE = initial :: ANY, variable :: VARIABLE IN list :: LIST<ANY> expression :: ANY) :: ANY` | List | — | [reduce](../cypher/functions/list.md#fn-reduce) |
| `relationships(input :: PATH) :: LIST<RELATIONSHIP>` | List | — | [relationships](../cypher/functions/list.md#fn-relationships) |
| `replace(original :: STRING, search :: STRING, replace :: STRING[, limit :: INTEGER]) :: STRING` | String | — | [replace](../cypher/functions/string.md#fn-replace) |
| `reverse(input :: LIST<ANY>) :: LIST<ANY>` | List | — | [reverse](../cypher/functions/list.md#fn-reverse) |
| `reverse(input :: STRING) :: STRING` | String | — | [reverse](../cypher/functions/string.md#fn-reverse) |
| `right(original :: STRING, length :: INTEGER) :: STRING` | String | — | [right](../cypher/functions/string.md#fn-right) |
| `round(input :: FLOAT [, precision :: INTEGER \| FLOAT, mode :: STRING]) :: FLOAT` | Numeric | — | [round](../cypher/functions/mathematical.md#fn-round) |
| `rtrim(input :: STRING[, trimCharacterString :: STRING]) :: STRING` | String | — | [rtrim](../cypher/functions/string.md#fn-rtrim) |
| `sign(input :: INTEGER \| FLOAT) :: INTEGER` | Numeric | — | [sign](../cypher/functions/mathematical.md#fn-sign) |
| `sin(input :: FLOAT) :: FLOAT` | Trigonometric | — | [sin](../cypher/functions/mathematical.md#fn-sin) |
| `single(variable :: ANY, list :: LIST<ANY>, predicate :: ANY) :: BOOLEAN` | Predicate | — | [single](../cypher/functions/predicate.md#fn-single) |
| `sinh(input :: FLOAT) :: FLOAT` | Trigonometric | — | [sinh](../cypher/functions/mathematical.md#fn-sinh) |
| `size(input :: STRING \| LIST<ANY> \| VECTOR) :: INTEGER` | Scalar | — | [size](../cypher/functions/scalar.md#fn-size) |
| `split(original :: STRING, splitDelimiters :: STRING \| LIST<STRING>) :: LIST<STRING>` | String | — | [split](../cypher/functions/string.md#fn-split) |
| `sqrt(input :: FLOAT) :: FLOAT` | Logarithmic | — | [sqrt](../cypher/functions/mathematical.md#fn-sqrt) |
| `startNode(input :: RELATIONSHIP) :: NODE` | Scalar | — | [startNode](../cypher/functions/scalar.md#fn-startnode) |
| `stDev(input :: FLOAT) :: FLOAT` | Aggregating | — | [stDev](../cypher/functions/aggregation.md#fn-stdev) |
| `stdev_pop(input :: FLOAT) :: FLOAT` | Aggregating | — | [stdev_pop](../cypher/functions/aggregation.md#fn-stdev_pop) |
| `stdev_samp(input :: FLOAT) :: FLOAT` | Aggregating | — | [stdev_samp](../cypher/functions/aggregation.md#fn-stdev_samp) |
| `stDevP(input :: FLOAT) :: FLOAT` | Aggregating | — | [stDevP](../cypher/functions/aggregation.md#fn-stdevp) |
| `string.indexOf(input :: STRING, value :: STRING) :: INTEGER` | String | — | [string.indexOf](../cypher/functions/string.md#fn-string-indexof) |
| `string.join(input :: LIST<STRING>, delimiter :: STRING) :: STRING` | String | — | [string.join](../cypher/functions/string.md#fn-string-join) |
| `string.regexReplace(original :: STRING, regex :: STRING, replacement :: STRING) :: STRING` | String | — | [string.regexReplace](../cypher/functions/string.md#fn-string-regexreplace) |
| `substring(original :: STRING, start :: INTEGER[, length :: INTEGER]) :: STRING` | String | — | [substring](../cypher/functions/string.md#fn-substring) |
| `sum(input :: INTEGER \| FLOAT \| DURATION) :: INTEGER \| FLOAT \| DURATION` | Aggregating | — | [sum](../cypher/functions/aggregation.md#fn-sum) |
| `tail(input :: LIST<ANY>) :: LIST<ANY>` | List | — | [tail](../cypher/functions/list.md#fn-tail) |
| `tan(input :: FLOAT) :: FLOAT` | Trigonometric | — | [tan](../cypher/functions/mathematical.md#fn-tan) |
| `tanh(input :: FLOAT) :: FLOAT` | Trigonometric | — | [tanh](../cypher/functions/mathematical.md#fn-tanh) |
| `time(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: ZONED TIME` | Temporal | — | [time](../cypher/functions/temporal.md#fn-time) |
| `time.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED TIME` | Temporal | — | [time.realtime](../cypher/functions/temporal.md#fn-time-realtime) |
| `time.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED TIME` | Temporal | — | [time.statement](../cypher/functions/temporal.md#fn-time-statement) |
| `time.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED TIME` | Temporal | — | [time.transaction](../cypher/functions/temporal.md#fn-time-transaction) |
| `time.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: ZONED TIME` | Temporal | — | [time.truncate](../cypher/functions/temporal.md#fn-time-truncate) |
| `timestamp() :: INTEGER` | Temporal | — | [timestamp](../cypher/functions/temporal.md#fn-timestamp) |
| `toBoolean(input :: BOOLEAN \| STRING \| INTEGER) :: BOOLEAN` | Scalar | — | [toBoolean](../cypher/functions/scalar.md#fn-toboolean) |
| `toBooleanList(input :: LIST<ANY>) :: LIST<BOOLEAN>` | List | — | [toBooleanList](../cypher/functions/list.md#fn-tobooleanlist) |
| `toBooleanOrNull(input :: ANY) :: BOOLEAN` | Scalar | — | [toBooleanOrNull](../cypher/functions/scalar.md#fn-tobooleanornull) |
| `toFloat(input :: STRING \| INTEGER \| FLOAT) :: FLOAT` | Scalar | — | [toFloat](../cypher/functions/scalar.md#fn-tofloat) |
| `toFloatList(input :: VECTOR \| LIST<ANY>) :: LIST<FLOAT>` | List | — | [toFloatList](../cypher/functions/list.md#fn-tofloatlist) |
| `toFloatOrNull(input :: ANY) :: FLOAT` | Scalar | — | [toFloatOrNull](../cypher/functions/scalar.md#fn-tofloatornull) |
| `toInteger(input :: BOOLEAN \| STRING \| INTEGER \| FLOAT) :: INTEGER` | Scalar | — | [toInteger](../cypher/functions/scalar.md#fn-tointeger) |
| `toIntegerList(input :: VECTOR \| LIST<ANY>) :: LIST<INTEGER>` | List | — | [toIntegerList](../cypher/functions/list.md#fn-tointegerlist) |
| `toIntegerOrNull(input :: ANY) :: INTEGER` | Scalar | — | [toIntegerOrNull](../cypher/functions/scalar.md#fn-tointegerornull) |
| `toLower(input :: STRING) :: STRING` | String | — | [toLower](../cypher/functions/string.md#fn-tolower) |
| `toString(input :: ANY) :: STRING` | String | — | [toString](../cypher/functions/string.md#fn-tostring) |
| `toStringList(input :: LIST<ANY>) :: LIST<STRING>` | List | — | [toStringList](../cypher/functions/list.md#fn-tostringlist) |
| `toStringOrNull(input :: ANY) :: STRING` | String | — | [toStringOrNull](../cypher/functions/string.md#fn-tostringornull) |
| `toUpper(input :: STRING) :: STRING` | String | — | [toUpper](../cypher/functions/string.md#fn-toupper) |
| `trim([[LEADING \| TRAILING \| BOTH] [trimCharacterString :: STRING] FROM] input :: STRING) :: STRING` | String | — | [trim](../cypher/functions/string.md#fn-trim) |
| `type(input :: RELATIONSHIP) :: STRING` | Scalar | — | [type](../cypher/functions/scalar.md#fn-type) |
| `upper(input :: STRING) :: STRING` | String | — | [upper](../cypher/functions/string.md#fn-upper) |
| `uuid() :: UUID` | Scalar | — | [uuid](../cypher/functions/scalar.md#fn-uuid) |
| `uuid(mostSigBits :: INTEGER, leastSigBits :: INTEGER) :: UUID` | Scalar | — | [uuid](../cypher/functions/scalar.md#fn-uuid) |
| `uuid(name :: STRING) :: UUID` | Scalar | — | [uuid](../cypher/functions/scalar.md#fn-uuid) |
| `uuid.leastSignificantBits(uuid :: UUID) :: INTEGER` | Scalar | — | [uuid.leastSignificantBits](../cypher/functions/scalar.md#fn-uuid-leastsignificantbits) |
| `uuid.mostSignificantBits(uuid :: UUID) :: INTEGER` | Scalar | — | [uuid.mostSignificantBits](../cypher/functions/scalar.md#fn-uuid-mostsignificantbits) |
| `valueType(input :: ANY) :: STRING` | Scalar | — | [valueType](../cypher/functions/scalar.md#fn-valuetype) |
| `vector(vectorValue :: STRING \| LIST<INTEGER \| FLOAT>, dimension :: INTEGER, coordinateType :: [INTEGER64, INTEGER32, INTEGER16, INTEGER8, FLOAT64, FLOAT32]) :: VECTOR` | Scalar | — | [vector](../cypher/functions/vector.md#fn-vector) |
| `vector.similarity.cosine(a :: VECTOR \| LIST<INTEGER \| FLOAT>, b :: VECTOR \| LIST<INTEGER \| FLOAT>) :: FLOAT` | Vector | — | [vector.similarity.cosine](../cypher/functions/vector.md#fn-vector-similarity-cosine) |
| `vector.similarity.euclidean(a :: VECTOR \| LIST<INTEGER \| FLOAT>, b :: VECTOR \| LIST<INTEGER \| FLOAT>) :: FLOAT` | Vector | — | [vector.similarity.euclidean](../cypher/functions/vector.md#fn-vector-similarity-euclidean) |
| `vector_dimension_count(vector :: VECTOR) :: INTEGER` | Vector | — | [vector_dimension_count](../cypher/functions/vector.md#fn-vector_dimension_count) |
| `vector_distance(vector1 :: VECTOR, vector2 :: VECTOR, vectorDistanceMetric :: [EUCLIDEAN, EUCLIDEAN_SQUARED, MANHATTAN, COSINE, DOT, HAMMING]) :: FLOAT` | Vector | — | [vector_distance](../cypher/functions/vector.md#fn-vector_distance) |
| `vector_norm(vector :: VECTOR, vectorDistanceMetric :: [EUCLIDEAN, MANHATTAN]) :: FLOAT` | Vector | — | [vector_norm](../cypher/functions/vector.md#fn-vector_norm) |
| `zoned_datetime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: ZONED DATETIME` | Temporal | — | [zoned_datetime](../cypher/functions/temporal.md#fn-zoned_datetime) |
| `zoned_time(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: ZONED TIME` | Temporal | — | [zoned_time](../cypher/functions/temporal.md#fn-zoned_time) |
<!-- /generated: functions -->

更新和只读一致性检查见 [示例验证说明](../guide/examples/README.md#reference-generation)。
