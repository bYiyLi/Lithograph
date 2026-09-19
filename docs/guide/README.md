# Lithograph Developer Documentation

这套文档面向把 Lithograph 嵌入自己产品的应用开发者。无需先阅读源码或内部设计，即可安装扩展、读写图、使用检索与版本管理，并处理事务、错误和数据库维护。

## 适用版本

| 项目 | 当前文档范围 |
| --- | --- |
| 当前 Release | **v0.1.1** |
| 基础接口 / 示例基线 | **v0.1.0**；未受 v0.1.1 影响的页面继续保留原版本标记 |
| v0.1.1 增量 | Full-text tokenizer： [Search](search.md)、[Procedure](../reference/procedures.md)、[Limits](../reference/limits.md)、[Release Notes](../releases/v0.1.1.md) |
| Unreleased `main` 增量 | Phase 13 Managed Semantic / Embedding Provider、storage format 4；尚未发布为新的 Release |
| Cypher compatibility profile | `CY25-2026.08` |
| Native ABI | `lithograph_v1_*`，ABI version `1` |
| 新数据库格式 | 正式 v0.1.1：`3`；Unreleased `main`：`4` |
| SQLite | `3.45.0+`，支持 loadable extension、FTS5 |
| 部署 | Linux、macOS、Windows，分别提供 x64 / arm64 制品 |

v0.1.1 没有改变 Native ABI、Cypher profile 或 storage format；因此未受 Full-text tokenizer 变更影响的 v0.1.0 指南与示例继续作为已验证基线。标记为 v0.1.0 的可执行示例仍会检查精确版本，不应拿 v0.1.1 binary 强行通过这些旧版本断言。当前 `main` 的 Phase 13 页面明确标记为 Unreleased，不表示 `latest` Release 已经包含 Managed Semantic 或 format 4。

Lithograph 是嵌入式数据库扩展，不是独立 Server、Neo4j 客户端、Agent 框架或 embedding 服务。一个 SQLite connection 的 `main` database 承载一个版本化 Property Graph；同一文件仍可保存宿主自己的普通 SQL 表，但不得使用保留的 `_lithograph_*` 名称。

## 阅读路径

第一次接入：**[安装](installation.md) → [快速入门](getting-started.md) → [应用集成](integration.md)**。

| 使用目标 | 文档 |
| --- | --- |
| 创建节点和关系、查询、修改、删除 | [Graph 与 Cypher](graph-and-cypher.md) |
| 定义类型与约束、创建和查看索引 | [Schema 与 Index](schema-and-indexes.md) |
| 全文检索、Raw Vector、Unreleased Managed Semantic、导入 CSV | [Search 与数据导入](search.md) |
| Commit、Branch、Tag、历史、Diff/Patch、Rebase、Squash、Reset、Revert | [版本管理](versioning.md) |
| 分批解决冲突，检查候选图后再合并 | [Merge Session](merge.md) |
| 一次写入、SQL 外层事务、多次查询形成一个 Commit | [事务与并发](transactions.md) |
| 选择查询可见子图并约束写入 | [Graph View](graph-views.md) |
| 备份、恢复、迁移、GC、安全与性能 | [部署与维护](operations.md) |
| 从错误症状定位恢复步骤 | [排障](troubleshooting.md) |
| 查找准确接口、参数、结果、类型和限制 | [Reference](../reference/README.md) |

## 示例约定

`sql` 代码块可以交给加载了扩展的 SQLite connection。`cypher` 代码块是传给 `lithograph()` 或 Native API 的查询文本，**不能直接作为 SQLite 顶层语句执行**。每篇指南中的 SQL 示例在独立、空的演示数据库中按出现顺序运行；不要求先执行其他章节的数据写入。

安装时只下载与你的进程 OS / CPU 架构匹配的扩展。示例中的 `./lithograph.dylib` 是 macOS 文件名；Linux 改为 `.so`，Windows 改为 `.dll`。程序使用扩展的绝对路径。

[Python 快速入门](examples/python_quickstart.py)、[完整版本工作流](examples/version_workflow.py) 和 [Native 单 Commit 事务](examples/native_transaction.c) 提供可执行代码。它们使用独立演示数据库，不修改已有业务数据库。运行方法见 [应用集成](integration.md)。

## 必须先理解的边界

普通 graph / Schema / Index 写查询成功后已经形成 Commit，**没有待手动提交的 working tree**。多次 `SELECT lithograph(...)` 放入 SQL `BEGIN`，只合并落盘边界，不合并历史中的 Commit；多 execution 单 Commit 使用 Native explicit transaction。

Commit 的图状态和 metadata 不可修改，但 Commit Data 是可修改注释；Tag 也可显式移动。历史版本查询只读。Branch、Tag 和 Merge Session 均不是账号或权限边界，Graph View 也不是认证机制。

v0.1.1 仍是 pre-1.0 版本。固定版本、保留升级前备份，不把当前 API 等同于永久兼容承诺；Full-text analyzer 从 v0.1.0 升级到 v0.1.1 前先阅读对应 Release Notes。

## 文档依据与验证

本手册以 v0.1.0 发布行为作为基础验证集，并在明确标记的页面加入 v0.1.1 Full-text tokenizer 与当前 Unreleased Phase 13 增量，不把开发分支能力冒充为已发布版本。v0.1.1 历史事实依据 [v0.1.1 Release Notes](../releases/v0.1.1.md)；Unreleased Phase 13 依据当前 [技术设计](../design.md)、实现、Phase acceptance 与真实 SQLite gates。

本次验证的命令、平台和范围记录在 [示例验证说明](examples/README.md)。验证范围不等同于重新运行全部发布压测，也不代表所有第三方 SQLite binding 已逐一验证。

许可条款见 [LICENSE](../../LICENSE) 与 [Commercial License](../../COMMERCIAL-LICENSE.md)。问题反馈见 [CONTRIBUTING](../../CONTRIBUTING.md)。内部实现仍由 [开发计划](../development/README.md) 管理，不与本手册混用。
