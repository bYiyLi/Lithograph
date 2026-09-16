# Execution Options

**版本：v0.1.0。** options 是 `lithograph()` / `lithograph_rows()` 第三个参数的 JSON object TEXT，也是普通 Native execute 的 options JSON。Key 大小写必须正确；未知 key 报 `INVALID_ARGUMENT`，没有隐含的 timeout、readOnly、database 或任意 Neo4j driver 配置。

## 普通 execution

| Key | 类型与默认 | 语义 |
| --- | --- | --- |
| `branch` | 非空 STRING；省略使用 connection active Branch，初始为 main | 本次执行的 Branch name，不带 `branch/` 前缀；不改变 checkout |
| `at` | 非空 STRING；省略不用 snapshot override | `commit/`、`branch/`、`tag/` Descriptor，固定只读 Snapshot |
| `author` | STRING 或 null；默认 null | 新 Commit author，不是账号认证 |
| `message` | STRING 或 null；默认 null | 新 Commit message |
| `graphView` | object；省略完整图 | `{requireAllLabels:[STRING],excludeAnyLabels:[STRING]}` |
| `mergeSession` | object；默认无 | `{id:"merge-session/<id>",revision:INTEGER}`，revision 为正整数 |

`branch` 与 `at` 互斥；`mergeSession` 与 `branch` / `at` / `author` / `message` 互斥。`mergeSession` 可以组合 graphView 做候选图只读检查。graphView 两个数组都可省略，省略等价空数组；相同 Label 同时 required/excluded 是非法 selector，未知字段/错误类型/显式 null 也不是默认值。

非空 graphView 限定图查询与图 mutation，不能把 Schema / version 操作变成 view-local 操作。`at` 下写入报 `READ_ONLY_SNAPSHOT`；candidate 不能 mutation，未解决冲突报 `MERGE_CONFLICT`，revision 改变报 `MERGE_SESSION_CHANGED`。

## Version procedure 的目标与 metadata

| Procedure 类别 | `branch` / author / message |
| --- | --- |
| `commit.create`、`patch.apply`、`squash`、`reset`、`revert` | 目标可由 query branch 指定；只有创建 Commit 的操作会使用新 metadata |
| `merge.start` | 接受 query branch 确定 target；不接受 author/message，不创建 Commit |
| `merge.finalize` | 目标已固定在 Session，不接受 branch；接受 author/message |
| `merge.get/list/conflicts/resolve/abort` | Session 自带目标；不接受 branch/author/message |
| `branch.*`、`tag.*`、`commit.data.*` | 参数已指定对象，不应另传 query branch；按对应 procedure 合同调用 |
| `rebase` | 接受 query branch；重写保留每个旧 Commit 的 author/message，不能靠 execution metadata 覆盖 |
| `index.rebuild` | 只用 name/version 参数；拒绝 branch/at/author/message/graphView 等不适用上下文 |

Version mutation 不能与 at 或非空 graphView 混用。不要把同一份 options 无差别附到所有 procedures，精确参数见 [Procedures](procedures.md)。

## Native tx_begin

仅允许 `branch`、`expectedHead`、`author`、`message`。Branch name 默认 active Branch；expectedHead 是 `commit/<id>`，在取得 writer 后比较，错误为 `BRANCH_HEAD_MOVED`，不创建 transaction。begin 不接受 at / graphView / mergeSession。

## Native tx_execute

仅允许本 statement 的 `graphView`。Branch、base、author/message 已在 begin 时确定，不接收 branch/at/expectedHead/author/message/mergeSession。每个 statement 重新在当前 staged state 上计算 view，能看到前面成功 statement 的写入。

参数与 options 不要混淆：查询里的 `$name` 从 params 取值，而不是 options。timeout、SQL busy handler、取消与线程策略属于宿主 SQLite API。

依据：[option parser](../../crates/lithograph-core/src/query/options.rs)、[Native ABI](../../include/lithograph.h)、[设计](../design.md)。
