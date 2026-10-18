# Temporal 与 Duration 函数

适用版本：**v0.3.2** · Cypher Profile：`CY25-2026.08`。

[Cypher 手册](../index.md) · [函数索引](../../reference/functions.md) · [值类型](../types.md) · [宿主 JSON 编码](../../reference/values.md)

本页完整说明日期、时间、带时区日期时间、Duration 的构造、时钟、截断与格式化函数。函数创建 Cypher 值；`format()` 返回 String，宿主 JSON 中的 tagged value 编码见 Values Reference。

<a id="temporal-construction"></a>

## 构造器共同规则

`date`、`localtime`、`time`、`localdatetime`、`datetime` 接受以下形式。签名中的 `DEFAULT_TEMPORAL_ARGUMENT` 表示省略参数，不能把该名称写入查询。

| 形式 | 行为 |
| --- | --- |
| `name()` | 使用当前 statement clock；默认时区 UTC |
| `name({timezone:'Asia/Shanghai'})` | 取同一 instant 在指定时区的当前表示；只包含 timezone 的 Map 可用于五种构造器 |
| `name(text)` | 解析该类型的 ISO 表示；支持精简日期、ISO week/ordinal/quarter 日期以及时间的小数秒形式 |
| `name(text, pattern)` | 按格式模板解析，两个参数必须是 String；解析模板见[模板规则](#temporal-patterns) |
| `name(temporal)` | 抽取可用分量或保留同类型值；允许的来源在各函数下列出 |
| `name(componentMap)` | 使用分量或从已有 temporal value 选择分量，再覆写显式字段 |

日期分量采用以下四种互斥形式。没有 base 时必须有 `year`；省略较小分量取起始值。使用 base 时未显式提供的分量继承 base。

| 日期形式 | 字段与默认 |
| --- | --- |
| Calendar | `year`、`month`（1–12，默认 1）、`day`（默认 1，必须在该月份范围内） |
| ISO week | `year`、`week`、`dayOfWeek`（1–7，周一为 1，默认 1） |
| Quarter | `year`、`quarter`（1–4）、`dayOfQuarter`（默认 1，范围由该季度决定） |
| Ordinal | `year`、`ordinalDay`（1–365/366） |

时间分量是 `hour`（0–23）、`minute`（0–59）、`second`（0–59）、`millisecond`、`microsecond`、`nanosecond`。没有 base 时省略的时间分量为 0；补充分量必须有其更大的分量或对应 base。单独指定一个小数秒字段时，可表达整秒内的相应单位；组合小数秒字段时每个字段为 0–999，按毫秒、微秒、纳秒相加。

`date`/`time`/`datetime` base 字段选择已有值的日期、时间或全部分量。`datetime` base 不能与 `date`/`time` base 同时使用；`date` 与 `time` 可以组合。`timezone` 与显式分量只可组合为带时区的 `time` 或 `datetime`，不能把本地类型暗中变成带时区类型。

`timezone` 接受 `Z`、`UTC`、数值 offset 或有效 IANA 名称。`ZONED DATETIME` 使用给定日期对应的时区规则；`ZONED TIME` 不含日期，使用当前日期确定命名时区的 offset。IANA zone 与显式 offset 同时出现在日期时间文本时必须匹配；DST gap 中不存在的本地时间被拒绝。DST overlap 可通过显式 offset 区分两个 instant。仅给 zone、没有显式 offset 的 overlap 默认选择较早的 instant。

构造器的整个参数或 pattern 为 `null` 时返回 `null`。不支持的输入类型、错误分量类型、日期/时间越界、无效日期或不匹配的解析输入产生 `TYPE_ERROR`；空 Map、未知字段、互斥分量或不支持的 pattern token 产生 `SEMANTIC_ERROR`。Map 内字段的 `null` 不表示“省略字段”。

<a id="fn-date"></a>

## `date()`

<!-- function-signatures: date -->
```text
date(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: DATE
```
<!-- /function-signatures -->

返回 `DATE`。String 可以是 `2026-10-15`、`20261015`、`2026-W42-4`、`2026-288` 或季度形式。直接输入 `DATE` 保留原值，输入 `LOCAL DATETIME`/`ZONED DATETIME` 抽取本地日期；不接受纯时间。Map 使用日期字段或 `date` base。返回值没有时区。

<!-- example: fn-date -->
```cypher
RETURN format(date('2026-10-15')) AS iso,
       format(date('15/10/2026','dd/MM/yyyy')) AS parsed,
       format(date({year:2026,week:42,dayOfWeek:4})) AS weekDate
```

预期 `['iso', 'parsed', 'weekDate']` 对应一行 `["2026-10-15", "2026-10-15", "2026-10-15"]`。

<a id="fn-localtime"></a>

## `localtime()`

<!-- function-signatures: localtime -->
```text
localtime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: LOCAL TIME
```
<!-- /function-signatures -->

返回不含时区的 `LOCAL TIME`。直接输入 LocalTime、Time、LocalDateTime 或 ZonedDateTime，抽取其本地时分秒；丢弃 offset，不换算为 UTC。Map 使用时间分量和 `time` base。`localtime({timezone:...})` 只获取当前本地时间，不在返回值中保留 zone。

<!-- example: fn-localtime -->
```cypher
RETURN format(localtime('12:34:56.125')) AS iso,
       format(localtime('12-34','HH-mm')) AS parsed
```

预期 `['iso', 'parsed']` 对应一行 `["12:34:56.125", "12:34:00"]`。

<a id="fn-time"></a>

## `time()`

<!-- function-signatures: time -->
```text
time(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: ZONED TIME
```
<!-- /function-signatures -->

返回 `ZONED TIME`，保留 local time 和 offset。String 缺省 offset 时使用 UTC。输入 Time 保留原值，输入 LocalTime/LocalDateTime 补 UTC，输入 ZonedDateTime 抽取其本地时间及 offset。Map 支持时间字段、`time` base 与 `timezone`；变更 base 的 timezone 时保持 instant 对应的时间关系。

<!-- example: fn-time -->
```cypher
RETURN format(time('12:34:56+02:00')) AS iso,
       format(time('12-34+02:00','HH-mmXXX')) AS parsed
```

预期 `['iso', 'parsed']` 对应一行 `["12:34:56+02:00", "12:34:00+02:00"]`。

<a id="fn-localdatetime"></a>

## `localdatetime()`

<!-- function-signatures: localdatetime -->
```text
localdatetime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: LOCAL DATETIME
```
<!-- /function-signatures -->

返回 `LOCAL DATETIME`。无 zone 的日期时间文本可精确到日期，此时时间为午夜；直接输入 Date 补午夜，输入 LocalDateTime 保留，输入 ZonedDateTime 抽取本地分量并丢弃 zone。Map 支持日期和时间分量，可组合 `date`/`time` 或使用 `datetime` base。

<!-- example: fn-localdatetime -->
```cypher
RETURN format(localdatetime('2026-10-15T12:34:56')) AS iso,
       format(localdatetime('15/10/2026 12:34','dd/MM/yyyy HH:mm')) AS parsed
```

预期 `['iso', 'parsed']` 对应一行 `["2026-10-15T12:34:56", "2026-10-15T12:34:00"]`。

<a id="fn-datetime"></a>

## `datetime()`

<!-- function-signatures: datetime -->
```text
datetime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: ZONED DATETIME
```
<!-- /function-signatures -->

返回 `ZONED DATETIME`。普通 ISO String 必须有 offset 或 `[IANA/Zone]`；pattern 解析若没有 zone 则补 UTC。直接输入 ZonedDateTime 保留，输入 LocalDateTime 或 Date 补 UTC，Date 的时间为午夜。Map 支持日期/时间/base 与 timezone，还接受 `epochSeconds` 或 `epochMillis`，二者互斥；epoch 形式只可再组合 `nanosecond`（0–999999999）及 `timezone`。不要把 epoch 字段与 year/month 等日历字段混合。

<!-- example: fn-datetime -->
```cypher
RETURN format(datetime('2026-10-15T12:34:56+02:00')) AS iso,
       format(datetime('15/10/2026 12:34+02:00','dd/MM/yyyy HH:mmXXX')) AS parsed
```

预期 `['iso', 'parsed']` 对应一行 `["2026-10-15T12:34:56+02:00", "2026-10-15T12:34:00+02:00"]`。

<a id="fn-local_time"></a>

## `local_time()`

<!-- function-signatures: local_time -->
```text
local_time(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: LOCAL TIME
```
<!-- /function-signatures -->

这是 [`localtime()`](#fn-localtime) 的同义函数名，输入形式、可选 pattern、返回类型、默认 clock、null 和错误行为完全相同。使用带下划线的名称不会改变时区或解析规则。

<!-- example: fn-local_time -->
```cypher
RETURN format(local_time('12:34:56')) AS value
```

预期 `['value']` 对应一行 `["12:34:56"]`。

<a id="fn-zoned_time"></a>

## `zoned_time()`

<!-- function-signatures: zoned_time -->
```text
zoned_time(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: ZONED TIME
```
<!-- /function-signatures -->

这是 [`time()`](#fn-time) 的同义函数名，输入形式、可选 pattern、返回类型、默认 clock、null 和错误行为完全相同。使用带下划线的名称不会改变时区或解析规则。

<!-- example: fn-zoned_time -->
```cypher
RETURN format(zoned_time('12:34:56Z')) AS value
```

预期 `['value']` 对应一行 `["12:34:56Z"]`。

<a id="fn-local_datetime"></a>

## `local_datetime()`

<!-- function-signatures: local_datetime -->
```text
local_datetime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: LOCAL DATETIME
```
<!-- /function-signatures -->

这是 [`localdatetime()`](#fn-localdatetime) 的同义函数名，输入形式、可选 pattern、返回类型、默认 clock、null 和错误行为完全相同。使用带下划线的名称不会改变时区或解析规则。

<!-- example: fn-local_datetime -->
```cypher
RETURN format(local_datetime('2026-10-15T12:34:56')) AS value
```

预期 `['value']` 对应一行 `["2026-10-15T12:34:56"]`。

<a id="fn-zoned_datetime"></a>

## `zoned_datetime()`

<!-- function-signatures: zoned_datetime -->
```text
zoned_datetime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY[, pattern :: STRING]) :: ZONED DATETIME
```
<!-- /function-signatures -->

这是 [`datetime()`](#fn-datetime) 的同义函数名，输入形式、可选 pattern、返回类型、默认 clock、null 和错误行为完全相同。使用带下划线的名称不会改变时区或解析规则。

<!-- example: fn-zoned_datetime -->
```cypher
RETURN format(zoned_datetime('2026-10-15T12:34:56Z')) AS value
```

预期 `['value']` 对应一行 `["2026-10-15T12:34:56Z"]`。

<a id="temporal-clocks"></a>

## 三种 clock 的执行边界

无参数构造器与 `timestamp()` 默认使用 statement clock。每次 top-level `lithograph()` 或 `lithograph_rows()` invocation 是一个 statement；跨 stream batch 保持同一 instant。普通 auto-commit execution 的 transaction clock 与 statement clock相同。

`lithograph_tx_begin()` 成功时固定 explicit transaction clock，同一 active transaction 中多次 invocation 的 `.transaction()` 保持稳定，而 `.statement()` 每次重取。caller-owned SQLite `BEGIN` 并不会将多个普通 invocation 合并成同一个 Cypher temporal transaction。`IN TRANSACTIONS` 的各 inner batch 各自有 transaction clock。

以下 clock 函数的 timezone 参数可省略；推荐显式使用 timezone String，或仅含 `timezone` 的 Map。timezone 只改变同一 instant 的表示。显式传 `null` 返回 `null`；无效 timezone 或错误类型按构造规则报错。`.realtime()` 每次调用读取 wall clock，所以两个连续结果不保证相等。

<a id="fn-date-statement"></a>

## `date.statement()`

<!-- function-signatures: date.statement -->
```text
date.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: DATE
```
<!-- /function-signatures -->

返回 `DATE`。读取本次 statement 固定的 instant；多次调用与跨 cursor batch 稳定。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `DATE` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-date-statement -->
```cypher
RETURN date.statement('UTC') = date.statement('UTC') AS stable
```

预期 `['stable']` 对应一行 `[true]`。

<a id="fn-date-transaction"></a>

## `date.transaction()`

<!-- function-signatures: date.transaction -->
```text
date.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: DATE
```
<!-- /function-signatures -->

返回 `DATE`。读取当前 Cypher transaction 固定的 instant；explicit transaction 内跨多次 statement 稳定。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `DATE` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-date-transaction -->
```cypher
RETURN date.transaction('UTC') = date.transaction('UTC') AS stable
```

预期 `['stable']` 对应一行 `[true]`。

<a id="fn-date-realtime"></a>

## `date.realtime()`

<!-- function-signatures: date.realtime -->
```text
date.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: DATE
```
<!-- /function-signatures -->

返回 `DATE`。在每次求值时读取当前 wall clock；不提供 statement 内稳定性保证。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `DATE` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-date-realtime -->
```cypher
RETURN date.realtime('UTC') IS NOT NULL AS available
```

预期 `['available']` 对应一行 `[true]`。

<a id="fn-localtime-statement"></a>

## `localtime.statement()`

<!-- function-signatures: localtime.statement -->
```text
localtime.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL TIME
```
<!-- /function-signatures -->

返回 `LOCAL TIME`。读取本次 statement 固定的 instant；多次调用与跨 cursor batch 稳定。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `LOCAL TIME` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-localtime-statement -->
```cypher
RETURN localtime.statement('UTC') = localtime.statement('UTC') AS stable
```

预期 `['stable']` 对应一行 `[true]`。

<a id="fn-localtime-transaction"></a>

## `localtime.transaction()`

<!-- function-signatures: localtime.transaction -->
```text
localtime.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL TIME
```
<!-- /function-signatures -->

返回 `LOCAL TIME`。读取当前 Cypher transaction 固定的 instant；explicit transaction 内跨多次 statement 稳定。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `LOCAL TIME` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-localtime-transaction -->
```cypher
RETURN localtime.transaction('UTC') = localtime.transaction('UTC') AS stable
```

预期 `['stable']` 对应一行 `[true]`。

<a id="fn-localtime-realtime"></a>

## `localtime.realtime()`

<!-- function-signatures: localtime.realtime -->
```text
localtime.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL TIME
```
<!-- /function-signatures -->

返回 `LOCAL TIME`。在每次求值时读取当前 wall clock；不提供 statement 内稳定性保证。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `LOCAL TIME` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-localtime-realtime -->
```cypher
RETURN localtime.realtime('UTC') IS NOT NULL AS available
```

预期 `['available']` 对应一行 `[true]`。

<a id="fn-time-statement"></a>

## `time.statement()`

<!-- function-signatures: time.statement -->
```text
time.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED TIME
```
<!-- /function-signatures -->

返回 `ZONED TIME`。读取本次 statement 固定的 instant；多次调用与跨 cursor batch 稳定。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `ZONED TIME` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-time-statement -->
```cypher
RETURN time.statement('UTC') = time.statement('UTC') AS stable
```

预期 `['stable']` 对应一行 `[true]`。

<a id="fn-time-transaction"></a>

## `time.transaction()`

<!-- function-signatures: time.transaction -->
```text
time.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED TIME
```
<!-- /function-signatures -->

返回 `ZONED TIME`。读取当前 Cypher transaction 固定的 instant；explicit transaction 内跨多次 statement 稳定。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `ZONED TIME` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-time-transaction -->
```cypher
RETURN time.transaction('UTC') = time.transaction('UTC') AS stable
```

预期 `['stable']` 对应一行 `[true]`。

<a id="fn-time-realtime"></a>

## `time.realtime()`

<!-- function-signatures: time.realtime -->
```text
time.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED TIME
```
<!-- /function-signatures -->

返回 `ZONED TIME`。在每次求值时读取当前 wall clock；不提供 statement 内稳定性保证。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `ZONED TIME` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-time-realtime -->
```cypher
RETURN time.realtime('UTC') IS NOT NULL AS available
```

预期 `['available']` 对应一行 `[true]`。

<a id="fn-localdatetime-statement"></a>

## `localdatetime.statement()`

<!-- function-signatures: localdatetime.statement -->
```text
localdatetime.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL DATETIME
```
<!-- /function-signatures -->

返回 `LOCAL DATETIME`。读取本次 statement 固定的 instant；多次调用与跨 cursor batch 稳定。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `LOCAL DATETIME` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-localdatetime-statement -->
```cypher
RETURN localdatetime.statement('UTC') = localdatetime.statement('UTC') AS stable
```

预期 `['stable']` 对应一行 `[true]`。

<a id="fn-localdatetime-transaction"></a>

## `localdatetime.transaction()`

<!-- function-signatures: localdatetime.transaction -->
```text
localdatetime.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL DATETIME
```
<!-- /function-signatures -->

返回 `LOCAL DATETIME`。读取当前 Cypher transaction 固定的 instant；explicit transaction 内跨多次 statement 稳定。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `LOCAL DATETIME` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-localdatetime-transaction -->
```cypher
RETURN localdatetime.transaction('UTC') = localdatetime.transaction('UTC') AS stable
```

预期 `['stable']` 对应一行 `[true]`。

<a id="fn-localdatetime-realtime"></a>

## `localdatetime.realtime()`

<!-- function-signatures: localdatetime.realtime -->
```text
localdatetime.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL DATETIME
```
<!-- /function-signatures -->

返回 `LOCAL DATETIME`。在每次求值时读取当前 wall clock；不提供 statement 内稳定性保证。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `LOCAL DATETIME` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-localdatetime-realtime -->
```cypher
RETURN localdatetime.realtime('UTC') IS NOT NULL AS available
```

预期 `['available']` 对应一行 `[true]`。

<a id="fn-datetime-statement"></a>

## `datetime.statement()`

<!-- function-signatures: datetime.statement -->
```text
datetime.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED DATETIME
```
<!-- /function-signatures -->

返回 `ZONED DATETIME`。读取本次 statement 固定的 instant；多次调用与跨 cursor batch 稳定。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `ZONED DATETIME` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-datetime-statement -->
```cypher
RETURN datetime.statement('UTC') = datetime.statement('UTC') AS stable
```

预期 `['stable']` 对应一行 `[true]`。

<a id="fn-datetime-transaction"></a>

## `datetime.transaction()`

<!-- function-signatures: datetime.transaction -->
```text
datetime.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED DATETIME
```
<!-- /function-signatures -->

返回 `ZONED DATETIME`。读取当前 Cypher transaction 固定的 instant；explicit transaction 内跨多次 statement 稳定。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `ZONED DATETIME` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-datetime-transaction -->
```cypher
RETURN datetime.transaction('UTC') = datetime.transaction('UTC') AS stable
```

预期 `['stable']` 对应一行 `[true]`。

<a id="fn-datetime-realtime"></a>

## `datetime.realtime()`

<!-- function-signatures: datetime.realtime -->
```text
datetime.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED DATETIME
```
<!-- /function-signatures -->

返回 `ZONED DATETIME`。在每次求值时读取当前 wall clock；不提供 statement 内稳定性保证。省略 timezone 使用 UTC；传入 timezone 时将该 instant 转换后再抽取 `ZONED DATETIME` 分量。参数、null 与失败行为见[共同 clock 规则](#temporal-clocks)。

<!-- example: fn-datetime-realtime -->
```cypher
RETURN datetime.realtime('UTC') IS NOT NULL AS available
```

预期 `['available']` 对应一行 `[true]`。

<a id="fn-timestamp"></a>

## `timestamp()`

<!-- function-signatures: timestamp -->
```text
timestamp() :: INTEGER
```
<!-- /function-signatures -->

无参数。返回本次 statement 固定 instant 距 Unix epoch 的毫秒数，类型为 `INTEGER`；它与 `datetime.statement().epochMillis` 对应，不是每次读取 wall clock。没有输入，因此不存在 null 传播；传入参数会产生语义或参数数量错误。

<!-- example: fn-timestamp -->
```cypher
RETURN timestamp() = datetime.statement().epochMillis AS sameInstant
```

预期 `['sameInstant']` 对应一行 `[true]`。

<a id="temporal-truncation"></a>

## 截断的共同规则

`name.truncate(unit [, input [, fields]])` 先将 input 转成目标 temporal 类型，再把比 unit 更小的分量重置到起始值。省略 input 使用当前 statement 时间；`fields` 是可选覆写 Map，只能提供比 unit 更小的合法字段。`week` 以周一为起点，`weekYear` 以 ISO week year 第一周周一为起点。

| 返回 family | 支持的 unit（不区分大小写） | 接受的 input |
| --- | --- | --- |
| DATE | `millennium`、`century`、`decade`、`year`、`weekYear`、`quarter`、`month`、`week`、`day` | Date、LocalDateTime、ZonedDateTime |
| LOCAL TIME / ZONED TIME | `day`、`hour`、`minute`、`second`、`millisecond`、`microsecond`、`nanosecond` | 任意 instant 类型；Date 作为午夜；本地来源补 UTC，带 zone 来源保留 offset（仅 ZONED TIME） |
| LOCAL DATETIME / ZONED DATETIME | 上述日期与时间 unit 全集 | Date、LocalDateTime、ZonedDateTime；Date 只可截到 day 或更大单位 |

日期时间截到日期单位时同时清空时间。省略 fields 与显式传 `null` 不同：前者直接返回截断结果，后者传播 `null`。任何显式参数为 null 都返回 null。`fields` 不能覆写 unit 本身或更大的分量；带时区类型允许通过 `timezone` 调整表示，本地类型不接受这一覆写。比如 month 截断后可覆写 day，millisecond 截断后可覆写 nanosecond，不能覆写 year。

错误 input/type 产生 `TYPE_ERROR`；不支持的 unit、不可用或不小于 unit 的覆写字段产生 `SEMANTIC_ERROR`；覆写后的非法日期或时间仍报错，不自动移到下一个月。

<a id="fn-date-truncate"></a>

## `date.truncate()`

<!-- function-signatures: date.truncate -->
```text
date.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: DATE
```
<!-- /function-signatures -->

返回 `DATE`；日期时间来源先抽取本地日期。常用于按周/月分组的日期键。 unit、input 转换、fields 和 null/error 规则见[截断共同规则](#temporal-truncation)。

<!-- example: fn-date-truncate -->
```cypher
RETURN format(date.truncate('month',date('2026-10-15'),{day:3})) AS value
```

预期 `['value']` 对应一行 `["2026-10-03"]`。

<a id="fn-localtime-truncate"></a>

## `localtime.truncate()`

<!-- function-signatures: localtime.truncate -->
```text
localtime.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: LOCAL TIME
```
<!-- /function-signatures -->

返回 `LOCAL TIME`；带时区来源只保留本地时间。 unit、input 转换、fields 和 null/error 规则见[截断共同规则](#temporal-truncation)。

<!-- example: fn-localtime-truncate -->
```cypher
RETURN format(localtime.truncate('minute',localtime('12:34:56.789'))) AS value
```

预期 `['value']` 对应一行 `["12:34:00"]`。

<a id="fn-time-truncate"></a>

## `time.truncate()`

<!-- function-signatures: time.truncate -->
```text
time.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: ZONED TIME
```
<!-- /function-signatures -->

返回 `ZONED TIME`；保留来源 offset，再应用可选 timezone 覆写。 unit、input 转换、fields 和 null/error 规则见[截断共同规则](#temporal-truncation)。

<!-- example: fn-time-truncate -->
```cypher
RETURN format(time.truncate('hour',time('12:34:56+02:00'))) AS value
```

预期 `['value']` 对应一行 `["12:00:00+02:00"]`。

<a id="fn-localdatetime-truncate"></a>

## `localdatetime.truncate()`

<!-- function-signatures: localdatetime.truncate -->
```text
localdatetime.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: LOCAL DATETIME
```
<!-- /function-signatures -->

返回 `LOCAL DATETIME`；带时区输入按本地分量截断后丢弃 zone。 unit、input 转换、fields 和 null/error 规则见[截断共同规则](#temporal-truncation)。

<!-- example: fn-localdatetime-truncate -->
```cypher
RETURN format(localdatetime.truncate('day',localdatetime('2026-10-15T12:34:56'))) AS value
```

预期 `['value']` 对应一行 `["2026-10-15T00:00:00"]`。

<a id="fn-datetime-truncate"></a>

## `datetime.truncate()`

<!-- function-signatures: datetime.truncate -->
```text
datetime.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: ZONED DATETIME
```
<!-- /function-signatures -->

返回 `ZONED DATETIME`；使用本地日期时间执行截断并按 zone 重新确定 offset，因此要考虑时区转换边界。 unit、input 转换、fields 和 null/error 规则见[截断共同规则](#temporal-truncation)。

<!-- example: fn-datetime-truncate -->
```cypher
RETURN format(datetime.truncate('day',datetime('2026-10-15T12:34:56+02:00'))) AS value
```

预期 `['value']` 对应一行 `["2026-10-15T00:00:00+02:00"]`。

<a id="fn-datetime-fromepoch"></a>

## `datetime.fromEpoch()`

<!-- function-signatures: datetime.fromEpoch -->
```text
datetime.fromEpoch(seconds :: INTEGER | FLOAT, nanoseconds :: INTEGER | FLOAT) :: ZONED DATETIME
```
<!-- /function-signatures -->

将 Unix epoch 秒数与附加纳秒相加，返回 UTC `ZONED DATETIME`。两个参数都必需，可为 Integer 或有限 Float；Float 分量各自向零截断到整数。纳秒可超出一秒并归一到 instant，支持负 epoch。任一参数 null 返回 null；非数值、非有限值或 temporal 范围溢出产生 `TYPE_ERROR`。

<!-- example: fn-datetime-fromepoch -->
```cypher
RETURN format(datetime.fromEpoch(0,500000000)) AS value
```

预期 `['value']` 对应一行 `["1970-01-01T00:00:00.5Z"]`。

<a id="fn-datetime-fromepochmillis"></a>

## `datetime.fromEpochMillis()`

<!-- function-signatures: datetime.fromEpochMillis -->
```text
datetime.fromEpochMillis(milliseconds :: INTEGER | FLOAT) :: ZONED DATETIME
```
<!-- /function-signatures -->

将 Unix epoch 毫秒转为 UTC `ZONED DATETIME`。唯一参数必需；有限 Float 毫秒先向零截断，因此小数毫秒不会保留为纳秒。null 返回 null；非数值、非有限值及范围溢出产生 `TYPE_ERROR`。

<!-- example: fn-datetime-fromepochmillis -->
```cypher
RETURN format(datetime.fromEpochMillis(1500)) AS value
```

预期 `['value']` 对应一行 `["1970-01-01T00:00:01.5Z"]`。

<a id="fn-duration"></a>

## `duration()`

<!-- function-signatures: duration -->
```text
duration(input :: ANY[, pattern :: STRING]) :: DURATION
```
<!-- /function-signatures -->

构造包含独立 months、days、seconds 与 nanoseconds 组的 `DURATION`。input 必需，接受现有 Duration、ISO String（例如 `P1M2DT3H`）或分量 Map。Map 可用 `years`、`months`、`weeks`、`days`、`hours`、`minutes`、`seconds`、`milliseconds`、`microseconds`、`nanoseconds`；省略字段为 0，数值允许有限小数。小数月份按平均月长度 30.436875 天换算，小数天按 86400 秒换算。月份、天数不会无条件互相归并。

传 pattern 时 input 必须为 String；见[模板规则](#temporal-patterns)。整个 input 或 pattern 为 null 返回 null；Map 字段 null、非数值、不可解析文本、非有限值和溢出产生 `TYPE_ERROR`，空 Map或未知字段产生 `SEMANTIC_ERROR`。Duration 用于与 temporal 值相加/相减或缩放；月份运算会将日期夹到目标月份最后一天。

<!-- example: fn-duration -->
```cypher
RETURN format(duration({months:1,days:2,hours:3})) AS value,
       format(duration('2 hours 30 minutes',"h 'hours' m 'minutes'")) AS parsed
```

预期 `['value', 'parsed']` 对应一行 `["P1M2DT3H", "PT2H30M"]`。

<a id="duration-differences"></a>

## Duration 差值共同规则

`from` 与 `to` 必需，接受 Date、LocalTime、Time、LocalDateTime、ZonedDateTime。返回从前者到后者的带符号 Duration。任一参数 null 返回 null；非 instant 输入、不可表达的范围或计算溢出产生 `TYPE_ERROR`。

当一方缺少日期时，使用另一方日期对齐；缺少时间按午夜处理；本地值与 ZonedDateTime 比较时使用另一方 zone。带时区的 elapsed seconds 依据真实 instant，能反映 DST，而日历月份保留日历含义。不要通过假定“每月 30 天”把 between、inMonths 与 inSeconds 互换。

<a id="fn-duration-between"></a>

## `duration.between()`

<!-- function-signatures: duration.between -->
```text
duration.between(from :: ANY, to :: ANY) :: DURATION
```
<!-- /function-signatures -->

按完整月份及剩余天/秒/纳秒分解逻辑差值。没有日期的输入返回秒差。 输入补全、null 与错误规则见[差值共同规则](#duration-differences)。

<!-- example: fn-duration-between -->
```cypher
RETURN format(duration.between(date('2026-01-31'),date('2026-03-01'))) AS value
```

预期 `['value']` 对应一行 `["P1M1D"]`。

<a id="fn-duration_between"></a>

## `duration_between()`

<!-- function-signatures: duration_between -->
```text
duration_between(from :: ANY, to :: ANY) :: DURATION
```
<!-- /function-signatures -->

与 `duration.between()` 完全相同的函数名；相同参数得到相同 Duration。 输入补全、null 与错误规则见[差值共同规则](#duration-differences)。

<!-- example: fn-duration_between -->
```cypher
RETURN format(duration_between(date('2026-01-31'),date('2026-03-01'))) AS value
```

预期 `['value']` 对应一行 `["P1M1D"]`。

<a id="fn-duration-inmonths"></a>

## `duration.inMonths()`

<!-- function-signatures: duration.inMonths -->
```text
duration.inMonths(from :: ANY, to :: ANY) :: DURATION
```
<!-- /function-signatures -->

只返回完整月份差，舍弃不足一个月的部分；没有日期的输入返回零 Duration。 输入补全、null 与错误规则见[差值共同规则](#duration-differences)。

<!-- example: fn-duration-inmonths -->
```cypher
RETURN format(duration.inMonths(date('2026-01-15'),date('2026-03-20'))) AS value
```

预期 `['value']` 对应一行 `["P2M"]`。

<a id="fn-duration-indays"></a>

## `duration.inDays()`

<!-- function-signatures: duration.inDays -->
```text
duration.inDays(from :: ANY, to :: ANY) :: DURATION
```
<!-- /function-signatures -->

只返回完整天数，舍弃不足一天的时间；对带时区 instant 按 elapsed time 的完整 86400 秒计数。 输入补全、null 与错误规则见[差值共同规则](#duration-differences)。

<!-- example: fn-duration-indays -->
```cypher
RETURN format(duration.inDays(date('2026-10-01'),date('2026-10-15'))) AS value
```

预期 `['value']` 对应一行 `["P14D"]`。

<a id="fn-duration-inseconds"></a>

## `duration.inSeconds()`

<!-- function-signatures: duration.inSeconds -->
```text
duration.inSeconds(from :: ANY, to :: ANY) :: DURATION
```
<!-- /function-signatures -->

返回 seconds/nanoseconds 组的 elapsed duration；不把秒提升为月份或天，因此适合计算真实耗时。 输入补全、null 与错误规则见[差值共同规则](#duration-differences)。

<!-- example: fn-duration-inseconds -->
```cypher
RETURN format(duration.inSeconds(datetime('2026-10-15T00:00:00Z'),datetime('2026-10-15T00:01:30.5Z'))) AS value
```

预期 `['value']` 对应一行 `["PT1M30.5S"]`。

<a id="fn-format"></a>

## `format()`

<!-- function-signatures: format -->
```text
format(value :: DATE | LOCAL TIME | ZONED TIME | LOCAL DATETIME | ZONED DATETIME | DURATION[, pattern :: STRING]) :: STRING
```
<!-- /function-signatures -->

返回 temporal/Duration 的 String 表示。只传 value 时输出 ISO 形式；ZonedDateTime 保留 offset，并为命名 zone 添加 `[Zone]`。指定 pattern 时按下表输出。null value 或 pattern 返回 null；非 temporal 输入产生 `TYPE_ERROR`；模板要求当前值没有的日期/时间/zone 分量、未知 token、非法 token 宽度和未闭合引号产生 `SEMANTIC_ERROR`。模板不是宿主 `strftime`，也没有隐式 locale 参数。

<!-- example: fn-format -->
```cypher
RETURN format(date('2026-10-15')) AS iso,
       format(localdatetime('2026-10-15T12:34:56'),'dd/MM/yyyy HH:mm') AS display
```

预期 `['iso', 'display']` 对应一行 `["2026-10-15", "15/10/2026 12:34"]`。

<a id="temporal-patterns"></a>

## 解析与输出模板

重复字母控制宽度，`'literal'` 包住文本；在 Cypher String 中用双引号包含带单引号的模板。解析和输出支持的 token 不完全相同，不能假定 `format()` 的任意输出模板都可反向解析。

| 分量 | 解析 token | `format()` token 与含义 |
| --- | --- | --- |
| 年代/年 | `G`、`u`、`y`、`Y` | `G` 年代，`u` 公元年份，`y` 纪元内年份，`Y` ISO week year；重复控制宽度 |
| 月 | `M`、`L`；1–2 数字、3 英文缩写、4 英文全名 | 同上；宽度 5 为首字母，解析不接受歧义的首字母月份 |
| 日期/周 | `d`、`D`、`E`、`e`、`c`、`w` | `d` 月内日，`D` 年内日，`E/e/c` 星期；`w` ISO week，`F/W` 月内周序号，`Q/q` 季度，`g` Julian day |
| 时分秒 | `H`、`k`、`h`、`K`、`m`、`s`、`a` | `H` 0–23、`k` 1–24、`h` 1–12、`K` 0–11；`m/s` 分/秒，`a` AM/PM，`B` 时段英文文本 |
| 小数秒 | `S`（1–9 位） | `S` 秒的小数部分（1–9 位），`n` 秒内纳秒，`N` 当日纳秒，`A` 当日毫秒 |
| 时区 | `X`、`x`、`Z`，宽度 3 使用含冒号 offset | `X/x/Z` offset；`O` 本地化 offset，`VV` zone id，`v`（宽度 1/4）与 `z`（宽度 1–4）zone name |
| 补齐 | 不支持 `p` | `p` 重复次数指定后一个 token 的空格补齐宽度 |

`DATE` 没有时间，Local 类型没有 offset；模板要求缺失分量时失败。Duration 的 pattern 是单位模板，`y/Y/u` 年、`q/Q` 季度、`M/L` 月、`w/W` 周、`d/D` 天、`h/H/k/K` 小时、`m` 分、`s` 秒、`A` 毫秒、`n/S/N` 纳秒。解析时各单位相加；输出时出现较大单位会先拆出该单位，再输出余量（例如使用 `h` 后 `m` 是不足一小时的余分）。

## null 与失败示例

<!-- example: fn-temporal-null -->
```cypher
RETURN date(null) AS missingDate,
       duration.between(null,date('2026-10-15')) AS missingDuration,
       date.truncate('month',date('2026-10-15'),null) AS explicitNull
```

预期 `['missingDate', 'missingDuration', 'explicitNull']` 对应一行 `[null, null, null]`。

<!-- example: fn-temporal-invalid-date -->
```cypher
RETURN date('2026-02-30') AS value
```

预期错误类别：`TYPE_ERROR`。

<!-- example: fn-temporal-invalid-map -->
```cypher
RETURN date({}) AS value
```

预期错误类别：`SEMANTIC_ERROR`。

标准语义参考：[Neo4j Cypher 25 temporal functions](https://neo4j.com/docs/cypher-manual/25/functions/temporal/)。本文说明冻结 Profile 的 Lithograph 行为，在线手册后续添加项以本地函数索引为准。
