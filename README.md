# Lithograph

Lithograph 为 SQLite 提供版本化 Property Graph 数据库能力。

它以标准 SQLite loadable extension 的形式运行：保留 SQLite 的嵌入式、单文件部署体验，同时使用 Cypher 25 查询图数据，并使用 immutable Commit DAG、Branch 与 Tag 管理同一张图的状态演进。Git / TerminusDB 是版本机制参考，不限定上层如何解释这些状态。

## 核心能力

- **Property Graph**：Node、Relationship、Label、Relationship Type 与 Property。
- **Cypher 25**：图查询、数据修改、路径、Schema、Constraint、Index 与现代 Cypher 类型系统。
- **版本化状态管理**：immutable Commit、Branch、Tag、可修改 Commit Data、显式 empty-delta Commit、可分页历史查询、Time-travel、结构化 Diff/Patch、Merge、Rebase、Squash、Reset 与 Revert。
- **全文与向量搜索**：Full-text Index、Raw Vector Index、Cypher 25 `SEARCH`，以及 Managed Semantic / Embedding Provider。
- **SQLite 原生部署**：作为 loadable extension 使用同一个 SQLite database file，不需要独立数据库 Server。
- **大规模单机图**：版本感知存储、索引化邻接访问、流式查询执行与 checkpointed history 面向大规模本地图数据设计。

## 安装与 Quickstart

当前正式版本是 **v0.2.1**。GitHub Releases 提供 Linux x64/arm64、macOS x64/arm64、Windows x64/arm64 六个平台的预编译 extension；运行时需要支持 loadable extension 与 FTS5 的 SQLite 3.45.0 或更高版本。

v0.2.1 在 v0.2.0 Managed Semantic / storage format 4 基线上发布 Phase 14 SQL Explicit Transaction Adapter：普通 SQLite driver 可通过四个 `lithograph_tx_*` SQL function 把多次 Cypher execution 组合为一个 graph Commit，无需自行绑定 C API。每个平台包继续同时包含 Lithograph 主 extension 与 OpenAI-compatible Provider extension；见 [v0.2.1 Release Notes](docs/releases/v0.2.1.md)、[事务指南](docs/guide/transactions.md) 和 [SQL API Reference](docs/reference/sql-api.md)。

基础接入从 [Developer Documentation](docs/guide/README.md) 开始。正式 v0.2.1 Release 仍保持其当时的 Native ABI 1 与 storage format 4；**当前仓库 `main` 已进入 Phase 15 未发布开发基线**，application execution 已收敛为 SQL-only，fresh/current storage format 为 3，persistent embedding cache 由 Provider 自己拥有。复现 v0.2.1 时使用对应 tag/Release Notes，不把当前 Guide 反套到旧二进制。

稳定下载地址：

| 平台 | Release Asset |
| --- | --- |
| Linux x64 | `https://github.com/bYiyLi/Lithograph/releases/latest/download/lithograph-linux-x64.tar.gz` |
| Linux arm64 | `https://github.com/bYiyLi/Lithograph/releases/latest/download/lithograph-linux-arm64.tar.gz` |
| macOS x64 | `https://github.com/bYiyLi/Lithograph/releases/latest/download/lithograph-macos-x64.tar.gz` |
| macOS arm64 | `https://github.com/bYiyLi/Lithograph/releases/latest/download/lithograph-macos-arm64.tar.gz` |
| Windows x64 | `https://github.com/bYiyLi/Lithograph/releases/latest/download/lithograph-windows-x64.zip` |
| Windows arm64 | `https://github.com/bYiyLi/Lithograph/releases/latest/download/lithograph-windows-arm64.zip` |

每个包包含对应平台的 `lithograph.so` / `lithograph.dylib` / `lithograph.dll`、`lithograph-openai-compatible.so` / `.dylib` / `.dll`、`README.md`、`LICENSE`、`COMMERCIAL-LICENSE.md` 与 `VERSION`。Release 同时提供 `SHA256SUMS` 用于校验下载内容。

如果需要从源码构建，需要 Rust 1.98.1：

先构建 release extension：

```sh
cargo build --locked --release -p lithograph-extension
```

Cargo 生成的 shared library 位于 `target/release/`：Linux 为 `liblithograph.so`，macOS 为 `liblithograph.dylib`，Windows 为 `lithograph.dll`。下面以 SQLite CLI 为例，先打开一个 database：

```sh
sqlite3 lithograph-demo.sqlite
```

进入 SQLite CLI 后，`<extension>` 替换为当前平台的实际文件路径：

```text
.load <extension>
SELECT lithograph_init();
```

`.load` 只注册 Lithograph API；`lithograph_init()` 才会在当前 SQLite database 中初始化或迁移 Lithograph storage，并建立空图的 Root Commit 与默认 `main` Branch。

通过 `lithograph()` 执行会修改图状态的 Cypher：

```sql
SELECT lithograph('CREATE (:Person {name: ''Alice''}) FINISH');
```

大结果或需要增量消费时，使用流式的 `lithograph_rows()`；它与 scalar 共享同一 execution 能力，也可以执行 mutation、LOAD CSV、Managed Semantic 与 transaction subquery：

```sql
SELECT ordinal, event, data
FROM lithograph_rows('MATCH (p:Person) RETURN p.name');
```

正式 Release Matrix 覆盖 Linux x64/arm64、macOS x64/arm64、Windows x64/arm64；当前 Phase 15 开发基线在发布前仍需重新跑 hosted matrix。运行时 minimum 继续是 SQLite 3.45.0，并在 release-current SQLite 上复验。其它 host application 需要在目标 SQLite connection 上启用 loadable extension，并通过 SQLite 官方 extension-loading API 加载同一个 shared library。

同一张图可以在不同 Commit 上查询，也可以在不同 Branch 上独立演化，而不需要复制 SQLite 数据库文件。

## 开发者文档

完整入口：**[Lithograph Developer Documentation](docs/guide/README.md)**。当前 Guide/Reference 描述 Phase 15 未发布开发基线；历史 Release Notes、Phase 文档与 tag 继续保留对应版本事实。

| 任务 | 文档 |
| --- | --- |
| 安装并完成第一次图查询与历史读取 | [安装](docs/guide/installation.md) · [快速入门](docs/guide/getting-started.md) |
| 数据建模、约束、索引与检索 | [Graph/Cypher](docs/guide/graph-and-cypher.md) · [Schema/Index](docs/guide/schema-and-indexes.md) · [Search/CSV](docs/guide/search.md) |
| 版本控制与逐步解决合并冲突 | [版本管理](docs/guide/versioning.md) · [Merge Session](docs/guide/merge.md) |
| Python/Native 接入、事务与子图边界 | [应用集成](docs/guide/integration.md) · [事务](docs/guide/transactions.md) · [Graph View](docs/guide/graph-views.md) |
| 参数、返回值、函数、Procedure 与兼容范围 | [API Reference](docs/reference/README.md) |
| 备份、升级、性能诊断与错误恢复 | [部署维护](docs/guide/operations.md) · [排障](docs/guide/troubleshooting.md) |

可执行示例和实际验证范围见 [Examples](docs/guide/examples/README.md)。文档验证发现的 v0.1.0 adapter / introspection 差异及规避方法见 [Known Issues](docs/reference/known-issues.md)；公开接口登记与 release gate 通过不表示所有边界都没有缺陷。

## 项目状态

Lithograph 正式发布版本仍为 **v0.2.1**；当前仓库开发阶段是 **Phase 15 — SQL Execution Surface + Provider-owned Cache**。Phase 15 的本地实现、resource/quality/CI 与文档 review 已完成 SQL-only application surface、side-effect-capable `lithograph_rows()`、Native query ABI / `lithograph_tx_execute` 删除、format3 回归与 Provider-owned persistent embedding cache；唯一仍 pending 的 acceptance 是 commit/push 后才能取得真实证据的六目标 hosted Release Matrix。

v0.2.1 仍属于 pre-1.0 版本；其 format 4 / Native ABI 行为见 [v0.2.1 Release Notes](docs/releases/v0.2.1.md)。当前 Phase 15 开发基线的 fresh/current format 为 3；升级旧正式版本前必须先看对应 Phase 15 migration/release 说明，不按旧文档自行修改 storage marker。

- [技术设计](docs/design.md)
- [开发计划](docs/development/README.md)
- [Phase 11 性能优化计划](docs/development/phases/11-performance-optimization.md)
- [Phase 12 全文 Tokenizer 扩展计划](docs/development/phases/12-fulltext-tokenizer.md)
- [Phase 13 Managed Semantic Vector / Embedding Provider](docs/development/phases/13-managed-semantic-vector.md)
- [Phase 14 SQL Explicit Transaction Adapter](docs/development/phases/14-sql-explicit-transaction.md)
- [Phase 15 SQL Execution Surface + Provider-owned Cache](docs/development/phases/15-sql-execution-provider-cache.md)
- 当前开发阶段：Phase 00–14 已完成；Phase 15 `in_progress`。

## 许可

Lithograph 采用双许可模式：

- **AGPL-3.0-only**：本仓库代码默认依据 [GNU Affero General Public License v3.0](LICENSE) 提供。AGPL 允许商业使用，但使用者必须遵守其开源义务。
- **Commercial License**：无法或不希望按 AGPL 使用 Lithograph 的组织，可以申请独立商业许可；商业条款仅通过单独书面协议授予，详见 [COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md)。

第三方依赖、测试数据或 vendored material 可以保留其各自许可证与 NOTICE；对应目录或文件的明确许可证优先。

## 贡献

当前欢迎通过 GitHub Issues 提交 bug、设计反馈和兼容性问题。为保持 AGPL + Commercial License 双许可能力，在 contributor licensing policy 正式建立前暂不接受外部代码或文档 Pull Request，详见 [CONTRIBUTING.md](CONTRIBUTING.md)。
