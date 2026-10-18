# 聚合函数

适用版本：**Lithograph v0.3.2** · Cypher Profile：`CY25-2026.08`。本文覆盖全部 Aggregating 函数及其 Cypher 25 命名别名。

[Cypher 手册](../index.md) · [完整函数索引](../../reference/functions.md) · [分组、DISTINCT 与作用域](../aggregation.md) · [null 与值排序](../null-and-ordering.md)

<a id="aggregate-rules"></a>

## 分组、null 与输入类型

聚合函数在 `RETURN` 或 `WITH` 中将每个分组的输入行归约为一个值。没有分组键的聚合在零行输入时仍产生一行；带分组键的零行输入不产生任何分组。`count(*)` 数输入行，其他条目先忽略表达式为 `null` 的行，因此缺失属性不会作为数值 0 参与平均值。

`fn(DISTINCT expression)` 在当前分组、当前调用中去重，然后聚合；它不同于对最终输出行使用 `RETURN DISTINCT`。只有 `count(*)` 使用星号语法。对聚合结果再次聚合应先经 `WITH` 形成新的输入阶段，不在同一聚合参数内嵌套聚合函数。函数与非聚合表达式混用的分组规则见 [聚合](../aggregation.md)。

本文签名中的 `FLOAT` 数值输入也接受 `INTEGER`。聚合函数不会把数字字符串或 Boolean 转成数值；数值、Duration 或 percentile 参数不符合条目要求时产生 `TYPE_ERROR`。参数个数不符产生 `SEMANTIC_ERROR`。所有签名参数都必须提供；百分位函数的第二参数 `percentile` 没有默认值。错误码识别见 [错误参考](../../reference/errors.md)。

| 目标 | 函数 |
| --- | --- |
| 计数与收集 | [count](#fn-count)、[collect](#fn-collect)、[collect_list](#fn-collect_list) |
| 求和与平均 | [sum](#fn-sum)、[avg](#fn-avg) |
| 极值 | [min](#fn-min)、[max](#fn-max) |
| 百分位 | [percentileCont](#fn-percentilecont)、[percentile_cont](#fn-percentile_cont)、[percentileDisc](#fn-percentiledisc)、[percentile_disc](#fn-percentile_disc) |
| 样本标准差 | [stDev](#fn-stdev)、[stdev_samp](#fn-stdev_samp) |
| 总体标准差 | [stDevP](#fn-stdevp)、[stdev_pop](#fn-stdev_pop) |

所有查询都在已加载并初始化的空数据库中独立运行，不依赖共享图数据。

<a id="fn-count"></a>

## `count()`

<!-- function-signatures: count -->
```text
count(input :: ANY) :: INTEGER
```
<!-- /function-signatures -->

返回 `INTEGER` 计数。`count(input)` 数表达式为非 null 的行；`count(DISTINCT input)` 数不同的非 null 值；`count(*)` 数全部输入行，包括只有 null 的行。空输入/全 null 表达式返回 `0`。`input` 可为任意 runtime value，不会先展开 List。下面输入 4 行，非 null 值有 3 个、不同值有 2 个。

<!-- example: fn-count -->
```cypher
UNWIND [7,7,8,null] AS v RETURN count(*) AS total, count(v) AS present, count(DISTINCT v) AS unique
```

预期列：`total`, `present`, `unique`；预期行：`[4, 3, 2]`。

<a id="fn-collect"></a>

## `collect()`

<!-- function-signatures: collect -->
```text
collect(input :: ANY) :: LIST<ANY>
```
<!-- /function-signatures -->

将 `input` 的所有非 null 值收集为 `LIST<ANY>`，保留重复值。空输入或全 null 输入返回 `[]`；可以收集 List/Map/图元素，List 值作为一个元素，不自动 flatten。需要确定列表顺序时，在前一 `WITH ... ORDER BY` 中明确排序；无 ORDER BY 不应依赖扫描顺序。`collect(DISTINCT input)` 仅在本次聚合内去重。

<!-- example: fn-collect -->
```cypher
UNWIND [9,null,3,9] AS v WITH v ORDER BY v RETURN collect(v) AS x
```

预期列：`x`；预期行：`[[3, 9, 9]]`。

<a id="fn-collect_list"></a>

## `collect_list()`

<!-- function-signatures: collect_list -->
```text
collect_list(input :: ANY) :: LIST<ANY>
```
<!-- /function-signatures -->

`collect()` 的 Cypher 25 命名别名。每个非 null 的 `input` 作为一个 List 元素，返回 `LIST<ANY>`；保留重复，空输入/全 null 返回 `[]`。支持 `DISTINCT`；需要稳定元素顺序时先用 `WITH ... ORDER BY`。它不会展开嵌套 List。

<!-- example: fn-collect_list -->
```cypher
UNWIND [4,2,4,null] AS v WITH v ORDER BY v RETURN collect_list(DISTINCT v) AS x
```

预期列：`x`；预期行：`[[2, 4]]`。

<a id="fn-sum"></a>

## `sum()`

<!-- function-signatures: sum -->
```text
sum(input :: INTEGER | FLOAT | DURATION) :: INTEGER | FLOAT | DURATION
```
<!-- /function-signatures -->

对同一分组的 `INTEGER | FLOAT | DURATION` 非 null 值求和。全整数输入返回 `INTEGER`；含浮点输入返回 `FLOAT`；全 Duration 返回 `DURATION`，按月、日、秒和纳秒分量相加并规范化。不能混合 Duration 与数值；其他类型产生 `TYPE_ERROR`。整数总和或 Duration 分量溢出产生 `TYPE_ERROR`。空输入/全 null 返回整数 `0`；如需不同的空值策略，用 CASE 显式表达。

<!-- example: fn-sum -->
```cypher
UNWIND [5,8,null] AS v RETURN sum(v) AS x
```

预期列：`x`；预期行：`[13]`。

<a id="fn-avg"></a>

## `avg()`

<!-- function-signatures: avg -->
```text
avg(input :: INTEGER | FLOAT | DURATION) :: INTEGER | FLOAT | DURATION
```
<!-- /function-signatures -->

对非 null 的数值或 Duration 求算术平均。数值输入返回 `FLOAT`（即使输入全为 INTEGER）；全 Duration 输入返回 `DURATION`。Duration 的月/日/秒分别处理，平均后不足一个月的部分按平均 Gregorian 月换算到较小分量，不等同于给日期依次加值。空输入/全 null 返回 `null`；数字字符串、非数值、Duration/数值混合和 Duration 溢出产生 `TYPE_ERROR`。

<!-- example: fn-avg -->
```cypher
UNWIND [4,8,null] AS v RETURN avg(v) AS x
```

预期列：`x`；预期行：`[6.0]`。

<a id="fn-min"></a>

## `min()`

<!-- function-signatures: min -->
```text
min(input :: ANY) :: ANY
```
<!-- /function-signatures -->

返回当前组中最小的非 null `input`，保留被选中值本身的类型。空输入/全 null 返回 `null`。它使用 Cypher 排序语义，可比较不同 value family，List 按元素字典顺序比较；混合类型的最小值不能只凭字符串显示判断。详见 [null 与值排序](../null-and-ordering.md)。这与某些 direct comparison 返回 null 的情形不同。

<!-- example: fn-min -->
```cypher
UNWIND [8,2,5,null] AS v RETURN min(v) AS x
```

预期列：`x`；预期行：`[2]`。

<a id="fn-max"></a>

## `max()`

<!-- function-signatures: max -->
```text
max(input :: ANY) :: ANY
```
<!-- /function-signatures -->

返回当前组中最大的非 null `input`，保留该值的类型。空输入/全 null 返回 `null`。支持 Cypher value ordering 所定义的同类/跨类型排序，包括按字典顺序的 List；不要把数值和文本混放后期望进行数字转换。排序细节见 [null 与值排序](../null-and-ordering.md)。

<!-- example: fn-max -->
```cypher
UNWIND [8,2,5,null] AS v RETURN max(v) AS x
```

预期列：`x`；预期行：`[8]`。

<a id="fn-percentilecont"></a>

## `percentileCont()`

<!-- function-signatures: percentileCont -->
```text
percentileCont(input :: FLOAT, percentile :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

对非 null 数值排序后进行线性插值，返回 `FLOAT`，可能不等于任何输入值。第二参数 `percentile` 为 `[0.0,1.0]` 内的数值，`0` 取最小位置、`1` 取最大位置。对 n 个数的有序序列，位置为 `percentile*(n-1)`，在相邻位置间插值。用字面量或查询参数传入统一 percentile，让每个分组的含义明确。空输入返回 `null`；非数值输入，或 null、NaN、范围外 percentile 产生 `TYPE_ERROR`，空组也必须提供合法 percentile。

<!-- example: fn-percentilecont -->
```cypher
UNWIND [0,10,20,30] AS v RETURN percentileCont(v,0.25) AS x
```

预期列：`x`；预期行：`[7.5]`。

<a id="fn-percentile_cont"></a>

## `percentile_cont()`

<!-- function-signatures: percentile_cont -->
```text
percentile_cont(input :: FLOAT, percentile :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

`percentileCont()` 的 Cypher 25 命名别名。对非 null 的数值排序，并按 `percentile*(n-1)` 在线性插值位置返回 `FLOAT`。`percentile` 必须是 `[0,1]` 数值，没有默认值；建议使用字面量或查询参数。空输入返回 `null`；input 类型错误、null/NaN/越界 percentile 产生 `TYPE_ERROR`。插值结果可能不是输入集合成员。

<!-- example: fn-percentile_cont -->
```cypher
UNWIND [10,20,30,40] AS v RETURN percentile_cont(v,0.5) AS x
```

预期列：`x`；预期行：`[25.0]`。

<a id="fn-percentiledisc"></a>

## `percentileDisc()`

<!-- function-signatures: percentileDisc -->
```text
percentileDisc(input :: INTEGER | FLOAT, percentile :: FLOAT) :: INTEGER | FLOAT
```
<!-- /function-signatures -->

对非 null 数值排序后选取离散百分位，返回某个实际输入值并保留其 `INTEGER` 或 `FLOAT` 类型，不插值。`percentile` 必须在 `[0,1]`，按向上取整的排名选择；`0` 仍选最小值。使用字面量或查询参数传入统一 percentile。空输入返回 `null`；input 非数值，或 percentile 为 null、NaN、非数值或越界时产生 `TYPE_ERROR`，即使组为空也检查 percentile。

<!-- example: fn-percentiledisc -->
```cypher
UNWIND [0,10,20,30] AS v RETURN percentileDisc(v,0.25) AS x
```

预期列：`x`；预期行：`[0]`。

<a id="fn-percentile_disc"></a>

## `percentile_disc()`

<!-- function-signatures: percentile_disc -->
```text
percentile_disc(input :: INTEGER | FLOAT, percentile :: FLOAT) :: INTEGER | FLOAT
```
<!-- /function-signatures -->

`percentileDisc()` 的 Cypher 25 命名别名。排序后选择离散排名的实际输入成员，结果保持该成员的 `INTEGER` 或 `FLOAT` 类型。`percentile` 必须是 `[0,1]` 的数值，没有默认值，建议传统一字面量/参数。null input 被忽略，空输入返回 `null`；无效 input 或 null/NaN/越界 percentile 产生 `TYPE_ERROR`。与 continuous 形式的关键区别是不插值。

<!-- example: fn-percentile_disc -->
```cypher
UNWIND [10,20,30,40] AS v RETURN percentile_disc(v,0.5) AS x
```

预期列：`x`；预期行：`[20]`。

<a id="fn-stdev"></a>

## `stDev()`

<!-- function-signatures: stDev -->
```text
stDev(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

计算样本标准差，返回 `FLOAT`。对 n 个非 null 数值，使用 `sqrt(sum((v-mean)^2)/(n-1))`；当只有一个非 null 值时返回 `0.0`，空输入/全 null 返回 `null`。INTEGER 参与浮点计算；其它类型产生 `TYPE_ERROR`。它用于把当前组视为样本的场景，总体标准差改用 `stDevP()`。

<!-- example: fn-stdev -->
```cypher
UNWIND [2,4] AS v RETURN abs(stDev(v) - sqrt(2.0)) < 1e-12 AS x
```

预期列：`x`；预期行：`[true]`。

<a id="fn-stdev_samp"></a>

## `stdev_samp()`

<!-- function-signatures: stdev_samp -->
```text
stdev_samp(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

`stDev()` 的 Cypher 25 命名别名。对非 null 数值计算样本标准差，分母为 `n-1`，结果为 `FLOAT`；单个值返回 `0.0`，空输入/全 null 返回 `null`。INTEGER 被转换成浮点计算；非数值产生 `TYPE_ERROR`。它与 `stdev_pop()` 的总体分母不同。

<!-- example: fn-stdev_samp -->
```cypher
UNWIND [3,3,null] AS v RETURN stdev_samp(v) AS x
```

预期列：`x`；预期行：`[0.0]`。

<a id="fn-stdevp"></a>

## `stDevP()`

<!-- function-signatures: stDevP -->
```text
stDevP(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

计算总体标准差，返回 `FLOAT`。对 n 个非 null 数值，使用 `sqrt(sum((v-mean)^2)/n)`；单个非 null 值的结果为 `0.0`，空输入/全 null 返回 `null`。INTEGER 参与浮点计算；非数值产生 `TYPE_ERROR`。当当前组就是完整总体时使用它，抽样估计使用 `stDev()`。

<!-- example: fn-stdevp -->
```cypher
UNWIND [2,4] AS v RETURN stDevP(v) AS x
```

预期列：`x`；预期行：`[1.0]`。

<a id="fn-stdev_pop"></a>

## `stdev_pop()`

<!-- function-signatures: stdev_pop -->
```text
stdev_pop(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

`stDevP()` 的 Cypher 25 命名别名。对非 null 数值计算总体标准差，分母为 `n`，结果为 `FLOAT`；单个值返回 `0.0`，空输入/全 null 返回 `null`。非数值产生 `TYPE_ERROR`，INTEGER 可以参与浮点运算。

<!-- example: fn-stdev_pop -->
```cypher
UNWIND [5,5,null] AS v RETURN stdev_pop(v) AS x
```

预期列：`x`；预期行：`[0.0]`。

<a id="empty-aggregation"></a>

## 空输入与 Duration 示例

零行输入而且没有分组键时，返回一行聚合结果。`sum()` 的空值是 `0`，不能由这个结果推断输入里真的存在数值 0。百分位的配置仍须有效，例如 `percentileCont(v,2)` 即使在空输入上也会报 `TYPE_ERROR`。

<!-- example: fn-aggregation-empty -->
```cypher
UNWIND [] AS v RETURN count(*) AS n, collect(v) AS items, sum(v) AS total, avg(v) AS mean, min(v) AS low, max(v) AS high, percentileCont(v,0.5) AS median, stDev(v) AS sample, stDevP(v) AS population
```

预期列：`n`, `items`, `total`, `mean`, `low`, `high`, `median`, `sample`, `population`；预期行：`[0, [], 0, null, null, null, null, null, null]`。

Duration 的平均值仍是 Duration，不是秒数。若宿主需要字符串显示，使用 `toString()`；如需原始类型则直接返回聚合值并按 Values Reference 解码。

<!-- example: fn-aggregation-duration -->
```cypher
UNWIND [duration('PT2H'),duration('PT4H'),null] AS v RETURN toString(sum(v)) AS total, toString(avg(v)) AS mean
```

预期列：`total`, `mean`；预期行：`["PT6H", "PT3H"]`。

语义核对来源：[Neo4j Cypher 25 聚合函数](https://neo4j.com/docs/cypher-manual/25/functions/aggregating/)（2026-10-15 核对）。2026.05 及之后的标准差空值规则为 `null`，本 Profile 已包含该规则；不要沿用旧版本返回 `0` 的假设。
