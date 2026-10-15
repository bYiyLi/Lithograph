# 聚合、DISTINCT 与 GROUP BY

[语言手册](index.md) · [版本与范围](../reference/cypher-compatibility.md)

本文适用于 Lithograph v0.3.1、`CY25-2026.08`。聚合把一组输入行计算成一条输出行；分组键决定哪些行属于同组。`RETURN` 和 `WITH` 都可以形成聚合边界。完整函数说明见 [聚合函数](functions/aggregation.md)。

<a id="implicit-grouping"></a>

## 隐式分组

```text
RETURN <grouping-expression> [AS <alias>],
       <aggregate-expression> [AS <alias>], ...
WITH   <grouping-expression> [AS <alias>],
       <aggregate-expression> [AS <alias>], ...
```

投影含有聚合时，独立的非聚合投影项形成隐式分组键。全部分组键值相等的行进入同组；null 分组键也构成一个组。没有分组键时，全部输入形成全局组。结果行没有隐含顺序，需要 ORDER BY。

<!-- example: foundation-aggregation-implicit -->
```cypher
UNWIND [{group:'甲',amount:3},{group:'乙',amount:4},{group:'甲',amount:5}] AS item
RETURN item.group AS category, count(*) AS total, sum(item.amount) AS amount
ORDER BY category
```

返回 `["乙",1,4]`、`["甲",2,8]`，按 Unicode 字符串顺序排列。不能在一个含聚合的复杂表达式中隐含引用没有合法分组依据的输入变量；复杂计算先用 WITH 得到命名分组键，或使用明确 GROUP BY。

<a id="group-by"></a>

## 显式 GROUP BY

```text
RETURN <projection-items> GROUP BY <expression-or-projected-alias>, ...
RETURN <projection-items> GROUP BY ALL
RETURN <projection-items> GROUP BY ()
```

WITH 使用相同的 GROUP BY 形式。GROUP BY 位于投影项之后、ORDER BY 之前。

| 形式 | 用途 |
| --- | --- |
| `GROUP BY key, ...` | 明确列出分组表达式，可使用对应投影别名；非聚合输出必须与合法分组项一致 |
| `GROUP BY ALL` | 将非聚合投影项作为分组键；无 aggregate 时相当于按投影行分组去重 |
| `GROUP BY ()` | 空分组键，形成全局组；输出可含 aggregate 及合法常量，不能夹带逐行值 |

<!-- example: foundation-aggregation-explicit -->
```cypher
UNWIND [1, 2, 3, 4] AS x
RETURN x % 2 AS parity, count(*) AS total, sum(x) AS amount
GROUP BY parity
ORDER BY parity
```

返回 `[0,2,6]`、`[1,2,4]`。把 `GROUP BY parity` 换为 `GROUP BY ALL`，这个例子结果相同。

<!-- example: foundation-aggregation-global -->
```cypher
UNWIND [1, 2] AS x
RETURN '所有' AS category, count(*) AS total
GROUP BY ()
```

返回 `["所有",2]`。

<!-- example: foundation-aggregation-group-error -->
```cypher
UNWIND [1, 2] AS x
RETURN x, count(*) AS total GROUP BY ()
```

返回 `SEMANTIC_ERROR`，因为逐行变量 x 不在全局组的分组键中。分组表达式、投影表达式和不确定函数的合法性在 prepare 时检查，不能依靠空数据绕过。

<a id="distinct"></a>

## 两种 DISTINCT 的不同作用

```text
RETURN DISTINCT <expression>, ...
RETURN <aggregate>(DISTINCT <expression>), ...
```

投影 DISTINCT 对最终整行去重；aggregate 内 DISTINCT 在各组内先去重该函数的输入，再聚合。一个 aggregate 使用 DISTINCT 不改变旁边其它 aggregate 的输入。`count(*)` 统计所有行，`count(value)` 只统计非 null；`count(DISTINCT value)` 统计不同的非 null 值。`count(DISTINCT *)` 不是本手册支持形式。

<!-- example: foundation-aggregation-distinct -->
```cypher
UNWIND [1, 1, 2, null] AS x
RETURN count(*) AS rows, count(x) AS present,
       count(DISTINCT x) AS uniqueValues, sum(x) AS amount,
       sum(DISTINCT x) AS uniqueAmount
```

返回 `[4,3,2,4,3]`。null 的聚合处理见下一节。

<!-- example: foundation-aggregation-distinct-star-error -->
```cypher
UNWIND [1, 1, 2] AS value
RETURN count(DISTINCT *) AS total
```

返回 `SEMANTIC_ERROR`；要统计不同的值，写 `count(DISTINCT value)`。

<a id="null-and-functions"></a>

## 聚合函数与 null

| 函数族 | 输入与结果 |
| --- | --- |
| `count(*)` / `count(expr)` | Integer 行数 / 非 null 值数 |
| `collect(expr)` / `collect_list(expr)` | 忽略 null，收集其它运行时值为 List |
| `sum(expr)` / `avg(expr)` | 数值或合法的 Duration 聚合；忽略 null，不把缺失当 0 输入 |
| `min(expr)` / `max(expr)` | 忽略 null，返回最小/最大原值；类型顺序遵循冻结 Profile |
| `stDev(expr)` / `stdev_samp(expr)` | 样本标准差 |
| `stDevP(expr)` / `stdev_pop(expr)` | 总体标准差 |
| `percentileCont(expr,p)` / `percentile_cont(expr,p)` | 连续百分位，必要时插值 |
| `percentileDisc(expr,p)` / `percentile_disc(expr,p)` | 离散百分位，从输入中取值 |

percentile 的 p 必须在 `0..1` 范围内且满足该函数的组内参数要求。输入类型错误、嵌套 aggregate、参数数量不正确或放在 WHERE/LET 等不允许 aggregation 的位置，会产生语义/类型错误。`sum(count(*))` 不能在一个聚合层级中计算；用 `WITH ... count(*) AS c` 再在下一投影做 `sum(c)`。表达式子查询 COUNT/COLLECT 的作用域独立，见 [子查询](subqueries.md)。

<a id="empty-input"></a>

## 空输入与全部 null

无分组键的全局聚合对零行输入仍返回一行：`count` 为 0、`sum` 为 0、`collect` 为 `[]`、`avg/min/max/percentile/stDev/stDevP` 为 null。有实际分组键而输入为零行时没有可形成的组，因此返回零行。

<!-- example: foundation-aggregation-empty -->
```cypher
UNWIND [] AS x
RETURN count(*) AS rows, count(x) AS present, sum(x) AS amount,
       collect(x) AS values, avg(x) AS average, min(x) AS smallest,
       max(x) AS largest, stDev(x) AS deviation
```

返回 `[0,0,0,[],null,null,null,null]`。

<!-- example: foundation-aggregation-empty-grouped -->
```cypher
UNWIND [] AS x
RETURN x AS key, count(*) AS total
```

返回零行。输入并非零行、但所有 expr 都是 null 时，`count(*)` 仍统计行数，而忽略 null 的 aggregate 按自己的空值结果处理。

<!-- example: foundation-aggregation-null -->
```cypher
UNWIND [null, null] AS x
RETURN count(*) AS rows, count(x) AS present,
       sum(x) AS amount, collect(x) AS values, avg(x) AS average
```

返回 `[2,0,0,[],null]`。OPTIONAL MATCH 保留的一条 null 扩展行尤其容易让 `count(*)` 与 `count(relationship)` 不同。

<a id="composition"></a>

## WITH、过滤、排序与 collect 顺序

WITH 可以先聚合，再由 WHERE 过滤分组结果，然后将这些结果用于后续 MATCH 或更高一级聚合。RETURN 后不加独立 WHERE；需要过滤 aggregate 时使用 WITH ... WHERE ... RETURN。

<!-- example: foundation-aggregation-filter -->
```cypher
UNWIND [1, 1, 2] AS value
WITH value, count(*) AS occurrences
WHERE occurrences > 1
RETURN value, occurrences ORDER BY value
```

返回 `[1,2]`。聚合形成作用域边界后，ORDER BY 不能随意引用已丢失的逐行变量，只能使用合法输出/分组/聚合表达式。

collect 的 List 次序取决于进入聚合的行序；未明确 ORDER BY 时不要假设稳定。需要排序后的 List 时，在聚合前的 WITH 明确排序。

<!-- example: foundation-aggregation-collect-order -->
```cypher
UNWIND [3, 1, 2] AS value
WITH value ORDER BY value
RETURN collect(value) AS ordered
```

返回 `[[1,2,3]]`。分组内排序与最终分组行排序是两个步骤；如同时需要，应分别在 aggregate 前和最终 RETURN 中声明。

相关主题：[读取与投影](reading.md) · [查询模型](query-model.md) · [null 与排序](null-and-ordering.md) · [聚合函数](functions/aggregation.md)。
