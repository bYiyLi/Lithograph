# Error Reference

**版本：v0.1.0。** SQL 异常前缀为 `LITHOGRAPH_<CATEGORY>:`；Native 通过返回码和 error JSON 的 category/message/sqliteCode/line/column 报错。不要只匹配英语 message。

| 稳定 category | 常见原因 | 调用方处理 |
| --- | --- | --- |
| `PARSE_ERROR` | Cypher 语法错误 | 修复 query，查看位置 |
| `SEMANTIC_ERROR` | 作用域、函数、模式或静态语义不合法 | 修复 query；不要当临时服务错误重试 |
| `TYPE_ERROR` | 参数/表达式/Property 的值类型不匹配 | 校验输入和编码 |
| `SCHEMA_ERROR` | Graph Type、DDL、Index 配置不合法 | 检查当前 Schema 与 frozen profile |
| `CONSTRAINT_ERROR` | 当前数据或候选状态违反约束 | 修改数据/迁移方案，不自动删除约束 |
| `NOT_INITIALIZED` | 尚未初始化 graph | 由 provisioning 步骤显式 init |
| `INVALID_ARGUMENT` | JSON、option、Descriptor、参数形状或调用状态不合法 | 修复调用；Native NULL JSON 见已知问题 |
| `GRAPH_VIEW_VIOLATION` | 写入越出 selector，或触及不可见依赖 | 在允许的完整边界内重建操作 |
| `VERSION_NOT_FOUND` | Commit/Descriptor 不存在或已被 GC | 核实 databaseId、保留引用和备份 |
| `BRANCH_NOT_FOUND` | Branch name 不存在 | 查 branch.list，不静默创建同名空分支 |
| `TAG_NOT_FOUND` | Tag 不存在 | 查 tag.list，核实环境 |
| `BRANCH_HEAD_MOVED` | CAS 预期 head 已改变 | 重新读取、重新决策，不盲目覆盖 |
| `MERGE_CONFLICT` | 未解决合并冲突或候选不满足要求 | 使用 Session conflict/resolve 流程 |
| `MERGE_SESSION_NOT_FOUND` | Session 已完成、abort 或 id 错误 | 重新读取状态，检查操作是否已成功 |
| `MERGE_SESSION_CHANGED` | revision/cursor 过期 | 重新 get/conflicts 并审查新 revision |
| `TRANSACTION_BOUNDARY_REQUIRED` | 调用入口/事务边界不支持该操作 | 选择正确 Native/SQL 入口；checkout 见已知问题 |
| `READ_ONLY_ADAPTER` | rows adapter 上执行写入/外部 I/O | 使用 scalar 或普通 Native；不绕过只读边界 |
| `READ_ONLY_SNAPSHOT` | at / 只读 Snapshot 上写入 | 从历史建立 Branch 后写入 |
| `FORMAT_TOO_NEW` | 文件格式高于当前扩展支持范围 | 使用相容新扩展，不改内部 format marker |
| `BUSY` | SQLite busy / locked | 关闭长事务/游标，采用有上限等待与退避 |
| `RESOURCE_ERROR` | 结果过大、内存/磁盘/资源不足或取消 | 结合 sqliteCode 区分限制与取消，缩小结果/负载 |
| `STORAGE_ERROR` | 格式、内部结构或 canonical 数据损坏/不一致 | 停止 mutation，检查原始副本并按备份恢复 |
| `IO_ERROR` | 文件、网络、打开、只读写入等 I/O 失败 | 检查权限、路径、空间与来源，不暴露凭据 |
| `INTERNAL_ERROR` | 内部失败、panic boundary 或清理失败 | 保留脱敏复现；清理失败时丢弃 connection |

## SQLite code 仍然重要

普通语法/参数/语义错误常见为 `SQLITE_ERROR` (1)；ABI 状态误用可为 `SQLITE_MISUSE` (21)。忙/锁分别可能是 `SQLITE_BUSY` (5) / `SQLITE_LOCKED` (6)。取消是 `SQLITE_INTERRUPT` (9)，在 Lithograph category 中归入 `RESOURCE_ERROR`，不是新的 `CANCELLED` category。

资源失败可能是 NOMEM (7)、TOOBIG (18)、FULL (13)；I/O 类还包括 READONLY 等 code。不要假定某个 category 永远只对应一个 code；使用实际返回值。

## 错误后的原子性

普通 mutating query 的失败不留下该 invocation 的部分图写入。Native explicit transaction 中任一 execution 失败会自动 abort 整体；不能继续 tx_execute/tx_commit。外层 SQL transaction 的后续处理仍由宿主决定。

独立 `IN TRANSACTIONS` 的已成功 batch 仍然持久化；错误不意味着整次导入没有副作用。已经向调用方发送的 ROW 也不等于整个操作成功，必须检查最终返回码/事务提交。

排障时记录版本、SQLite runtime、操作名、参数**形状**、category/code、Commit/Session revision。避免记录完整 credentials、私有文档、向量正文或含 token 的 URL。依据：[错误映射](../../crates/lithograph-extension/src/execution.rs)、[Native API](native-api.md)、[设计](../design.md)。
