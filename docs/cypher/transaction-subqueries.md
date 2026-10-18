# Transaction subquery：分批事务

适用版本：Lithograph **v0.3.3**，Cypher Profile **`CY25-2026.08`**。

[Cypher 手册](index.md) · [兼容范围](../reference/cypher-compatibility.md) · [事务指南](../guide/transactions.md)

`CALL { … } IN TRANSACTIONS` 将输入行分组，在独立的 inner transaction 中执行子查询。它用于允许部分成功的大批量操作。每个成功且实际修改 graph、Schema 或 Index 的 batch 创建一个 Commit；只读 batch 不创建 Commit。后续失败不会撤销之前已经提交的 batch。

<a id="syntax"></a>

## 完整语法

```text
CALL [(importedVariable [, ...] | *)] {
  subquery
}
IN [ [concurrency] CONCURRENT ] TRANSACTIONS
  [OF batchSize ROW | ROWS]
  [DISJOINT BY NONE | AUTO | (expression [, ...])]
  [ON ERROR CONTINUE | BREAK | FAIL |
     RETRY [[FOR] seconds SEC | SECOND | SECONDS]
           [THEN CONTINUE | BREAK | FAIL]]
  [REPORT STATUS AS statusVariable]
```

方括号表示可选部分；`|` 表示选一个形式，不是查询中的字面字符。`OF …`、`DISJOINT BY …` 位于 `ON ERROR` / `REPORT STATUS` 前。`ON ERROR` 和 `REPORT STATUS` 各最多一次；同一 transaction modifier 不得重复。

| 设置 | 默认值与要求 |
| --- | --- |
| `OF batchSize ROWS` | 默认 **1000 输入行**。batchSize 必须是正 Integer；`ROW` 与 `ROWS` 等价，可使用参数 |
| 不写 `CONCURRENT` | 按输入 batch 顺序运行；后续 batch 观察先前成功 batch 的图状态 |
| `concurrency` | 可省略；显式值必须为非零 Integer。负数字面量被 semantic validation 拒绝，负值参数可接受；实际图写入仍受 SQLite 串行提交约束 |
| `DISJOINT BY` | 只允许与 `CONCURRENT TRANSACTIONS` 组合；显式表达式只能使用导入变量，且不能聚合 |
| `ON ERROR` | 默认 `FAIL` |
| `RETRY` 时长 | 默认 **30 秒**；显式值为有限、非负 Integer 或 Float，单位秒 |
| `RETRY … THEN` | 默认 `FAIL` |
| `REPORT STATUS AS` | 仅当最终错误策略为 `CONTINUE` 或 `BREAK` 时合法，包括 `RETRY … THEN CONTINUE/BREAK` |

数量按进入 transaction subquery 的**外层输入行数**计算；一次输入可能在子查询中产生零行、多行或许多图修改。将 `UNWIND` 放在子查询内部不会按其展开结果自动划分 batch。最后一个不足 batchSize 的 batch 也会执行；零输入不启动 batch。

导入、变量隔离与返回行遵守 [CALL 子查询](subqueries.md)。推荐显式 `CALL (value) { … }`；`CALL ()` 不导入变量，`CALL (*)` 导入全部可见变量。unit subquery 保留外层行；有 `RETURN` 的子查询按实际返回行组合外层结果。子查询返回的变量不能与外层已有变量冲突。

transaction subquery 应放在当前 outer query 的 clause 链中；不支持将它嵌套到另一个 `CALL` 子查询中，也不支持放入 `UNION` 的分支。

<a id="boundaries"></a>

## 允许的执行上下文

两个 SQL surface 都可以执行：`lithograph()` 返回完整 envelope，`lithograph_rows()` 输出 execution event stream。它们要求 SQLite connection 处于 **autocommit mode**，且没有 active Lithograph explicit transaction。

| 上下文 | 结果 |
| --- | --- |
| 普通 standalone `SELECT lithograph(…)` / `lithograph_rows(…)` | 允许，逐 batch 独立提交 |
| caller-owned `BEGIN … COMMIT` | `TRANSACTION_BOUNDARY_REQUIRED` |
| active `lithograph_tx_begin()` | `TRANSACTION_BOUNDARY_REQUIRED`；失败按 explicit transaction 规则 abort |
| Merge candidate execution | `TRANSACTION_BOUNDARY_REQUIRED` |
| 在 transaction subquery 前已有 outer graph write | 拒绝独立事务边界；之前的未提交 outer write 不留下 Commit |
| `readOnlyNoExternalIO=true` | prepare 阶段 `INVALID_ARGUMENT`，即使子查询只读或不会获得输入行 |

可以在 batch 后继续普通 outer 写入；这部分遵守自己的 ordinary execution 边界。后缀写入失败只撤销该后缀，已经成功的 inner batch 不变。需要“所有步骤全部成功或全部撤销”时，使用[显式事务](../reference/transactions.md)，不要使用本语法。

<a id="successful-batches"></a>

## 成功 batch 与返回值

以下查询在空图运行，输入 3 行、batch size 为 2，因此产生两个成功写入 batch 和两个 Commit。

<!-- example: csv-batch-return -->
```cypher
UNWIND [1,2,3] AS value
CALL (value) {
  CREATE (item:BatchItem {value:value})
  RETURN item.value AS stored
} IN TRANSACTIONS OF 2 ROWS
RETURN value, stored
ORDER BY value
```

结果为 `[1,1]`、`[2,2]`、`[3,3]`。导入变量 `value` 和返回变量 `stored` 在外层都可见。

<!-- example: csv-batch-check -->
```cypher
MATCH (item:BatchItem)
RETURN count(item) AS items
```

导入后结果为 `[3]`。只读子查询也支持 batching：

<!-- example: csv-batch-read -->
```cypher
UNWIND [1,2,3] AS value
CALL (value) {
  RETURN value * 2 AS doubled
} IN TRANSACTIONS OF 2 ROWS
RETURN doubled
ORDER BY doubled
```

结果为 `[2]`、`[4]`、`[6]`，没有新增 Commit。

<a id="error-modes"></a>

## ON ERROR 与部分提交

一个 batch 中任意输入失败，会撤销该 batch 的全部写入；该 batch 中其它原本成功的输入也不能保留。错误策略作用于 inner batch 执行失败，不会捕获 query parse/semantic failure、上游 CSV 读取失败或 outer 后缀错误。

| 策略 | 失败 batch | 后续输入 | 外层结果 |
| --- | --- | --- | --- |
| `FAIL` | rollback | 停止执行 | 返回错误；之前成功的 batch 保留 |
| `CONTINUE` | rollback | 继续下一 batch | 对失败输入生成结果行，子查询输出变量为 `null`；可以检查 status |
| `BREAK` | rollback | 不再执行后续 batch | 保留后续外层输入并返回未启动状态；子查询输出变量为 `null` |
| `RETRY … THEN mode` | 失败 attempt rollback；符合条件时重试整个 batch | 重试耗尽或不可重试时采用 mode | 仅最终有效 attempt 的结果可见 |

`CONTINUE` / `BREAK` 可以让 outer query 最终成功，因此仅看到 success summary 不能证明所有 batch 已提交。业务要求逐项确认时，应返回 status 并检查 `committed`。

下面用动态属性 key 制造可预测的类型错误：当 `value=2` 时，key 为 Integer `42`，合法 key 应为 String。

<!-- example: csv-batch-continue -->
```cypher
UNWIND [1,2,3] AS value
CALL (value) {
  CREATE (item:ContinueItem {value:value})
  SET item[CASE value WHEN 2 THEN 42 ELSE 'ok' END] = value
  RETURN item.value AS stored
} IN TRANSACTIONS OF 1 ROW
ON ERROR CONTINUE
REPORT STATUS AS status
RETURN value, stored, status.started AS started, status.committed AS committed
ORDER BY value
```

结果为 `[1,1,true,true]`、`[2,null,true,false]`、`[3,3,true,true]`。失败的 `value=2` 对应节点被撤销：

<!-- example: csv-batch-continue-state -->
```cypher
MATCH (item:ContinueItem)
RETURN item.value AS value
ORDER BY value
```

结果只包含 `[1]`、`[3]`。

如果把 batch size 改为 2，第一条输入也会与失败的第二条一起回滚：

<!-- example: csv-batch-whole-rollback -->
```cypher
UNWIND [1,2,3] AS value
CALL (value) {
  CREATE (item:WholeBatchItem {value:value})
  SET item[CASE value WHEN 2 THEN 42 ELSE 'ok' END] = value
  RETURN item.value AS stored
} IN TRANSACTIONS OF 2 ROWS
ON ERROR CONTINUE
REPORT STATUS AS status
RETURN value, stored, status.committed AS committed
ORDER BY value
```

结果为 `[1,null,false]`、`[2,null,false]`、`[3,3,true]`。这说明 `CONTINUE` 跳过的是整个失败 batch。

<!-- example: csv-batch-break -->
```cypher
UNWIND [1,2,3] AS value
CALL (value) {
  CREATE (item:BreakItem {value:value})
  SET item[CASE value WHEN 2 THEN 42 ELSE 'ok' END] = value
} IN TRANSACTIONS OF 1 ROW
ON ERROR BREAK
REPORT STATUS AS status
RETURN value, status.started AS started, status.committed AS committed
ORDER BY value
```

结果为 `[1,true,true]`、`[2,true,false]`、`[3,false,false]`；第三项从未执行。

<!-- example: csv-batch-fail -->
```cypher
UNWIND [1,2,3] AS value
CALL (value) {
  CREATE (item:FailItem {value:value})
  SET item[CASE value WHEN 2 THEN 42 ELSE 'ok' END] = value
} IN TRANSACTIONS OF 1 ROW
FINISH
```

默认 `FAIL` 返回 `TYPE_ERROR`，但第一项已经提交。在同一数据库检查：

<!-- example: csv-batch-fail-state -->
```cypher
MATCH (item:FailItem)
RETURN item.value AS value
ORDER BY value
```

结果为 `[1]`。恢复时必须依据已提交的数据、业务 key 或导入进度决定重放范围；盲目重放 `CREATE` 会重复创建已成功部分。

<a id="status"></a>

## REPORT STATUS 的字段

`REPORT STATUS AS status` 把 Map 绑定到外层变量。它属于 batch outcome，同一 batch 的各个输入共享 transaction outcome。

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `started` | Boolean | 是否实际启动了该输入所属 batch；`BREAK` 后剩余输入为 `false` |
| `committed` | Boolean | 该 batch 是否成功完成；成功只读 batch 同样为 `true`，并不表示创建了 graph Commit |
| `transactionId` | String 或 null | 已启动 batch 的诊断标识；未启动为 `null`。它不是 Commit descriptor，不作为跨 execution 的永久标识 |
| `errorMessage` | String 或 null | 失败 batch 的诊断信息；成功或未启动为 `null`。不要依赖完整文案做稳定错误分类 |

不带可继续的错误策略时不能声明 status：

<!-- example: csv-batch-invalid-status -->
```cypher
UNWIND [1] AS value
CALL (value) { RETURN value AS innerValue }
IN TRANSACTIONS
REPORT STATUS AS status
RETURN status
```

预期 `SEMANTIC_ERROR`；`ON ERROR FAIL` 和 `RETRY … THEN FAIL` 同样不能与 status 组合。

<a id="retry"></a>

## RETRY 的范围

`ON ERROR RETRY` 重试暂时性的 `SQLITE_BUSY`、`SQLITE_LOCKED` 和 `BRANCH_HEAD_MOVED`，每次 attempt 回滚后重新 pin 当前 Branch head。语法、属性类型、约束等确定性错误不会因设置 `RETRY` 就无限重复。

```text
IN TRANSACTIONS OF 100 ROWS
ON ERROR RETRY FOR 5 SECONDS THEN CONTINUE
REPORT STATUS AS status
```

时长限制重试等待窗口，不是子查询的执行 deadline；不能替代宿主取消和 I/O timeout。重试可能再次执行外部读取或其它不能由图事务撤销的动作，调用方应在选择 batch 内容时考虑其幂等性。业务应把 batch size、重试时间和 SQLite busy policy 一起设计。

<!-- example: csv-batch-retry-type -->
```cypher
UNWIND [2] AS value
CALL (value) {
  CREATE (item:RetryItem {value:value})
  SET item[42] = value
} IN TRANSACTIONS OF 1 ROW
ON ERROR RETRY FOR 0 SECONDS THEN CONTINUE
REPORT STATUS AS status
RETURN value, status.committed AS committed
```

结果为 `[2,false]`。错误是确定性的属性 key 类型错误，直接采用 `THEN CONTINUE`，不会把它当成可恢复的锁冲突。

<a id="concurrent"></a>

## CONCURRENT 与 DISJOINT BY

```text
IN 4 CONCURRENT TRANSACTIONS OF 100 ROWS DISJOINT BY (accountId)
IN CONCURRENT TRANSACTIONS DISJOINT BY AUTO
IN $concurrency CONCURRENT TRANSACTIONS DISJOINT BY NONE
```

`DISJOINT BY (expressions)` 声明用于协调访问的导入值；同一 key 的工作需要互相协调。`AUTO` 采用自动协调，`NONE` 不提供显式 disjoint key。该声明不创建 uniqueness constraint，也不会证明输入业务上互不相交。

Lithograph 运行在同一个 SQLite database 内，同一 Branch 的实际 batch transaction 经协调后串行提交。并发语法不保证多个 graph writer 同时持有写锁；不要据此推导 Neo4j 的并行加速比例。每个 batch 从其实际进入事务时的最新 head 开始；外部 writer 造成的真实 stale head 仍可能失败。

<!-- example: csv-batch-concurrent -->
```cypher
UNWIND [1,2,3] AS value
CALL (value) {
  CREATE (item:ConcurrentItem {value:value})
  RETURN item.value AS stored
} IN 2 CONCURRENT TRANSACTIONS OF 1 ROW
DISJOINT BY (value)
RETURN stored
ORDER BY stored
```

结果为 `[1]`、`[2]`、`[3]`。显式 `ORDER BY` 固定公开结果顺序；并发 batch 的实际提交次序不应成为业务逻辑前提。

存在 Graph View 时，各个 batch 使用同一个 selector，但基于各自事务 pin 的 graph state 重新计算可见成员。后续顺序 batch 能观察前一成功 batch 在该 View 中新增的可见数据。

<a id="csv-composition"></a>

## 与 LOAD CSV 和其它 clause 组合

把 `LOAD CSV` 放在外层，才能按 CSV 输入记录分 batch：

```text
LOAD CSV WITH HEADERS FROM $source AS row
CALL (row) {
  MERGE (person:Person {id:row.id})
  SET person.name = row.name
} IN TRANSACTIONS OF 1000 ROWS
FINISH
```

如果 transaction subquery 前存在 `WITH count(row)`、`DISTINCT`、排序等全局运算，先完成该 prefix 的正常语义，然后按其输出行 batching。batch size 因而可能对应聚合后的行，而非 CSV 原始记录。

使用 [LOAD CSV 页](load-csv.md#atomicity)中的 `malformed.csv`，参数 `$source` 指向它的 URI：

<!-- example: csv-batch-malformed -->
```cypher
LOAD CSV WITH HEADERS FROM $source AS row
CALL (row) {
  CREATE (:PartialCsv {name:row.name})
} IN TRANSACTIONS OF 1 ROW
FINISH
```

第三条数据记录格式错误使 outer query 返回 `RESOURCE_ERROR`，之前两条已提交：

<!-- example: csv-batch-malformed-state -->
```cypher
MATCH (person:PartialCsv)
RETURN person.name AS name
ORDER BY name
```

结果为 `['Ada']`、`['Bo']`。上游 reader 的格式错误不受 inner `ON ERROR CONTINUE` 捕获。

transaction subquery 可以与 `NEXT`、`WHEN` 和后续读取组合；这些 clause 的变量与列合同照常生效。组合不会把已经 durable 的独立 batch 重新变成一个 outer 原子事务。

<a id="streaming"></a>

## 流式结果、取消与 summary

`lithograph_rows()` 只读取 `columns` event 不启动 batch；推进 cursor 时开始按 batch 消费输入。每个 batch 必须先知道最终提交或失败 outcome，才能公开该 batch 的结果；失败或重试 attempt 的 provisional 成功行不会泄漏。

成功 batch 的结果行开始交付时，该 batch 已提交。提前 close、interrupt、JSON 编码或结果交付错误会停止后续 batch，并回滚当前尚未提交的 batch；已经提交的 batch 保留，即使它的全部返回行尚未被消费。`lithograph()` 最终 envelope 生成失败同样不会撤销这些 earlier batch。

因此 outer execution 没有 success summary，也可能已经修改图。排障和重试必须检查实际图状态及 History，不能只用“没有收到 summary”判断是否可以完整重放。完整 cursor 生命周期见[事务参考](../reference/transactions.md)。

<a id="errors"></a>

## 类型与错误检查

| 情况 | 错误 |
| --- | --- |
| batchSize 为 `null`、Float、String | `TYPE_ERROR` |
| batchSize 为零或负值 | `INVALID_ARGUMENT` |
| concurrency 为 `null`、Float、String | `TYPE_ERROR` |
| concurrency 为零 | `INVALID_ARGUMENT` |
| 负数 concurrency 字面量 | `SEMANTIC_ERROR`；负值参数可接受 |
| retry 时长非数值 | `TYPE_ERROR` |
| retry 时长负数或非有限值 | `INVALID_ARGUMENT` |
| `DISJOINT BY` 不带 `CONCURRENT`、含未导入变量或聚合 | `SEMANTIC_ERROR` |
| 重复 transaction modifier、非法 status/error 组合 | `SEMANTIC_ERROR` |
| transaction subquery 位于 `UNION` 分支或嵌套 transaction subquery | `SEMANTIC_ERROR` |
| 非法独立事务上下文 | `TRANSACTION_BOUNDARY_REQUIRED` |

<!-- example: csv-batch-zero -->
```cypher
UNWIND [1] AS value
CALL (value) { RETURN value AS innerValue }
IN TRANSACTIONS OF 0 ROWS
RETURN innerValue
```

预期 `INVALID_ARGUMENT`。

<!-- example: csv-batch-null -->
```cypher
UNWIND [1] AS value
CALL (value) { RETURN value AS innerValue }
IN TRANSACTIONS OF $batchSize ROWS
RETURN innerValue
```

参数 `{"batchSize":null}` 返回 `TYPE_ERROR`。

<!-- example: csv-batch-invalid-disjoint -->
```cypher
UNWIND [1] AS value
CALL (value) { RETURN value AS innerValue }
IN TRANSACTIONS DISJOINT BY (value)
RETURN innerValue
```

预期 `SEMANTIC_ERROR`。

<!-- example: csv-batch-union -->
```cypher
CALL () { RETURN 1 AS value } IN TRANSACTIONS
RETURN value
UNION
RETURN 2 AS value
```

预期 `SEMANTIC_ERROR`。普通子查询的 `UNION` 用法不能直接套用到独立事务子查询。

<!-- example: csv-batch-nested -->
```cypher
CALL () {
  CALL () { RETURN 1 AS value } IN TRANSACTIONS
  RETURN value
} IN TRANSACTIONS
RETURN value
```

预期 `SEMANTIC_ERROR`。

标准语言用法参考 [Neo4j Cypher 25 Transaction subqueries](https://neo4j.com/docs/cypher-manual/25/subqueries/subqueries-in-transactions/)。本页的 SQL 入口、Commit、SQLite 串行提交和 stream lifecycle 说明属于 Lithograph 的公开执行行为。
