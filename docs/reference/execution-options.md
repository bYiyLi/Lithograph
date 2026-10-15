# Execution Options

适用版本：**v0.3.1** · [产品手册](../guide/README.md) · [接口参考](README.md)

options 是 `lithograph()` / `lithograph_rows()` 第三个参数的 JSON object TEXT；省略等价于 `'{}'`。SQL `NULL`、JSON `null`、数组、malformed JSON、未知 key 或 key 的错误大小写均返回 `INVALID_ARGUMENT`。params 与 options 独立：查询里的 `$name` 从第二个参数读取，options 不用于参数绑定。

<a id="ordinary-execution"></a>

## 普通 execution

| Key | 类型与默认 | 语义 |
| --- | --- | --- |
| `branch` | 非空 STRING；默认 connection active Branch，初始为 `main` | 本次执行的 Branch **name**，不带 `branch/`；不改变 checkout |
| `at` | 非空 STRING；默认无 override | `commit/<id>`、`branch/<name>`、`tag/<name>` Descriptor；开始时解析并 pin 只读 Snapshot |
| `author` | STRING 或 null；默认 null | 新 Commit 的 author，不是账号认证 |
| `message` | STRING 或 null；默认 null | 新 Commit 的 message |
| `readOnlyNoExternalIO` | BOOLEAN；默认 `false` | 同时禁止本次 Cypher 的 mutation 和外部 I/O，完整规则见下节 |
| `graphView` | object；默认完整图 | `{requireAllLabels:[STRING],excludeAnyLabels:[STRING]}` |
| `mergeSession` | object；默认无 | `{id:"merge-session/<id>",revision:INTEGER}`，revision 为正整数 |

`branch` 与 `at` 互斥。`mergeSession` 与 `branch` / `at` / `author` / `message` 互斥，包括显式给出的 null metadata；它可以与 `graphView`、`readOnlyNoExternalIO` 组合。`branch` / `at` / `graphView` / `mergeSession` 的显式 null 均不表示省略。

指定 `at` 时，即使 Descriptor 是当前 Branch head，写入仍报 `READ_ONLY_SNAPSHOT`。candidate 只能做允许的只读检查，未解决冲突报 `MERGE_CONFLICT`，revision 不匹配报 `MERGE_SESSION_CHANGED`。Descriptor 与 Session 共享结构见 [Versioning Reference](versioning.md)。

<a id="read-only-no-external-io"></a>

## `readOnlyNoExternalIO`

`true` 在 prepare 阶段检查完整的、已通过 semantic validation 的 AST，拒绝不允许的操作，错误 category 为 `INVALID_ARGUMENT`。`false` 或省略保留普通行为。`null`、`0`、`1`、字符串 `"true"` 都不是 Boolean。

| 操作 | `true` 时的行为 |
| --- | --- |
| 普通 MATCH、投影、聚合、只读子查询、语言组合 | 允许；仍受正常语义、Snapshot 和资源限制 |
| `at` 历史读取、Graph View 内读取、current-graph Schema/Index `SHOW` | 允许 |
| Raw Vector `SEARCH` | 允许 |
| 下表列出的只读 Procedure | 允许 |
| Graph / Schema / Index 写入与 version/ref/checkout/maintenance mutation | 拒绝 |
| `CALL ... IN TRANSACTIONS` / concurrent variant | 拒绝，即使 inner query 只读 |
| `LOAD CSV` | 拒绝，包含本地文件、HTTP、参数 URI、零行路径 |
| Managed Semantic create/query/rebuild | 拒绝，即使 Provider cache 已 warm |
| Full-text query Procedure | 拒绝，因为它可调用宿主注册的 tokenizer |

本版本允许的命名 Procedure：

```text
db.labels                        db.propertyKeys
db.relationshipTypes             lithograph.branch.list
lithograph.commit.get            lithograph.tag.list
lithograph.log                   lithograph.diff
lithograph.merge.get             lithograph.merge.list
lithograph.merge.conflicts
```

白名单以 Procedure 名称分类，不能只依据 introspection 的 `mode=READ` 推断允许。其它 Procedure 即使结果只读，也必须经过分类后才能用于该选项。

检查遍历嵌套 CALL、EXISTS/COUNT/COLLECT、WHEN/NEXT/UNION 等完整结构；未进入的分支、空 UNWIND、运行时参数以及 `EXPLAIN` / `PROFILE` 均不能豁免。例如 `EXPLAIN CREATE (:Person)` 仍被拒绝，而不是因 EXPLAIN 不执行而获准。语法本身不合法时可先返回 `PARSE_ERROR` / `SEMANTIC_ERROR`。

这是 query-local 限制，不会改变 Branch checkout 或给连接安装永久只读属性。它允许当前 SQLite database 的正常读取，以及 SQLite 的 TEMP/spill 和引擎内部派生工作；不能替代 SQL 权限控制或 OS sandbox。它不控制同一连接上的其它 SQL、`lithograph_init()` 或单独调用的 transaction lifecycle API。接收不可信 Cypher 的宿主必须自行构造 options、强制为 `true`，并限制用户对原始 SQL surface 的访问。实际流程见 [安全执行指南](../guide/safe-execution.md)。

<a id="graph-view"></a>

## `graphView`

两个数组可以各自省略，`{}` 等价完整图。字段只能为 `requireAllLabels`、`excludeAnyLabels`，值只能为 STRING 数组；未知字段、null、非字符串元素返回 `INVALID_ARGUMENT`。

- `requireAllLabels`：Node 必须同时拥有全部 Label。
- `excludeAnyLabels`：命中任意 Label 即不可见。
- 每个数组按精确名称的集合语义去重，不做 case folding 或 Unicode normalization；同一 Label 同时 required/excluded 为非法 selector。
- 不存在的 required Label 使当前 Node 均不可见；不存在的 excluded Label 无效果。解析 selector 不创建 Label。
- Relationship 仅在两端 Node 均可见时可见；没有独立 Relationship selector。
- 后续 clause 按前面 staged writes 后的 graph state 重新计算 visibility。写入越界报 `GRAPH_VIEW_VIOLATION`。

非空 selector 限定 graph-data 查询、修改和 Search；Schema/Constraint/Index definition 和 Version Procedure 不变成 view-local 操作，与非空 Graph View 组合返回 `INVALID_ARGUMENT`。Graph View 不持久化、不命名、不进入 Commit hash，也不是账号权限。完整用法见 [Graph View 指南](../guide/graph-views.md)。

<a id="version-procedure-options"></a>

## Version procedure 的目标与 metadata

| Procedure 类别 | `branch` / author / message |
| --- | --- |
| `commit.create`、`patch.apply`、`squash`、`reset`、`revert` | 目标可由 query branch 指定；创建 Commit 时使用新 metadata |
| `merge.start` | 接受 query branch 确定 target；不接受 author/message，不创建 Commit |
| `merge.finalize` | Session 已固定 target，不接受 branch；接受 author/message，只有实际新建 Commit 才保存 |
| `merge.get/list/conflicts/resolve/abort` | Session 自带目标；不接受 branch/author/message |
| `branch.*`、`tag.*`、`commit.data.*` | 参数或 connection 已指定目标，不接受 query branch |
| `rebase` | 接受 query branch；重写保留各旧 Commit 的 author/message，不能用 execution metadata 覆盖 |
| `index.rebuild` | 只使用 name/version 参数；拒绝 branch/at/author/message/graphView 等不适用上下文 |

Version mutation 与 `at` 组合报 `READ_ONLY_SNAPSHOT`。不要把同一份 options 无差别传给所有 Procedure；精确接口见 [Procedure Reference](procedures.md)。

<a id="transaction-begin-options"></a>

## `lithograph_tx_begin()` options

这是单独的 transaction-level object，仅允许：

| Key | 类型与默认 | 说明 |
| --- | --- | --- |
| `branch` | 非空 STRING；默认 active Branch | 事务开始后目标固定 |
| `expectedHead` | `commit/<id>` STRING；默认不检查 | 取得 writer 后比较目标 head；不一致返回 `BRANCH_HEAD_MOVED`，事务不开始 |
| `author` | STRING 或 null；默认 null | 最终唯一 Commit metadata |
| `message` | STRING 或 null；默认 null | 最终唯一 Commit metadata |

begin 不接受 `at`、`graphView`、`mergeSession`、`readOnlyNoExternalIO`；要限制每次 query，在 execution options 中传入。完整生命周期见 [Transactions Reference](transactions.md)。

<a id="active-transaction-options"></a>

## Active explicit transaction 中的 execution options

begin 后继续调用普通 `lithograph()` / `lithograph_rows()`。此时仅接受 `graphView`、`readOnlyNoExternalIO`，默认分别为完整图、`false`。它们作用于当前 staged state，能看到同一事务内前面成功的 execution。

`branch` / `at` / `expectedHead` / `author` / `message` / `mergeSession` 和其它 key 均返回 `INVALID_ARGUMENT`；作为 active execution failure，它们也会自动 abort 整个 Lithograph explicit transaction。安全拒绝同样遵循这条 fail-closed 规则。v0.3.1 没有独立 `lithograph_tx_execute()`。

超时、SQL busy handler、线程策略和取消属于宿主 SQLite API；不存在隐含的 `timeout`、`readOnly`、`database` 或任意 Neo4j driver 配置。
