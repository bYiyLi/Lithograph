# 排障与恢复路径

适用 v0.1.0。先记录实际应用进程中的 SQLite version、`lithograph_version()`、OS/进程架构、调用入口，以及完整的 category / SQLite code。不要先重复执行所有写请求，也不要直接编辑内部表。

## 安装与加载

| 症状 | 检查与处理 |
| --- | --- |
| Python 没有 `enable_load_extension` | 当前 Python SQLite 构建未提供 loading API；选择支持它的构建。安装另一个 SQLite CLI 不会改变这个 Python runtime |
| C 编译报 `sqlite3_load_extension` 未声明 | 实际使用了不支持该入口的 header，例如本轮 macOS SDK；同时指定支持 loading 的 SQLite include/lib 前缀，不只手写函数声明绕过 |
| 无法加载动态库、架构不匹配 | 检查完整路径、进程架构与制品、文件权限、依赖库和 checksum；macOS arm64 硬件上的 x64 进程仍需匹配进程架构 |
| SQLite 太旧或 FTS5 不可用 | 检查绑定实际链接的 SQLite，而不是另一个 shell 程序的版本 |
| `no such function: lithograph` | 扩展未加载到这个 connection；每个连接都需要加载一次 |
| `NOT_INITIALIZED` | `.load` 不创建图；由明确的初始化步骤调用 init，勿误打开空文件后当作原库 |

安装步骤见 [Installation](installation.md)。不要为解决加载问题关闭整机安全保护或加载来源不明的扩展。

## 查询、参数与结果

| 症状 | 检查与处理 |
| --- | --- |
| `PARSE_ERROR` / `SEMANTIC_ERROR` | Cypher 必须传给 Lithograph，不是直接作为 SQLite 顶层语句；检查 profile、作用域与 SQL 引号 |
| `INVALID_ARGUMENT` / JSON EOF | params/options 必须为 JSON object TEXT；Native v0.1.0–v0.2.0 空参数显式传 `"{}",2`，不能用 `NULL,0` |
| 数字精度丢失 | 在跨 JSON 边界前保留 signed 64-bit 大整数的 Integer tag，不先转成 float |
| `$type` 业务字段被识别为类型 | 用 Map wrapper 保存包含 `$type` 的普通业务 Map；不要删除数据库返回的合法 tags |
| 返回 rows 为空但发生了变化 | 写查询没有 RETURN / 使用 FINISH 可以成功返回空 rows；查看成功状态、summary.commit 和 counters |
| SHOW 描述的类型与实际结果不一致 | v0.1.0–v0.2.0 Procedure introspection 把 returnDescription 类型全部标成 STRING；按真实 JSON type/tag 与 Reference 解码，不据此强制转换 |
| 全文/向量索引不存在 | 核对当前 Branch / at 所看到的历史 Schema；索引名称存在于当前 main 不代表历史中已经创建 |
| `TYPE_ERROR` / `CONSTRAINT_ERROR` | 检查 query 参数、合法 Property 值、向量维度和已有约束；不要删除约束来掩盖数据问题 |
| 找不到预期节点 | 核对 databaseId、options.branch/at、Label 大小写、graphView，以及 mutation 是否最终提交 |

完整编码见 [Values](../reference/values.md)。把 Node / Relationship / Path 当作 params 传回会被拒绝；应使用业务键或 elementId 字符串重新查询。

## 事务与版本

| 症状 | 检查与处理 |
| --- | --- |
| `branch.checkout` 即使没有 BEGIN 也报事务边界错误 | v0.1.0–v0.2.0 已知 adapter 问题；使用每次执行的 `options.branch`，不要修改内部状态 |
| SQL BEGIN 内仍出现多个 Commit | 这是外层落盘边界，不是多次执行单 Commit；后者使用 Native explicit transaction |
| `lithograph_rows` 拒绝 CREATE / LOAD CSV | 该 adapter 只读且禁止 external I/O；选择 scalar 或普通 Native |
| `at` 写入失败 | historical Snapshot 只读；从所需版本创建 Branch 后写入 |
| Native tx_execute 失败后 tx_commit 返回 MISUSE | 之前的 execution 已自动 abort 整体；重新读取当前 head，重建一个新事务 |
| `IN TRANSACTIONS` 在 SQL 中被拒绝 | 该 query 需要普通 Native execute，connection 不能已有外层/Native explicit transaction |
| Merge candidate 被拒绝 | 检查 unresolved 是否为 0、revision 是否最新，以及是否混用了 branch/at/author/message |
| `MERGE_SESSION_CHANGED` | 重新 get/conflicts 并检查新 revision；旧 cursor/resolution submission 不可直接复用 |
| `BRANCH_HEAD_MOVED` | 当前状态已不同于你审查的基线；重新读取并重新审查，不盲目覆盖 |
| `VERSION_NOT_FOUND`，但日志里保存了 hash | 核对 databaseId；无保护引用的 Commit 可能已被 GC，hash 字符串本身不保留历史 |

发布差异的复现、源码证据与规避见 [Known Issues](../reference/known-issues.md)。文档没有把它们宣称为已修复。

## 性能、锁和文件

`BUSY` / `LOCKED`：检查长寿命 SQL/Native 事务、未关闭的流式游标、另一个进程的写入。不同 Branch 仍共享 writer。设置有上限的等待/退避，确认此前调用是否已经提交，再判断是否重试。

查询突然变慢：比较实际 Snapshot、Schema、输入规模与 cache 状态，执行只读 EXPLAIN。不要用带写入的 PROFILE 做健康检查，也不要把所有慢查询归结为缺少一个索引。完全缺缓存与已有持久索引 reopen 应分别测量。

`RESOURCE_ERROR`：同时查看 sqliteCode；INTERRUPT 是取消，TOOBIG 是长度限制，磁盘/内存不足则需恢复资源。将大结果改成必要字段投影与分页；流式查询也不能绕过单行限制。

`STORAGE_ERROR` / integrity 失败：停止写入，保存原文件与脱敏复现，在隔离副本上检查并恢复有效备份。`FORMAT_TOO_NEW`：使用相容扩展，不修改格式标记。不要删除正在使用的 WAL / SHM 或只复制热库主文件后认为已经备份。

## 报告问题

提供可复现的最小 query、参数结构、接口类型、版本/运行时、category/code，以及问题是否在全新库复现。涉及 Session 时包含 revision 和状态，涉及历史时说明 Branch/Tag/Commit 选择方式。

不要提交生产数据库、凭据、完整私有文档、secret URL 或原始 embedding。先用合成数据复现；提交渠道与贡献流程见 [CONTRIBUTING](../../CONTRIBUTING.md)。
