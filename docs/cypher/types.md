# 值、类型与 Property

[语言手册](index.md) · [版本与范围](../reference/cypher-compatibility.md)

**适用版本：v0.3.3，Cypher Profile：`CY25-2026.08`。** 本章说明 Cypher 查询内部的值与类型。SQLite 参数、结果中的 JSON wrapper、整数精度和解码规则见 [Value 与 Lithograph JSON](../reference/values.md)。类型的相等、比较和排序规则见 [null、相等与排序](null-and-ordering.md)。

<a id="value-families"></a>

## 值类型总览

每个表达式计算得到一个值。值可以绑定到变量、放进查询结果或传给函数；能否作为参数、能否保存为图 Property，是另外两个问题。

| 类型 | 例子或来源 | 可作为参数 | 可作为单个 Property |
| --- | --- | --- | --- |
| `BOOLEAN` | `true`、`false` | 是 | 是 |
| `INTEGER` | `42`，signed 64-bit | 是 | 是 |
| `FLOAT` | `1.5`、`1e3` | 是 | 是 |
| `STRING` | `'你好'` | 是 | 是 |
| `DATE` | `date('2026-10-15')` | 是 | 是 |
| `LOCAL TIME` | `localtime('12:30:00')` | 是 | 是 |
| `ZONED TIME` | `time('12:30:00+08:00')` | 是 | 是 |
| `LOCAL DATETIME` | `localdatetime('2026-10-15T12:30:00')` | 是 | 是 |
| `ZONED DATETIME` | `datetime('2026-10-15T12:30:00+08:00')` | 是 | 是 |
| `DURATION` | `duration('P1DT2H')` | 是 | 是 |
| `POINT` | `point({x:1,y:2})` | 是 | 是 |
| `VECTOR` | `vector([1,2],2,FLOAT32)` | 是 | 是 |
| `UUID` | `uuid('550e8400-e29b-41d4-a716-446655440000')` | 是 | 是 |
| `LIST` | `[1,2]`、`[1,'x',null]` | 元素递归满足参数要求时 | 仅满足下述 Property List 规则时 |
| `MAP` | `{name:'Ada',values:[1,2]}` | 值递归满足参数要求时 | 否 |
| `NODE` | `MATCH (n) RETURN n` 中的 `n` | 否 | 否 |
| `RELATIONSHIP` | `MATCH ()-[r]->() RETURN r` 中的 `r` | 否 | 否 |
| `PATH` | `MATCH p = ()-->() RETURN p` 中的 `p` | 否 | 否 |
| `null` | 缺失或未知值 | 是 | 赋值表示删除 Property |

`null`、空 List、空 Map、空字符串、`0` 和 `false` 是不同的值。`ANY` 等宽泛类型是类型表达式，不能通过 `ANY(...)` 创建另一种运行时值。

<a id="numeric-values"></a>

## Boolean、Integer、Float 与 String

`BOOLEAN` 只有 `true`、`false` 两个非 null 值。`WHERE` 和逻辑运算符接受 Boolean 或 null，不把数字、字符串或容器隐式转换为真假。

`INTEGER` 范围为 `-9223372036854775808` 至 `9223372036854775807`。字面量超过范围、整数算术溢出返回 `TYPE_ERROR`，不会自动升级成任意精度整数。`FLOAT` 使用 IEEE 754 binary64；大整数转换为 Float 可能丢失精度，宿主也必须按 JSON Reference 保留超出 JavaScript 安全整数范围的 Integer。

Integer 与 Float 仍是不同类型，但 `=` 和大小比较按数值比较，不会先把所有 Integer 无条件舍入为 Float。

<!-- example: foundation-values-numeric -->
```cypher
RETURN valueType(1) AS integerType,
       valueType(1.0) AS floatType,
       valueType(null) AS nullType,
       1 = 1.0 AS equal
```

结果为 `['INTEGER NOT NULL','FLOAT NOT NULL','NULL',true]`。`valueType()` 返回值的运行时类型，不将 Float 的整数部分等同于 Integer 类型。

<!-- example: foundation-values-overflow -->
```cypher
RETURN 9223372036854775807 + 1 AS overflow
```

该例返回 `TYPE_ERROR`。`NaN` 和正负 Infinity 属于 Float；它们的比较、排序和 JSON 表示见 [非有限数值](null-and-ordering.md#nan-and-infinity)。

`STRING` 保存 Unicode 文本。字符串字面量、转义与标识符语法见 [语法基础](syntax.md)，字符串函数见 [String 函数](functions/string.md)。文本 `'42'` 不会在比较或赋值时自动成为数字 `42`；需要转换时显式调用转换函数。

<a id="containers"></a>

## List 与 Map

查询中的 List 有顺序、允许重复、允许异构值与 null，也允许嵌套 List/Map。List 下标从 `0` 开始，负下标从末尾计算；越界下标返回 null。切片、comprehension 和 Map projection 见 [表达式](expressions.md)。

Map 的 key 是字符串，key 不重复。`m.key` 与 `m['key']` 访问同一项；缺失 key 返回 null。Map 的 key 顺序不是应用可以依赖的结果合同。`keys()`、`properties()`、List 函数见 [函数索引](../reference/functions.md)。

<!-- example: foundation-values-containers -->
```cypher
RETURN valueType([1,null]) AS nullableList,
       valueType([]) AS emptyList,
       [1,'x',null] AS mixed,
       {name:'Ada',values:[1,2]} AS record
```

结果分别是 `LIST<INTEGER> NOT NULL`、`LIST<NOTHING> NOT NULL`、`[1,'x',null]` 和 `{name:'Ada',values:[1,2]}`。外层 `NOT NULL` 表示 List 本身存在；`LIST<INTEGER>` 的元素仍允许 null。空 List 没有可观察的元素类型，因此 `valueType([])` 使用 `NOTHING`。

<a id="graph-values"></a>

## Node、Relationship 与 Path

Node 具有 identity、零到多个 Label 和 Property。Relationship 具有独立 identity、一个 Type、有方向的两个端点和 Property；允许 self-loop 与平行关系。Path 是按遍历次序排列的 Node/Relationship 序列，它不是新的持久化图元素。

<!-- example: foundation-values-graph -->
```cypher
CREATE p = (a:TypeDemo {name:'Ada'})-[r:LINK {active:true}]->(b:TypeDemo {name:'Lin'})
RETURN valueType(a) AS nodeType,
       valueType(r) AS relationshipType,
       valueType(p) AS pathType,
       length(p) AS hops,
       p = [a,r,b] AS samePath
```

该例在空图中创建两个 Node 和一个 Relationship，返回 `NODE NOT NULL`、`RELATIONSHIP NOT NULL`、`PATH NOT NULL`、`1`、`true`。Path 与按相同遍历次序交替排列的 Node/Relationship List 可以做相等比较。

Node/Relationship 的相等比较基于 identity，不是“Label 和 Property 恰好相同”。`elementId()` 返回 Lithograph 数据库内的稳定字符串，Node 形式为 `n:<id>`，Relationship 为 `r:<id>`；同一历史元素在 Branch/Commit 中保持 identity。业务上定位实体时通常使用业务键，Snapshot 中不存在的元素不能因为知道旧 ID 而重新出现。

图元素和 Path 可以出现在查询结果以及查询内部的 List/Map 中，不能作为参数传回，包括把 Node 放在嵌套参数容器里的情况。传业务键或 `elementId()` 字符串后重新 MATCH。关于当前图、历史 Snapshot 和可见性见 [版本指南](../guide/versioning.md) 与 [Graph View](../guide/graph-views.md)。

<a id="persistent-properties"></a>
<a id="property-types"></a>

## 可持久化 Property

Node 与 Relationship 遵循相同规则：Property 可以保存表中的单值 Property 类型，或保存**同一种标量类型、无 null 元素的一维 List**。空 List 合法；Map、Node、Relationship、Path、嵌套 List、含 Vector 的 List 不合法。Integer 和 Float 即使数值相等，也不算同一种 Property List 元素类型。

| 值 | 查询运行时合法 | Property 合法 |
| --- | --- | --- |
| `[]`、`[1,2]`、`[date('2026-10-15')]` | 是 | 是 |
| `[1,1.0]`、`[1,'2']` | 是 | 否，混合元素类型 |
| `[1,null]` | 是 | 否，含 null |
| `[[1,2]]`、`[{x:1}]` | 是 | 否，嵌套容器 |
| `[vector([1,2],2,FLOAT32)]` | 是 | 否，Vector List |
| `{name:'Ada'}` | 是 | 否，Map |

`SET n.key = null` 删除该 Property；创建时值为 null 也不建立一个 null slot。读取缺失属性仍返回 null，用 `keys(n)` 可观察该 key 是否存在。

<!-- example: foundation-values-property -->
```cypher
CREATE (n:TypeDemo {items:[1,2],empty:[],day:date('2026-10-15')})
SET n.optional = null
RETURN n.items AS items, n.empty AS empty,
       n.optional IS NULL AS absent,
       'optional' IN keys(n) AS stored
```

结果为 `[[1,2],[],true,false]`。Property Type Constraint 和 Graph Type 可以进一步限制 Property 类型与存在性，见 [Constraint](constraints.md) 与 [Graph Type](graph-types.md)。

以下三个例子分别因混合数字类型、null 元素、Map Property 返回 `TYPE_ERROR`；失败 execution 不保留本次创建的 Node。

<!-- example: foundation-values-property-mixed-error -->
```cypher
CREATE (:TypeDemo {bad:[1,1.0]})
```

<!-- example: foundation-values-property-null-error -->
```cypher
CREATE (:TypeDemo {bad:[1,null]})
```

<!-- example: foundation-values-property-map-error -->
```cypher
CREATE (:TypeDemo {bad:{nested:1}})
```

<a id="temporal-values"></a>

## Temporal 与 Duration

Temporal 值使用独立类型，不能把日期字符串直接当成 Date。构造函数接受相应字符串、组件 Map 和文档规定的其它输入；完整 overload、组件访问、时钟、format/truncate 见 [Temporal 函数](functions/temporal.md)。

| 类型 | 含义 | 时间信息 |
| --- | --- | --- |
| `DATE` | 日历上的日期 | 年/月/日，无时钟与时区 |
| `LOCAL TIME` | 一天内的本地时钟 | 无时区，不能独立决定一个 UTC instant |
| `ZONED TIME` | 本地时钟和 offset | 有 UTC offset，无日期 |
| `LOCAL DATETIME` | 本地日期和时钟 | 无时区 |
| `ZONED DATETIME` | 日期、时钟和时区 | 保留 offset 和显式 IANA zone |
| `DURATION` | 月、日、秒、纳秒组成的时间量 | 不绑定到一个 instant |

<!-- example: foundation-values-temporal -->
```cypher
RETURN format(date('2026-10-15')) AS day,
       format(localtime('12:30:00.123')) AS localTime,
       format(time('12:30:00+08:00')) AS zonedTime,
       format(localdatetime('2026-10-15T12:30:00')) AS localDateTime,
       format(datetime('2026-10-15T12:30:00+08:00[Asia/Shanghai]')) AS zonedDateTime,
       format(duration('P1DT2H')) AS span
```

该例返回对应的规范文本；结果列都为 String，因为显式使用了 `format()`。直接返回 Temporal 值时，结果使用各自的 JSON tag，不能假定得到普通 String。

月份长度和夏令时会改变日历运算的结果。Duration 保留月份、天数和秒等组件，`P1M` 不等于固定的 `P30D`。比较两个 Duration 的 `<`、`<=`、`>`、`>=` 返回 null；要比较某个业务场景中的实际间隔，先明确参考日期/instant 和要比较的单位。

<!-- example: foundation-values-temporal-arithmetic -->
```cypher
RETURN format(date('2024-01-31') + duration('P1M')) AS monthEnd,
       duration('P1M') = duration('P30D') AS sameDuration,
       duration('P1M') < duration('P30D') AS ordered
```

结果为 `['2024-02-29',false,null]`。月份加法把不存在的月末日期调整至目标月份的最后一天。

带 IANA zone 的 datetime 根据该 instant 的 zone rules 校验显式 offset。夏令时跳跃产生的不存在时间、与 zone 不符的 offset 会失败；回拨重叠时间可以用显式 offset 指定是哪一次。下面 Paris 夏季时间的 offset 应为 `+02:00`，因此返回 `TYPE_ERROR`。

<!-- example: foundation-values-timezone-error -->
```cypher
RETURN datetime('2024-07-01T12:00:00+01:00[Europe/Paris]') AS invalid
```

不要用宿主进程的本地时区解释一个 Local DateTime，也不要在解码 Zoned DateTime 时丢弃 zone 字段。

<a id="point-values"></a>

## Point 与坐标系

Point 必须声明一致的坐标系统与维数。使用 `x/y[/z]` 构造 Cartesian，或使用 `longitude/latitude[/height]` 构造 WGS-84；可显式指定一个 `crs` 或 `srid`，不能同时指定两者，也不能混用两套坐标字段。

| CRS | SRID | 维数 | 常见构造 |
| --- | --- | --- | --- |
| `cartesian` | `7203` | 2 | `point({x:1,y:2})` |
| `cartesian-3d` | `9157` | 3 | `point({x:1,y:2,z:3})` |
| `wgs-84` | `4326` | 2 | `point({longitude:120,latitude:30})` |
| `wgs-84-3d` | `4979` | 3 | `point({longitude:120,latitude:30,height:10})` |

坐标必须有限；latitude 范围为 `[-90,90]`，longitude 规范化至地理经度范围。`p.x/p.y/p.z` 和地理别名按实际 CRS/维数访问；不存在的第三维或不适用的地理组件会报错。缺失/冲突字段、无效 CRS、非法维数返回 `TYPE_ERROR`。

<!-- example: foundation-values-point -->
```cypher
WITH point({longitude:181,latitude:30}) AS p
RETURN p.longitude AS longitude, p.latitude AS latitude, p.srid AS srid,
       point.distance(point({x:0,y:0}),point({x:3,y:4})) AS distance
```

结果为 `[-179.0,30.0,4326,5.0]`。Cartesian distance 使用坐标单位；WGS-84 distance 使用米。Point 的 direct 大小比较返回 null，空间判断应使用 `point.distance()` / `point.withinBBox()`，见 [Spatial 函数](functions/spatial.md)。

<a id="vector-values"></a>

## Vector、坐标类型与维数

Vector 是具有固定坐标类型和维数的独立值。List `[1,2]` 与 Vector 不相等，也不能通过相同 JSON array 外形混为一谈。

```text
vector(coordinates, dimension, coordinateType)
VECTOR<coordinateType>(dimension)
```

第一个形式构造值，第二个用于类型表达式。`coordinates` 接受 numeric List，也接受逗号分隔的 numeric String，例如 `'[1,2]'`。坐标类型支持 `INTEGER8`、`INTEGER16`、`INTEGER32`、`INTEGER`/`INTEGER64`、`FLOAT32`、`FLOAT`/`FLOAT64`。构造器推荐使用这些不带引号的类型 keyword；JSON `coordinateType` 使用短名 `I8/I16/I32/I64/F32/F64`，不要把 JSON 短名直接当成 Cypher keyword。`valueType()` 使用规范类型名称。

维数必须为 `1..4096`，并与输入长度完全一致。坐标不得为 null、NaN 或 Infinity；整数坐标必须在对应宽度的范围内且没有小数部分。转换到 `FLOAT32` 会采用相应精度。Vector Property 保留原始坐标类型和维数。

<!-- example: foundation-values-vector -->
```cypher
WITH vector([1,2],2,INTEGER8) AS v
RETURN valueType(v) AS kind, vector_dimension_count(v) AS dimension,
       toIntegerList(v) AS coordinates,
       v = vector([1,2],2,INTEGER8) AS equal
```

结果为 `['VECTOR<INTEGER8 NOT NULL>(2) NOT NULL',2,[1,2],true]`。Vector equality 要求相同坐标类型及维数；否则返回 `TYPE_ERROR`。同 family direct 大小比较返回 null；`ORDER BY` 可以排序但不是相似度排名。距离、范数及转换见 [Vector 函数](functions/vector.md)，索引查询见 [SEARCH](search.md)。

<!-- example: foundation-values-vector-dimension-error -->
```cypher
RETURN vector([1,2],3,FLOAT64) AS invalid
```

输入长度与声明维数不一致，返回 `TYPE_ERROR`。

<a id="uuid-values"></a>

## UUID

`UUID` 保存 128-bit 值，区别于恰好包含 UUID 文本的 String。支持以下构造形式：

```text
uuid()
uuid(text)
uuid(mostSigBits, leastSigBits)
```

无参数形式生成随机 UUID；String 输入必须为 `8-4-4-4-12` 的 32 个十六进制数字形式，可接受大写输入并规范为小写。双 Integer 形式使用两个 signed 64-bit 的位模式，按 most/least significant 顺序组成 UUID。`uuid.mostSignificantBits()` / `uuid.leastSignificantBits()` 反向提取这两部分。

<!-- example: foundation-values-uuid -->
```cypher
RETURN uuid('550E8400-E29B-41D4-A716-446655440000') AS canonical,
       toString(uuid('550e8400-e29b-41d4-a716-446655440000')) AS text,
       uuid(0,1) = uuid('00000000-0000-0000-0000-000000000001') AS same,
       valueType(randomUUID()) AS randomStringType
```

首列为原生 UUID，宿主收到 `{"$type":"UUID","value":"550e8400-e29b-41d4-a716-446655440000"}`；`text` 列是显式转换后的同一小写文本，最后两列为 `true` 和 `STRING NOT NULL`。`randomUUID()` 返回 String，`uuid()` 返回 UUID，调用方应根据所需属性类型选择。UUID 的 equality 和 order 使用其 128-bit 值；不会在比较时自动解析 String。

<!-- example: foundation-values-uuid-error -->
```cypher
RETURN uuid('invalid') AS invalid
```

非法文本返回 `TYPE_ERROR`；传入 null 时构造结果为 null。UUID 可以独立保存为 Property，也可以组成同类、无 null 的 Property List。

<a id="type-expressions"></a>

## 类型判断与 nullability

```text
expression IS :: type
expression IS NOT :: type
type NOT NULL
LIST<type>
type | type
VECTOR<coordinateType>(dimension)
```

类型判断返回 Boolean；类型未写 `NOT NULL` 时允许 null。这与 `null = value` 返回 null 的规则不同。`NOT NULL` 可作用于容器本身或元素：`LIST<INTEGER NOT NULL>` 排除 null 元素，但 List 本身仍 nullable；`LIST<INTEGER NOT NULL> NOT NULL` 同时排除两者。Union 用 `|` 表示满足任一成员，空 List 满足每个元素都通过的 List 类型条件。

<!-- example: foundation-values-type-predicate -->
```cypher
RETURN null IS :: INTEGER AS nullable,
       null IS :: INTEGER NOT NULL AS required,
       [1,null] IS :: LIST<INTEGER> AS nullableElements,
       [1,null] IS :: LIST<INTEGER NOT NULL> AS requiredElements,
       'x' IS :: INTEGER | STRING AS unionType,
       vector([1,2],2,FLOAT32) IS :: VECTOR<FLOAT32>(2) AS shaped
```

结果为 `[true,false,true,false,true,true]`。类型判断检查值，不做转换；`1 IS :: FLOAT` 为 false。`ANY PROPERTY VALUE NOT NULL` 可检查一个非 null 值是否满足 Property 合法性；未附 `NOT NULL` 时还允许 null。Schema 中可用的 Property type 及约束语法见 [Constraint](constraints.md)，不应把允许 Map/Path 的运行时类型表达式直接当成合法 Property type。

常用规范名称包括 `BOOLEAN`、`INTEGER`、`FLOAT`、`STRING`、`LIST`、`MAP`、`NODE`、`RELATIONSHIP`、`PATH`、各 Temporal 类型、`DURATION`、`POINT`、`VECTOR`、`UUID`、`ANY VALUE` 和 `ANY PROPERTY VALUE`。规范化显示可能将别名改为上述名称。

<a id="explicit-conversion"></a>

## 显式转换与失败

`toBoolean()`、`toInteger()`、`toFloat()`、`toString()` 按其公开签名转换。字符串无法解析通常返回 null；输入类型不在该函数允许范围内通常返回 `TYPE_ERROR`。`...OrNull()` 形式把不支持的输入类型也变为 null。List 转换逐元素执行，保留 null，把无法转换的元素变为 null；这可能产生不能直接保存为 Property 的 List。

<!-- example: foundation-values-conversion -->
```cypher
RETURN toInteger('42') AS integer, toFloat('2.5') AS float,
       toBoolean('FALSE') AS boolean, toInteger('invalid') AS invalidText,
       toIntegerOrNull({x:1}) AS invalidType,
       toIntegerList(['3','bad',null]) AS convertedList
```

结果为 `[42,2.5,false,null,null,[3,null,null]]`。转换不会改变参数原值，也不会建立图 Property。需要保存转换结果时，先决定对 null 的业务处理方式，再执行写入。完整函数输入范围见 [Scalar 函数](functions/scalar.md)。

<a id="native-property-storage"></a>

## 持久化原生 Vector 与 UUID

构造器产生原生值，赋给 Property 后保留其类型和坐标宽度。下面第一条 execution 完成提交，再由第二条查询读取；大整数的 JSON wrapper 见 [Values](../reference/values.md)。

<!-- example: foundation-native-store-create -->
```cypher
CREATE (:NativeStored {
  embedding:vector([9007199254740993],1,INTEGER64),
  identity:uuid('550e8400-e29b-41d4-a716-446655440000')
})
RETURN count(*) AS created
```

返回 `[1]`。

<!-- example: foundation-native-store-read -->
```cypher
MATCH (n:NativeStored)
RETURN n.embedding AS embedding,n.identity AS identity
```

返回的 embedding 是 coordinateType 为 `I64`、dimension 为 1 的 tagged Vector，唯一坐标为准确的 `9007199254740993`；identity 是 UUID typed value，canonical String 为 `550e8400-e29b-41d4-a716-446655440000`。
