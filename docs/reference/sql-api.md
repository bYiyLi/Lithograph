# SQL API 与结果合同

SQL 入口作用于加载扩展的 connection 的 `main` database。查询/参数/options 均为 UTF-8 TEXT，省略的可选 JSON 参数等价于 `'{}'`；SQL `NULL` 不是省略参数的替代品。

## 入口清单

| SQL 入口 | 输入 | 返回 JSON TEXT / 行 |
| --- | --- | --- |
| `lithograph_init()` | 无 | `{databaseId,storageFormat,root,branch}`；显式初始化或迁移 |
| `lithograph_version()` | 无 | `{extension,cypherProfile,databaseId,storageFormat:{min,max,current}}` |
| `lithograph_validate(query)` | 一个 Cypher 文本 | 成功 `{valid:true,cypherProfile}`；失败为 SQLite error |
| `lithograph_integrity_check()` | 无 | `{ok,errors,checked}`；完整检查，不是廉价请求前置检查 |
| `lithograph(query[,params[,options]])` | Cypher + JSON object TEXT | 完整 result envelope |
| `lithograph_rows(query[,params[,options]])` | 同上 | `ordinal,event,data` execution event stream |
| `lithograph_tx_begin(options_json)` | begin options JSON object TEXT | `{baseCommit}` |
| `lithograph_tx_commit()` | 无 | `{commit,counters}` |
| `lithograph_tx_abort()` | 无 | `{aborted:true}` |

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
| active Lithograph explicit transaction 内的普通 execution | `null`；最终值由 tx_commit 返回 |
| `lithograph.index.rebuild` | `null`；目标 anchor 在返回行的 `commit` 列 |

v0.1.0 实际序列化在普通结果中包含 `mergeSession:null`；客户端应兼容 null / absence，而不能仅检查 key 存在就当作 candidate。

`metrics` 固定含 `rows,dbHits,elapsedMicros`。`PROFILE` 额外返回 `summary.profile.operators`，每项 `id,operator,rows,dbHits`；它会执行查询。`EXPLAIN` 不执行图 mutation，也不构建索引缓存。正常耗时与 dbHits 随数据、缓存、查询而变，不把示例数字当性能保证。

## SQL 显式事务

三个 lifecycle function 在同一 SQLite connection 上管理 Engine-owned transaction；它们不提供第三种 Cypher execution surface。不要额外包一层 SQL `BEGIN/COMMIT`：

```sql
SELECT lithograph_init();
SELECT lithograph_tx_begin('{"author":"demo","message":"create people"}');
SELECT lithograph('CREATE (:Person {name: ''张三''}) FINISH');
SELECT lithograph('CREATE (:Person {name: ''李四''}) FINISH');
SELECT lithograph(
  'MATCH (p:Person) RETURN p.name AS name ORDER BY name'
);
SELECT lithograph_tx_commit();
```

`tx_begin` 内部进入 SQLite transaction；其后的普通 `lithograph()` / `lithograph_rows()` 自动共享 staged state，staged execution 的 `summary.commit` 为 `null`；只有 `tx_commit` 生成并返回最终 graph Commit，然后提交 SQLite transaction。取消时调用：

```sql
SELECT lithograph_tx_begin('{}');
SELECT lithograph('CREATE (:Discarded) FINISH');
SELECT lithograph_tx_abort();
```

`tx_begin` 只接受一个 JSON object TEXT：`branch`、`expectedHead`、`author`、`message`。active transaction 内普通 execution 不能用 `branch/at/author/message/mergeSession` 切换 version context；`graphView` 仍可按 execution 指定。`NULL`、非 TEXT、malformed JSON、未知 option 与空 query 均是 `INVALID_ARGUMENT`。

任一 staged `lithograph()` failure/interrupt，或 `lithograph_rows()` 在 success summary 前被取消/关闭，都会自动 rollback 整个 explicit transaction，包括先前成功的 staged writes；之后 commit/abort 会报“no active transaction”。connection 在 active transaction 期间关闭时，SQLite 回滚未提交内容。

已处于 caller-owned SQLite transaction 时 `tx_begin` 返回 `TRANSACTION_BOUNDARY_REQUIRED`。纯读 transaction 的 commit 返回 base Commit 且不创建新 Commit；只要执行过 mutating query，即使最终净变化为空，也会生成一个 empty-delta Commit。详见[事务指南](../guide/transactions.md)。

## Rows execution stream

`lithograph_rows()` visible columns 固定为 `ordinal INTEGER, event TEXT, data TEXT`，另有 hidden `query/params/options` inputs。每个 scan 固定产生：

```text
0       columns  ["name", ...]
1..N    row      [<Lithograph JSON values>...]
N + 1   summary  {<summary object>}
```

零 row 仍有 `columns -> summary`；没有 RETURN/YIELD 的 statement 为 `columns=[] -> summary`。stream 可以执行 mutation、LOAD CSV、Managed Semantic 与 transaction subquery；external I/O 本身不是 rows adapter 拒绝理由。普通 side-effecting execution 在 success summary 前 early-close 会 rollback 未发布 state；已经 durable 的 transaction-subquery earlier batches 保留。

外层 SQL 的 `WHERE event=...` / `LIMIT` 只改变消费，不是 Cypher execution option。尤其 `LIMIT 1` 会在 columns 后关闭 cursor，因此不能用外层 SQL LIMIT 表达 Cypher top-k。

## 错误与事务

SQL 错误文本是 `LITHOGRAPH_<CATEGORY>: <message>`，保留 SQLite primary result code。不会把失败包装成成功 envelope 的 `error` 字段。详见 [Errors](errors.md)。

普通 mutating execution 使用可 rollback boundary。外层 SQL BEGIN/COMMIT 仍由宿主掌握；多次普通调用不会因此变成一个图 Commit。只有显式 `lithograph_tx_*` lifecycle 使用 Engine-owned transaction 组合一个 Commit。`CALL { ... } IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 可在普通 SQLite autocommit execution 中直接运行；caller-owned SQLite transaction 或 active Lithograph explicit transaction 中返回 `TRANSACTION_BOUNDARY_REQUIRED`。

依据：[公开 SQL 实现](../../crates/lithograph-extension/src/scalar.rs)、[结果适配](../../crates/lithograph-extension/src/execution.rs)、[SQL Bridge](../design/interfaces.md#sql-bridge)。
