# SQL API 与结果合同

**版本：v0.1.0。** SQL 入口作用于加载扩展的 connection 的 `main` database。查询/参数/options 均为 UTF-8 TEXT，省略的可选 JSON 参数等价于 `'{}'`；SQL `NULL` 不是省略参数的替代品。

## 入口清单

| SQL 入口 | 输入 | 返回 JSON TEXT / 行 |
| --- | --- | --- |
| `lithograph_init()` | 无 | `{databaseId,storageFormat,root,branch}`；显式初始化或迁移 |
| `lithograph_version()` | 无 | `{extension,abi,cypherProfile,databaseId,storageFormat:{min,max,current}}` |
| `lithograph_validate(query)` | 一个 Cypher 文本 | 成功 `{valid:true,cypherProfile}`；失败为 SQLite error |
| `lithograph_integrity_check()` | 无 | `{ok,errors,checked}`；完整检查，不是廉价请求前置检查 |
| `lithograph(query[,params[,options]])` | Cypher + JSON object TEXT | 完整 result envelope |
| `lithograph_rows(query[,params[,options]])` | 同上，只读且无外部 I/O | 每行 `ordinal INTEGER, columns TEXT, row TEXT` |

`lithograph_version()` 可在未初始化库调用；其他 graph API 需要初始化。加载扩展不会自动 init。init 的 `root` 是裸 hash；版本 procedure 的 `commit` 是带 `commit/` 前缀的 Descriptor。

`validate` 不执行图操作，也不接收参数值；它不保证真实执行时的 Schema、Constraint、数据、权限、资源都满足。失败时没有 `{valid:false}` 的成功 envelope。

## 参数绑定示例

下面是应用交给 SQLite 的 statement 模板，`?` 由宿主 binding 绑定，不能原样在 CLI 当成已给值的调用：

```text
SELECT lithograph(?, ?, ?)

query   = MATCH (p:Person) WHERE p.name=$name RETURN p.name AS name
params  = {"name":"Alice"}
options = {"branch":"main"}
```

Graph mutation 应作为独立调用执行一次。不要把带副作用的 `lithograph()` 放进一个从多行 SQL 表调用它的 SELECT，再假定只执行一次。

## Scalar envelope

以下展示形状；Commit 值为示意，不是可查询的真实版本。实际 counters 全部存在，未发生的为 0：

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

`rows` 中每个数组按 `columns` 顺序解释。可以出现同名列，因此通用客户端不能无条件转成以列名为 key 的 Map；示例 helper 只在已知列名唯一的 procedure 上这样做。

`queryType` 的值域是 `read | write | schema | version | mixed`。不带结果投影的 mutation 返回 `columns:[]`、`rows:[]`；这不表示没有写入，必须查看成功状态、Commit 与 counters。

| 执行上下文 | `summary.commit` |
| --- | --- |
| 普通读 | 此查询读取的 pinned Commit |
| 普通 graph / Schema / Index 写 | 新 Commit |
| 一般 ref-only version 操作 | 操作后 active Branch 的 Commit；特定 ref 的结果以 procedure 返回列为准 |
| Merge candidate query | `null`，同时 `mergeSession:{id,revision}` |
| Native tx_execute | `null`；最终值由 tx_commit 返回 |
| `lithograph.index.rebuild` | `null`；目标 anchor 在返回行的 `commit` 列 |

v0.1.0 实际序列化在普通结果中包含 `mergeSession:null`；客户端应兼容 null / absence，而不能仅检查 key 存在就当作 candidate。

`metrics` 固定含 `rows,dbHits,elapsedMicros`。`PROFILE` 额外返回 `summary.profile.operators`，每项 `id,operator,rows,dbHits`；它会执行查询。`EXPLAIN` 不执行图 mutation，也不构建索引缓存。正常耗时与 dbHits 随数据、缓存、查询而变，不把示例数字当性能保证。

## 行适配器

`ordinal` 从 0 开始。`columns` 是 JSON 字符串数组；`row` 是同顺序的 JSON 值数组，使用与 scalar / Native 相同的 tagged encoding。每行都携带 columns；零行结果没有携带 metadata 的行，需要空结果列信息时用 scalar 或 Native COLUMNS event。

只允许 read-only、无 external I/O、无 connection/ref/state mutation 的查询。CREATE/SET/DELETE、Schema DDL、LOAD CSV、checkout、GC、index rebuild 等不能通过此接口执行。不要用 SQL 外层 LIMIT 来表达 Cypher 语义的 top-k；需要的 WHERE/ORDER BY/LIMIT 应写在 Cypher 内。

调用方逐行消费并关闭 cursor。流式接口不保证 ORDER BY / aggregation / DISTINCT 等需要物化的查询恒定内存，也不消除单行 JSON 长度限制。

## 错误与事务

SQL 错误文本是 `LITHOGRAPH_<CATEGORY>: <message>`，保留 SQLite primary result code。不会把失败包装成成功 envelope 的 `error` 字段。详见 [Errors](errors.md)。

每次 mutating scalar 调用使用内部 savepoint。外层 SQL BEGIN/COMMIT 仍由宿主掌握；多次调用不会因此变成一个图 Commit。SQL Bridge 不支持 transaction-owning subquery，checkout 另见 [Known Issues](known-issues.md)。

依据：[公开 SQL 实现](../../crates/lithograph-extension/src/scalar.rs)、[结果适配](../../crates/lithograph-extension/src/execution.rs)、[SQL Bridge](../design/interfaces.md#sql-bridge)。
