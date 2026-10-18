# SQL API 与结果合同

适用版本：**v0.3.3** · [产品手册](../guide/README.md) · [接口参考](README.md)

SQL 入口作用于加载扩展的 connection 的 `main` database；ATTACH 的其它 schema 不参与。查询/参数/options 必须为 UTF-8 TEXT；省略的可选 JSON 参数等价 `'{}'`，SQL `NULL` 不表示省略。应用使用 SQLite statement 参数绑定传递这些文本。

<a id="entrypoints"></a>

## 入口清单

| SQL 入口 | 输入 | 返回 JSON TEXT / 行 |
| --- | --- | --- |
| `lithograph_init()` | 无 | `{databaseId,storageFormat,root,branch}`；显式初始化或迁移 |
| `lithograph_version()` | 无 | `{extension,cypherProfile,databaseId,storageFormat:{min,max,current}}` |
| `lithograph_validate(query)` | 一个非空 Cypher TEXT | 成功 `{valid:true,cypherProfile}`；失败为 SQLite error |
| `lithograph_integrity_check()` | 无 | `{ok,errors,checked}`；完整检查报告 |
| `lithograph(query[,params[,options]])` | 非空 Cypher TEXT；可选 JSON object TEXT | 完整 result envelope |
| `lithograph_rows(query[,params[,options]])` | 同上 | `ordinal,event,data` execution event stream |
| `lithograph_tx_begin(options_json)` | 恰好一个 JSON object TEXT | `{baseCommit}` |
| `lithograph_tx_commit()` | 无 | `{commit,counters}` |
| `lithograph_tx_abort()` | 无 | `{aborted:true}` |

没有 application-facing Native query ABI；C、Python、Rust、Go 等宿主均通过 SQLite SQL 调用。Provider SPI 是两个扩展间的接口，见 [Native API](native-api.md)。

`lithograph()`、init、tx lifecycle 使用 SQLite `DIRECTONLY`；不要放入 schema view/trigger 或依赖持久 schema 间接执行副作用。mutation 应作为一条独立宿主调用执行一次，不要放进遍历多行 SQL 表的 SELECT 后假定只执行一次。

<a id="initialization"></a>

## 初始化与版本

加载扩展只注册入口，不自动创建图。未初始化库可以调用 `lithograph_version()`：`databaseId` 与 `storageFormat.current` 为 null，`min/max` 表示扩展接受的格式范围，`extension` 为实际 binary 版本，`cypherProfile` 为冻结 Profile。

`lithograph_init()` 在新库创建 Root Commit 与 `main` Branch；已有受支持库完成必要迁移和完整性检查后返回同一 repository identity，不重建历史。返回 `root` 是 **64 个十六进制字符的 hash 字符串**，不是 `commit/` Descriptor；`branch` 字段为初始 Branch 名 `main`，不能用它推断连接刚执行的 checkout 状态。`databaseId` 是 canonical UUID，便于区分不同数据库；分支不改变它。

```sql
SELECT lithograph_version();
SELECT lithograph_init();
SELECT lithograph_version();
```

fresh/current storage format 为 `3`；更高格式报 `FORMAT_TOO_NEW`，不能修改内部 marker“降级”。init 不占用宿主 `PRAGMA user_version`。其它 graph API 要求初始化；init/validate/integrity 在 active Lithograph explicit transaction 中有各自的生命周期限制，不作为 staged graph query 使用。

<a id="validation"></a>

## `lithograph_validate(query)`

验证 Cypher parsing 与 static semantics，不执行图操作。它不接受 params/options，因此不会验证 `readOnlyNoExternalIO` 或后续 `readAccess` / `writeAccess` policy，也不能证明实际参数、Schema、Constraint、数据、资源和 I/O 来源可用。调用仍要求图已初始化。成功返回 `{"valid":true,"cypherProfile":"CY25-2026.08"}`；失败通过 SQLite error channel，没有 `{valid:false}` 成功 envelope。

<a id="integrity-check"></a>

## `lithograph_integrity_check()`

正常健康库返回：

```json
{"ok":true,"errors":[],"checked":["metadata","storage","history","checkpoints"]}
```

发现可报告的完整性问题时可返回 `ok:false`。`errors` 每项包含 STRING `category` / `message`、INTEGER `sqliteCode`，以及 INTEGER 或 null 的 `line` / `column`；这些字段供完整性报告定位，不建立第二套 query failure channel。`checked` 是本次检查范围的 STRING array；元数据已异常时可能仅为 `["metadata"]`。API 本身无法完成（例如 busy、I/O、格式过新）仍返回 SQLite error。调用方必须同时检查 SQL 成功与 `ok`，不能只看函数返回了一行。

这是 full integrity 操作，会读取 history/storage；不适合作为每次请求的廉价 health check。发现损坏后停止 mutation、保存副本，再按 [运维指南](../guide/operations.md) 检查恢复。

<a id="parameter-binding"></a>

## 参数绑定示例

下面是应用交给 SQLite 的 statement 模板，`?` 必须由 binding 绑定：

```text
SELECT lithograph(?, ?, ?)

query   = MATCH (p:Person) WHERE p.name=$name RETURN p.name AS name
params  = {"name":"Alice"}
options = {"branch":"main","readOnlyNoExternalIO":true}
```

第二、第三参数是 JSON 文本，而不是 SQLite object、SQL NULL 或单独的 SQL 参数键值列表。安全编码、64-bit INTEGER 和 tagged values 见 [Values](values.md)，所有 options 见 [Execution Options](execution-options.md)。未提供所需 query parameter 在执行时失败；参数不能替代关键字或任意语法片段。

<a id="scalar-envelope"></a>

## Scalar envelope

`lithograph()` 成功时返回一个 JSON TEXT。以下 Commit 是结构示意，metrics 数字不是性能保证：

```json
{
  "columns": ["name"],
  "rows": [["Alice"]],
  "summary": {
    "queryType": "read",
    "commit": "commit/<64-hex-id>",
    "mergeSession": null,
    "counters": {
      "nodesCreated": 0, "nodesDeleted": 0,
      "relationshipsCreated": 0, "relationshipsDeleted": 0,
      "propertiesSet": 0, "propertiesRemoved": 0,
      "labelsAdded": 0, "labelsRemoved": 0,
      "constraintsAdded": 0, "constraintsRemoved": 0,
      "indexesAdded": 0, "indexesRemoved": 0
    },
    "metrics": {"rows": 1, "dbHits": 0, "elapsedMicros": 0}
  }
}
```

`columns` 是 STRING 数组；每个 `rows` 元素是按 columns 顺序排列的值数组，元素使用 Lithograph JSON v1。可以出现同名列，通用客户端不能无条件转成以列名为 key 的 Map。需要对象结果时在查询中给出唯一别名，或保留位置表示。

没有最终 RETURN / YIELD 等公开投影的 mutation 返回 `columns:[]`、`rows:[]`；这仍可完成写入。`summary.queryType` 的封闭值是 `read | write | schema | version | mixed`。所有列出的 counters 始终存在，无操作为 0；counter 是执行行为/最终事务差异的报告，不以 rows 数量替代它。

| 执行上下文 | `summary.commit` |
| --- | --- |
| 普通读 | 查询读取的 pinned Commit |
| 普通 graph / Schema / Index 写 | 新 Commit |
| 一般 ref-only version 操作 | 操作后 active Branch 的 Commit；特定 ref 以 Procedure 返回列为准 |
| Merge candidate query | null，同时 `mergeSession:{id,revision}` |
| active Lithograph explicit transaction 内的 execution | null，最终由 tx_commit 返回 |
| `lithograph.index.rebuild` | null，目标 anchor 在返回行 `commit` 列 |

普通结果的 `mergeSession` 可为 null；客户端应兼容 null/absence，只有非 null object 才表示 candidate。

`metrics` 固定包含 INTEGER `rows,dbHits,elapsedMicros`。elapsedMicros 覆盖 query/operator 到全部公开 rows 和 summary candidate 确定，不包含最终 envelope 编码、SQLite result handoff 或最后 commit/finalize 的时间。测端到端延迟时由宿主另行计时。

`PROFILE` 执行实际查询，额外返回 `summary.profile.operators`，每项 `{id,operator,rows,dbHits}`；id 为零基，operator 顺序与 physical plan 一致，各 operator dbHits 之和等于 metrics.dbHits。普通查询和 EXPLAIN 不返回 profile。`EXPLAIN` 只编译/验证和展示计划，不执行 graph mutation，也不构建索引缓存；安全 options 仍检查它包含的 AST。

<a id="sql-explicit-transactions"></a>

## SQL 显式事务

三个 lifecycle function 只管理 Engine-owned transaction；begin 后继续使用标准 execution surface：

```sql
SELECT lithograph_tx_begin('{"author":"demo","message":"create people"}');
SELECT lithograph('CREATE (:Person {name: ''张三''}) FINISH');
SELECT lithograph('CREATE (:Person {name: ''李四''}) FINISH');
SELECT lithograph('MATCH (p:Person) RETURN p.name AS name ORDER BY name');
SELECT lithograph_tx_commit();
```

不再额外包 SQL BEGIN；多个 staged execution 只有最终 commit 发布唯一 graph Commit。中途任一 execution error/interrupt，或 rows cursor 在 success summary 前关闭，会自动 rollback 全部 staged writes。

`tx_begin` 必需显式 object，begin 与 active execution 的 options 不同；完整状态、纯读和 empty-delta、连接池处理见 [Transactions Reference](transactions.md)。

<a id="rows-execution-stream"></a>

## Rows execution stream

`lithograph_rows()` 是 table-valued function，visible columns 固定：

| 列 | SQLite 类型 | 含义 |
| --- | --- | --- |
| `ordinal` | INTEGER | 当前 scan 的连续零基事件序号 |
| `event` | TEXT | `columns`、`row`、`summary` |
| `data` | TEXT | 此事件的 JSON payload |

hidden `query/params/options` 是输入，不属于 Cypher 结果列。事件顺序：

```text
0       columns  ["name", ...]
1..N    row      [<Lithograph JSON values>...]
N + 1   summary  {<summary object>}
```

零 row 仍有 columns→summary；无公开结果投影时 columns payload 为 `[]`。读取语句：

```sql
SELECT ordinal,event,data
FROM lithograph_rows('MATCH (p:Person) RETURN p.name AS name ORDER BY name');
```

summary payload 与 scalar 的 summary 使用相同 shape。stream 可执行写入、LOAD CSV、Managed Semantic 和 transaction subquery；外部 I/O 本身不是 adapter 拒绝理由。JSON 编码和错误与 scalar 共用合同。

必须 drain 全部事件，看到唯一 summary 并检查正常 `SQLITE_DONE`。外层 SQL WHERE/LIMIT 改变消费，不能代替 Cypher WHERE/LIMIT；尤其外层 `LIMIT 1` 在 columns 后关闭 cursor，会取消普通未完成写入。不要丢掉 columns 或 summary 后只检查几条 row 就宣布成功。

普通 side effects 在 terminal summary 成功前保持可 rollback；transaction-subquery earlier durable batches 是例外，后面的错误、编码失败或 early-close 不会撤销它们。详细生命周期见 [cursor reference](transactions.md#stream-lifecycle)。

<a id="errors-and-transactions"></a>

## 错误与事务

调用失败通过 SQLite error channel 返回 `LITHOGRAPH_<CATEGORY>: <message>`，可能带尾部 `[line=N,column=N]`，并保留 SQLite primary code。没有成功 envelope 的 `error` 字段或 stream error event；integrity 的 `ok:false` 是显式检查报告，属于该入口定义的正常结果。

普通执行失败不发布该 invocation 的未提交 graph state；caller-owned transaction 仍由宿主决定 COMMIT/ROLLBACK。batch 查询可能已经提交 earlier batches，不能看到 SQL error 就盲目重放。完整分类与恢复见 [Errors](errors.md) 和 [Transactions](transactions.md)。
