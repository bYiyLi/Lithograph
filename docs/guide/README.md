# Lithograph Developer Documentation

这套文档面向把 Lithograph 嵌入自己产品的应用开发者。当前正文描述 **v0.3.0 正式发布基线**；旧版本历史行为见 `docs/releases/` 与对应 tag，不要用旧 Release Notes 反推当前接口。

## v0.3.0 基线

| 项目 | 当前仓库行为 |
| --- | --- |
| Cypher compatibility profile | `CY25-2026.08` |
| Application execution surface | SQLite SQL-only：`lithograph()` / `lithograph_rows()` |
| Explicit transaction | `lithograph_tx_begin()` → 普通 execution → `lithograph_tx_commit()` / `lithograph_tx_abort()` |
| Application-facing query C ABI | 不提供 |
| Embedding Provider SPI | 保留 `EmbeddingProviderV1` |
| Lithograph storage format | `3` |
| Persistent embedding result cache | 由具体 Provider 自己拥有；OpenAI-compatible Provider 可使用独立 SQLite cache DB |
| SQLite | `3.45.0+`，支持 loadable extension、FTS5 |

Lithograph 是嵌入式 SQLite extension，不是独立 Server、Agent 框架或 embedding 服务。一个 SQLite `main` database 承载一个版本化 Property Graph；宿主可以同时拥有普通 SQL 表，但不得占用 `_lithograph_*` 保留命名空间。

## 阅读路径

第一次接入：**[安装](installation.md) → [快速入门](getting-started.md) → [应用集成](integration.md)**。

| 使用目标 | 文档 |
| --- | --- |
| 创建/查询/修改图数据 | [Graph 与 Cypher](graph-and-cypher.md) |
| Schema、Constraint、Index | [Schema 与 Index](schema-and-indexes.md) |
| Full-text、Vector、Managed Semantic、LOAD CSV | [Search 与数据导入](search.md) |
| Commit / Branch / Tag / Diff / Patch / Rebase / Reset / Revert | [版本管理](versioning.md) |
| Merge Session | [Merge](merge.md) |
| SQL 外层事务与多 execution 单 Commit | [事务与并发](transactions.md) |
| Graph View | [Graph View](graph-views.md) |
| 备份、完整性、GC、Provider cache | [部署与维护](operations.md) |
| 精确接口 | [Reference](../reference/README.md) |

## Execution 选择

- 小/中等结果：`SELECT lithograph(...)`，得到完整 `{columns,rows,summary}` JSON envelope。
- 大结果或需要增量消费：`lithograph_rows(...)`，事件固定为 `columns -> row* -> summary`。
- 多条标准 Cypher execution 需要最终只产生一个 Commit：同一 connection 上先 `lithograph_tx_begin()`，期间继续调用正常 `lithograph()` / `lithograph_rows()`，最后 `commit` 或 `abort`。
- `CALL { ... } IN TRANSACTIONS` 直接通过普通 SQL execution surface 在 SQLite autocommit mode 下执行。

`lithograph_rows()` 是 execution stream，不是只读 adapter。它可以执行 mutation、LOAD CSV、Managed Semantic 和 transaction subquery；未到 success `summary` 就关闭的普通 side-effecting stream 会 rollback 未发布的当前 execution。不要用外层 SQL `LIMIT` 代替 Cypher 内的语义 limit。

## 示例

当前可执行示例使用 SQLite SQL surface：[Python 快速入门](examples/python_quickstart.py)、[版本工作流](examples/version_workflow.py)、[SQL 单 Commit 事务](examples/sql_transaction.py)。`native_transaction.c` 文件保留为 **SQLite C host 示例**，不调用 Lithograph application query ABI。

## 版本与历史

v0.2.0/v0.2.1 曾包含 format 4 / Lithograph-owned embedding cache 与 application Native query ABI；v0.3.0 已通过 Phase 15 替换这些合同。需要复现旧 Release 时使用对应 tag 与 Release Notes，不把当前 Guide 当作旧二进制兼容说明。

许可见 [LICENSE](../../LICENSE) 与 [Commercial License](../../COMMERCIAL-LICENSE.md)。
