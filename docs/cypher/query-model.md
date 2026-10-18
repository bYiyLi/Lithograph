# 查询行模型、作用域与执行边界

[语言手册](index.md) · [版本与范围](../reference/cypher-compatibility.md)

本文适用于 Lithograph v0.3.3、`CY25-2026.08`。理解一条查询时，把中间结果看作一张由变量组成的行表：每个 clause 接收输入行，读取当前图状态，生成下一组行及变量。图 Snapshot 与中间行是不同的概念；一条输入行可以匹配出多条结果，也可以在过滤后消失。

<a id="rows"></a>

## 起始行与结果基数

没有前序 clause 的查询从一条空行开始，因此 `RETURN 1 AS x` 返回一行。`MATCH` 对每条输入行查找全部合法绑定；没有匹配时该输入行消失。`OPTIONAL MATCH` 则为没有匹配的输入行保留一行，将新变量绑定为 null。

`UNWIND`/`FOR` 把一个值展开成零至多行。`FILTER`/`WHERE` 只保留谓词为 true 的行；false 和 null 都被过滤。`RETURN`/`WITH` 普通投影逐行求值，不自动去重；`DISTINCT` 与 aggregation 会改变基数。不同 clause 的具体规则见 [读取](reading.md) 与 [聚合](aggregation.md)。

<!-- example: foundation-model-duplicates -->
```cypher
UNWIND [1, 1, 2] AS value
RETURN value ORDER BY value
```

返回三行 `1, 1, 2`。重复值不会因为 Property 或节点相同而自动消失。没有共享变量的多个 Pattern 可以形成笛卡尔积；要避免无意扩大结果，使用明确的关系或连接条件。

<!-- example: foundation-model-zero -->
```cypher
UNWIND [] AS value
RETURN value
```

返回零行，但仍有 `value` 结果列。空输入的全局聚合可能重新产生一行，详见 [空输入](aggregation.md#empty-input)。

<a id="scope"></a>

## 变量绑定与作用域

Pattern 为 Node、Relationship、Path 绑定变量；`UNWIND`/`FOR`、`LET`、投影别名和 Procedure `YIELD` 也可产生变量。变量名称区分大小写，只能在可见作用域中使用；变量既不是字符串 key，也不是数据库中的全局名称。

`WITH` 形成投影边界：只有显式投影或 `*` 保留的变量进入后续 clause；`WITH p AS person` 后使用 `person`，旧名称 `p` 不再由该投影保留。`LET` 增加新变量并保留原变量；它不能重定义已绑定变量。表达式中的 comprehension、predicate 和 reduce 局部变量只在对应表达式内部可见。

<!-- example: foundation-model-scope -->
```cypher
WITH 5 AS original
LET doubled = original * 2
WITH doubled AS result
RETURN result
```

返回 `result = 10`。最后的作用域中没有 `original` 或 `doubled`。

<!-- example: foundation-model-scope-error -->
```cypher
WITH 5 AS original
WITH original AS renamed
RETURN original
```

返回 `SEMANTIC_ERROR`。`CALL` 导入变量具有额外规则：通过 scope clause 显式导入的变量在 inner query 中保持可见，不能简单套用普通 `WITH` 的丢弃规则。见 [子查询](subqueries.md)。

同一个 Pattern 中重用图变量会限制到同一元素，而不是重新分配它；把一个 Integer 变量当作节点使用、重复绑定冲突的变量类别，属于语义错误。子查询、UNION、WHEN、NEXT 的输入和输出作用域见 [查询组合](composition.md)。

<a id="columns-and-order"></a>

## 结果列、重复值与顺序

终端 `RETURN` 决定结果列及其顺序；`AS` 提供稳定列名。表达式没有别名时也会得到列名，但宿主程序应为计算列显式命名，尤其在组合查询和后续引用时。`RETURN *` 展开当前可见变量；面向稳定 API 的查询应列出所需列，避免新增变量改变输出形状。

没有 `ORDER BY` 时，不保证结果顺序。当前扫描、索引或执行计划恰好产生的顺序不属于合同；`UNWIND` 的列表顺序也不能代替最终排序。分页需要在 `ORDER BY` 中给出稳定且足以打破并列的键，再使用 `SKIP`/`LIMIT`。排序后的查询如果继续经过会改变行序的 clause，最终返回前仍需按实际需要排序。

<!-- example: foundation-model-page -->
```cypher
UNWIND [{key:2,name:'乙'},{key:1,name:'甲'},{key:3,name:'丙'}] AS item
RETURN item.key AS key, item.name AS name
ORDER BY key SKIP 1 LIMIT 1
```

返回 `[2,"乙"]`。`DISTINCT` 按整个投影行去重；`count(DISTINCT x)` 只改变该 aggregate 的输入，不使其它输出自动 distinct。

<a id="graph-state"></a>

## 查询观察哪个图状态

一个 SQLite database 对应一个 versioned graph。普通 execution 在开始时解析 Branch/Commit 并固定其 base Snapshot；执行期间另一连接移动 Branch head 不改变当前 query 的 base Snapshot。历史 Commit 只读。具体 Branch、`at`、Graph View 输入由 [execution options](../reference/execution-options.md) 提供。

同一 query 中，后一个 clause 观察前序 clause 已完成的全部写入，不能把前一 clause 的逐行执行时机当作查询语义。中间投影影响传递哪些变量，不清除已经完成的图写入。

<!-- example: foundation-model-write-visibility -->
```cypher
CREATE (:PipelineExample {value:1}), (:PipelineExample {value:2})
WITH 1 AS ignored
MATCH (n:PipelineExample)
RETURN count(n) AS total
```

在空图执行返回 `total = 2`。普通 mutating execution 的修改作为同一个逻辑写单元原子完成，失败时回滚；外层 SQLite transaction 与 Lithograph explicit transaction 的关系见 [事务参考](../reference/transactions.md)。Cypher `IN TRANSACTIONS` 使用独立 batch transaction，是上述普通 execution 边界的特定例外，见 [事务子查询](transaction-subqueries.md)。

<a id="termination"></a>

## 完整查询与消费结果

读取查询通常以 `RETURN` 结束；不需要返回行时使用 `FINISH`。终端写入或 unit Procedure/subquery 可按自身语法结束。单独 `MATCH`、`WITH`、`LET`、`UNWIND`，以及末尾只有 returning `CALL { ... }` 而未消费其输出，都不是完整查询。

<!-- example: foundation-model-finish -->
```cypher
UNWIND [1, 2] AS x
FINISH
```

返回零列零行。`FINISH` 丢弃结果行，并不跳过前面的执行或写入。

`lithograph_rows()` 是拉取式 execution stream。宿主需要消费到 summary 才能把一个 execution 视为成功；只读到首行不能证明后续无错。提前关闭未完成的普通写 execution 会回滚未 durable 的修改，active explicit transaction 会按其失败清理合同 abort。完整消费、取消和归还连接流程见 [集成指南](../guide/integration.md) 与 [事务参考](../reference/transactions.md)。
