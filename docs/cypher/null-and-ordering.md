# null、相等、比较与排序

[语言手册](index.md) · [版本与范围](../reference/cypher-compatibility.md)

**适用版本：v0.3.3，Cypher Profile：`CY25-2026.08`。** 本章区分缺失值传播、相等比较、大小谓词与 `ORDER BY`。这些规则同样适用于表达式、过滤、聚合输入和索引访问；索引不会把查询本应报出的类型错误静默变成空结果。

<a id="null-values"></a>

## null 表示缺失或未知

`null` 不是 `0`、`false`、空字符串、空 List 或空 Map。读取不存在的 Property/Map key、某些越界访问、`OPTIONAL MATCH` 未匹配变量、失败但允许返回空值的转换，都可能得到 null。

大部分运算传播 null：`null + 1`、`null = null`、`null <> 1`、`null < 1` 的结果都是 null。检测缺失值使用 `IS NULL` 或 `IS NOT NULL`。给 Property 赋 null 表示移除该 Property，见 [Property 合法性](types.md#persistent-properties)。

<!-- example: foundation-values-null -->
```cypher
RETURN null = null AS equal, null <> 1 AS unequal,
       null IS NULL AS missing, coalesce(null,null,'fallback') AS chosen
```

结果为 `[null,null,true,'fallback']`。`coalesce()` 返回第一个非 null 值，不跳过 `0`、`false` 或空容器。`null IS :: INTEGER` 则为 true，因为该类型表达式允许 null；需要排除缺失值时使用 `INTEGER NOT NULL`，见 [类型判断](types.md#type-expressions)。

<a id="three-valued-logic"></a>

## Boolean 三值逻辑

`AND`、`OR`、`XOR`、`NOT` 在 true、false、null 上运算。null 表示当前信息不足以判断真假；当已知值足以决定答案时，可以得到确定结果。

| `a` | `b` | `a AND b` | `a OR b` | `a XOR b` |
| --- | --- | --- | --- | --- |
| true | true | true | true | false |
| true | false | false | true | true |
| true | null | null | true | null |
| false | true | false | true | true |
| false | false | false | false | false |
| false | null | false | null | null |
| null | true | null | true | null |
| null | false | false | null | null |
| null | null | null | null | null |

`NOT true` 为 false，`NOT false` 为 true，`NOT null` 为 null。非 Boolean 的非 null 输入不是隐式真假值，会返回 `TYPE_ERROR`。三值逻辑规定结果，不承诺可用左侧条件屏蔽右侧任意非法表达式。

<!-- example: foundation-values-logic -->
```cypher
RETURN true AND null AS andUnknown, false AND null AS andFalse,
       true OR null AS orTrue, false OR null AS orUnknown,
       true XOR null AS xorUnknown, NOT null AS negated
```

结果为 `[null,false,true,null,null,null]`。

`WHERE`、`FILTER`、comprehension 的筛选条件和 `CASE WHEN` 只把 true 当作满足条件；false 和 null 都不保留该行或不进入该分支。需要保留未知值时明确写 `predicate OR predicate IS NULL`，或根据业务选择 `coalesce(predicate,true)`。

<!-- example: foundation-values-filter-null -->
```cypher
UNWIND [true,false,null] AS keep
WITH keep WHERE keep
RETURN keep
```

结果只有一行 `[true]`。`OPTIONAL MATCH` 中 WHERE 的位置还会决定是在匹配内部还是后续行流中过滤，见 [读取子句](reading.md)。

<a id="equality"></a>

## 相等与不等：`=`、`<>`

| 输入 | 结果规则 |
| --- | --- |
| 任意一侧为 null | 返回 null |
| Integer 与 Float | 按数值比较，保留大整数精度 |
| 两个 String | 按 Unicode 文本内容比较，区分大小写 |
| 两个 Boolean | true 只等于 true，false 只等于 false |
| 两个 Node / 两个 Relationship | 按 element identity 比较 |
| 两个 Path | 按遍历次序的 Node 与 Relationship identity 比较 |
| Path 与交替 Node/Relationship List | 可比较等价路径序列 |
| 两个 List / 两个 Map | 按容器结构与对应值递归比较，保留 null 的未知结果 |
| 两个 Temporal / Duration | 按对应类型的值及组件比较 |
| 两个 Point | CRS 与坐标均相等时相等 |
| 两个 Vector | 要求坐标类型和维数相同，再比较坐标 |
| 两个 UUID | 比较 128-bit 值 |
| 其余不同 value family | 返回 `TYPE_ERROR` |

`<>` 反转 `=` 的 true/false，null 仍为 null。`'1' = 1` 不会尝试解析 String，也不会简单返回 false。该规则使用冻结 Profile 的当前语义，不能套用旧 openCypher 场景中的跨类型 equality 规则。

<!-- example: foundation-values-equality -->
```cypher
RETURN 9007199254740993 = 9007199254740992.0 AS precise,
       'a' = 'a' AS same, false < true AS booleanOrder
```

结果为 `[false,true,true]`。左侧整数比右侧可精确表达的 Float 大 `1`，比较不会先把 Integer 转为舍入后的 Float。

<!-- example: foundation-values-cross-type-error -->
```cypher
RETURN '1' = 1 AS invalid
```

返回 `TYPE_ERROR`。Vector 虽属于同一 family，坐标类型或维数不同也不可直接做 equality：

<!-- example: foundation-values-vector-equality-error -->
```cypher
RETURN vector([1],1,INTEGER8) = vector([1],1,INTEGER16) AS invalid
```

该例返回 `TYPE_ERROR`；需要转换时先显式构造相同类型与维数的 Vector。

<a id="container-equality-and-in"></a>

## 容器、null 与 `IN`

List equality 比较长度和对应位置；长度不同就为 false。Map equality 比较 key 集合和对应值，key 不同就为 false。在结构相同时，存在明确不等的对应值可确定 false；否则若某项比较结果为 null，容器 equality 也为 null。由非法跨类型项产生的 `TYPE_ERROR` 仍是错误。

`x IN list` 判断是否存在与 `x` 相等的项。存在明确匹配时为 true；没有匹配但含有导致比较为 null 的项时为 null；没有匹配和未知项时为 false。空 List 中不存在任何元素，因此 `null IN []` 也为 false。`NOT IN` 保留三值逻辑，不能自动把未知项当作不匹配。

<!-- example: foundation-values-containment -->
```cypher
RETURN 2 IN [1,null] AS unknown, 1 IN [1,null] AS found,
       null IN [] AS empty, [] = [] AS sameEmpty,
       [1,null] = [2,null] AS unequal,
       [1,null] = [1,null] AS unknownList
```

结果为 `[null,true,false,true,false,null]`。如果业务需要把未知成员关系视为 false，可显式使用 `coalesce(x IN list,false)`；这是一项业务选择，不是 `IN` 的默认语义。

聚合函数通常忽略 null，`count(*)` 计行而 `count(expr)` 只计非 null；空输入结果与 DISTINCT 分组规则见 [聚合](aggregation.md)，不能从 `null = null` 推导出分组会为每个 null 新建一组。

<a id="direct-comparison"></a>

## 大小谓词：`<`、`<=`、`>`、`>=`

任意一侧为 null 时结果为 null。同 family 使用下表规则：

| family | direct comparison |
| --- | --- |
| Number | 按数值，NaN 的特殊规则见下文 |
| String | 按 Unicode 文本顺序，大小写有区别 |
| Boolean | `false < true` |
| Date / Local Time / Local DateTime | 按日期或本地时钟的组件顺序 |
| Zoned Time | 先比较对应 UTC 时间，再按 offset 区分相同 instant |
| Zoned DateTime | 先比较 instant，再按 offset/zone 区分相同 instant |
| UUID | 按 128-bit 值顺序 |
| List | 按对应元素的字典序；先遇到不同元素决定结果，相同前缀时较短者在前；遇到待比较的 null 元素可返回 null |
| Point / Vector / Duration | 返回 null，包括相等值的 `<=` 或 `>=` |
| Map / Node / Relationship / Path | 使用下述可排序值规则 |

不同非 null family 的大小谓词使用以下 hierarchy，Integer/Float 共同属于 Number：

```text
MAP < NODE < RELATIONSHIP < LIST < PATH < VECTOR < POINT
    < ZONED DATETIME < LOCAL DATETIME < DATE
    < ZONED TIME < LOCAL TIME < DURATION
    < STRING < BOOLEAN < UUID < NUMBER
```

这解释了 `'z' < false` 为 true；它不表示 String 已转换为 Boolean。不同 family 在 `=` 上可能报错，在 `<` 上却有 hierarchy 顺序，两个运算符不能互相替代。

<!-- example: foundation-values-comparison -->
```cypher
RETURN 'z' < false AS familyOrder, [1,2] < [1,3] AS listOrder,
       [1,null] < [1,2] AS unknownListOrder,
       point({x:0,y:0}) < point({x:1,y:1}) AS pointOrder
```

结果为 `[true,true,null,null]`。Point 不应用大小谓词表达“位于范围内”；应使用空间函数。Duration 大小需要业务参考时点；Vector 相似度需要距离/相似度函数。

<a id="order-by"></a>

## `ORDER BY` 的完整排序

`ORDER BY` 必须能够对异构结果排序，因此使用 total ordering。跨 family 顺序与上面的 hierarchy 一致，并将 null 放在所有非 null 值之后。`ASC` 默认升序，null 在末尾；`DESC` 反转顺序，null 在开头。

<!-- example: foundation-values-order -->
```cypher
UNWIND [null,2,true,'b',[],{a:1},1.5] AS value
RETURN value ORDER BY value
```

按顺序返回 `{a:1}`、`[]`、`'b'`、`true`、`1.5`、`2`、`null`。没有 `ORDER BY` 时，不应依赖扫描或 UNWIND 的偶然输出顺序。需要稳定分页或稳定展示时，在业务排序列后再加一个能消除并列的键。

同 family 的排序规则如下：

| family | `ORDER BY` 的比较键 |
| --- | --- |
| Map | 先比较项数，再比较排好序的 key 序列，再比较对应值 |
| Node / Relationship | element identity；应用展示排序应显式使用业务 Property |
| List | 逐项按 total ordering 比较，相同前缀时较短 List 在前；null 元素也有确定顺序 |
| Path | 按交替 Node/Relationship identity 的遍历序列排序，相同前缀时较短者在前 |
| Vector | 先坐标类型 `I8 < I16 < I32 < I64 < F32 < F64`，再维数，再坐标序列 |
| Point | 先 SRID，再坐标序列 |
| Duration | 使用平均月长度折算的内部排序值；不能把该顺序解释为特定日期上的实际时间跨度 |
| Temporal / String / Boolean / UUID / Number | 使用对应类型的确定顺序；Number 中 NaN 排在所有其它数字之后 |

**可以排序不代表可直接比较大小。** 两个 Point、Vector 或 Duration 的 `ORDER BY` 有顺序，它们的 direct inequality 仍为 null；List 的 direct comparison 也保留 null 的未知结果，而 List sorting 能为 null 元素确定顺序。

<a id="nan-and-infinity"></a>

## NaN 与 Infinity

NaN 是 Float 值，不是 null。数学函数在没有实数结果时可能产生 NaN，例如 `sqrt(-1.0)`；判断使用 `isNaN()`。

<!-- example: foundation-values-nan -->
```cypher
WITH sqrt(-1.0) AS nan
RETURN isNaN(nan) AS detected, nan = nan AS equal,
       nan <> nan AS unequal, nan < 1 AS less,
       nan >= 1 AS greaterOrEqual, nan IS NULL AS missing
```

结果为 `[true,false,true,false,false,false]`。任何涉及 NaN 的数值 `<`、`<=`、`>`、`>=` 都为 false；NaN 不与自身相等。不要使用 `x <> x` 代替所有输入类型的 NaN 检查。

正负 Infinity 按普通数值方向排序：`-Infinity < finite number < Infinity < NaN < null`。最后两个值的顺序是 `ORDER BY` 规则，不是 NaN 大小谓词的结果。

<!-- example: foundation-values-nan-order -->
```cypher
UNWIND [sqrt(-1.0),null,1,toFloat('Infinity'),toFloat('-Infinity')] AS value
RETURN CASE WHEN value IS NULL THEN 'null'
            WHEN isNaN(value) THEN 'NaN'
            ELSE toString(value) END AS kind
ORDER BY value
```

结果依次为 `'-inf'`、`'1'`、`'inf'`、`'NaN'`、`'null'`。这里把值转为 String 是为了直观显示顺序；直接返回非有限 Float 使用 [Float JSON tag](../reference/values.md)，不是标准 JSON number。Point 与 Vector 的坐标拒绝 NaN/Infinity。

<a id="predicate-null"></a>

## String predicate 与其它空值边界

`STARTS WITH`、`ENDS WITH`、`CONTAINS`、regex 等 String predicate 对 null 或非 String 输入返回 null，不把非 String 强制格式化，也不将其变成一个简单 false。

<!-- example: foundation-values-string-null -->
```cypher
RETURN 7 STARTS WITH '7' AS stringPredicate,
       null STARTS WITH 'x' AS nullPredicate
```

结果为 `[null,null]`。在 `WHERE` 中这两个结果都会被过滤掉。如果要接受数字字符串表达形式，先显式选择合适的 `toString()` 转换。

参数 Map 中省略参数、显式传 `{"x":null}` 和查询中访问 `m.x` 缺失项有不同边界：缺少必需参数是 `INVALID_ARGUMENT`，已提供的 null 则按 Cypher 空值规则计算。参数绑定见 [SQL API](../reference/sql-api.md)。表达式和函数各自可能有特别的 null 规则，完整说明见 [表达式](expressions.md) 与 [函数索引](../reference/functions.md)。
