# Lithograph

Lithograph 为 SQLite 提供版本化 Property Graph 数据库能力。

它以标准 SQLite loadable extension 的形式运行：保留 SQLite 的嵌入式、单文件部署体验，同时使用 Cypher 25 查询图数据，并使用 immutable Commit DAG、Branch 与 Tag 管理同一张图的状态演进。Git / TerminusDB 是版本机制参考，不限定上层如何解释这些状态。

## 核心能力

- **Property Graph**：Node、Relationship、Label、Relationship Type 与 Property。
- **Cypher 25**：图查询、数据修改、路径、Schema、Constraint、Index 与现代 Cypher 类型系统。
- **版本化状态管理**：immutable Commit、Branch、Tag、可修改 Commit Data、显式 empty-delta Commit、可分页历史查询、Time-travel、结构化 Diff/Patch、Merge、Rebase、Squash、Reset 与 Revert。
- **全文与向量搜索**：Full-text Index、Raw Vector Index 与 Cypher 25 `SEARCH`；当前 Unreleased 开发分支另提供 Managed Semantic / Embedding Provider。
- **SQLite 原生部署**：作为 loadable extension 使用同一个 SQLite database file，不需要独立数据库 Server。
- **大规模单机图**：版本感知存储、索引化邻接访问、流式查询执行与 checkpointed history 面向大规模本地图数据设计。

## 安装与 Quickstart

当前正式版本是 **v0.1.1**。GitHub Releases 提供 Linux x64/arm64、macOS x64/arm64、Windows x64/arm64 六个平台的预编译 extension；运行时需要支持 loadable extension 与 FTS5 的 SQLite 3.45.0 或更高版本。

仓库当前 `main` 的 **Unreleased** 开发状态已经实现 Phase 13 Managed Semantic、独立 `lithograph-openai-compatible` Provider 与 storage format 4；这些能力尚未发布为新的 GitHub Release，不能把现有 v0.1.1 Release Asset 当作包含 Phase 13 Provider 的制品。源码使用与接口见 [Search 指南](docs/guide/search.md) 和 [Procedure Reference](docs/reference/procedures.md)。

基础接入从 [Developer Documentation](docs/guide/README.md) 开始。文档保留 v0.1.0/v0.1.1 的已发布基线，并把当前 Unreleased Phase 13 单独标注；v0.1.1 仍保持 Native ABI 1、`CY25-2026.08` 与 storage format 3。版本变化见 [CHANGELOG](CHANGELOG.md)、[v0.1.1 Release Notes](docs/releases/v0.1.1.md) 与 [Search 指南](docs/guide/search.md)；下面的 `latest` 地址会随未来 release 更新。

稳定下载地址：

| 平台 | Release Asset |
| --- | --- |
| Linux x64 | `https://github.com/bYiyLi/Lithograph/releases/latest/download/lithograph-linux-x64.tar.gz` |
| Linux arm64 | `https://github.com/bYiyLi/Lithograph/releases/latest/download/lithograph-linux-arm64.tar.gz` |
| macOS x64 | `https://github.com/bYiyLi/Lithograph/releases/latest/download/lithograph-macos-x64.tar.gz` |
| macOS arm64 | `https://github.com/bYiyLi/Lithograph/releases/latest/download/lithograph-macos-arm64.tar.gz` |
| Windows x64 | `https://github.com/bYiyLi/Lithograph/releases/latest/download/lithograph-windows-x64.zip` |
| Windows arm64 | `https://github.com/bYiyLi/Lithograph/releases/latest/download/lithograph-windows-arm64.zip` |

每个包包含对应平台的 `lithograph.so` / `lithograph.dylib` / `lithograph.dll`、`README.md`、`LICENSE`、`COMMERCIAL-LICENSE.md` 与 `VERSION`。Release 同时提供 `SHA256SUMS` 用于校验下载内容。

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

只读结果较大时，使用流式的 `lithograph_rows()`：

```sql
SELECT row
FROM lithograph_rows('MATCH (p:Person) RETURN p.name');
```

上述流程已由 release gate 在 Linux x64/arm64、macOS x64/arm64、Windows x64/arm64 六个平台验证；运行时 gate 覆盖 SQLite 3.45.0 minimum 与 3.53.4 release-current runtime。其它 host application 需要在目标 SQLite connection 上启用 loadable extension，并通过 SQLite 官方 extension-loading API 加载同一个 shared library。

同一张图可以在不同 Commit 上查询，也可以在不同 Branch 上独立演化，而不需要复制 SQLite 数据库文件。

## 开发者文档

完整入口：**[Lithograph Developer Documentation](docs/guide/README.md)**。文档明确区分 v0.1.0 基线与 v0.1.1 Full-text tokenizer 增量；不会把固定 v0.1.0 的示例、已知问题或历史证据静默改写成新版本事实。

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

Lithograph 当前已经完成 Phase 00–13 的实现与开发验收，当前发布版本为 **v0.1.1**。当前 `main` 的 Phase 13 实现已经闭合 Managed Semantic 主路径：公开 Embedding Provider ABI、OpenAI-compatible reference Provider、versioned Semantic Index、文本 query、persistent cache / rebuild、format 4 migration、Graph View / history / Diff-Patch-Merge-Rebase-Revert publication validation、SQL/Native transaction boundary、真实 SQLite 3.45.0 / 3.53.4，以及 Linux/macOS/Windows x64/arm64 六目标 hosted Release Matrix 均已通过。Phase 13 的开发状态为 `done`，但仍属于 Unreleased 能力；正式发布状态仍以 v0.1.1 为准。

v0.1.1 仍属于 pre-1.0 版本。升级已有数据库前应保留完整备份；format 3 没有自动 downgrade。Full-text analyzer 从 v0.1.0 到 v0.1.1 存在明确的配置行为变化，详见 [CHANGELOG](CHANGELOG.md) 与 [v0.1.1 Release Notes](docs/releases/v0.1.1.md)。

- [技术设计](docs/design.md)
- [开发计划](docs/development/README.md)
- [Phase 11 性能优化计划](docs/development/phases/11-performance-optimization.md)
- [Phase 12 全文 Tokenizer 扩展计划](docs/development/phases/12-fulltext-tokenizer.md)
- [Phase 13 Managed Semantic Vector / Embedding Provider](docs/development/phases/13-managed-semantic-vector.md)
- 当前开发阶段：Phase 00–13 全部 `done`；Phase 13 的 repository CI 与六目标 hosted Release Matrix 已通过，但尚未发布为新的 GitHub Release。

## 许可

Lithograph 采用双许可模式：

- **AGPL-3.0-only**：本仓库代码默认依据 [GNU Affero General Public License v3.0](LICENSE) 提供。AGPL 允许商业使用，但使用者必须遵守其开源义务。
- **Commercial License**：无法或不希望按 AGPL 使用 Lithograph 的组织，可以申请独立商业许可；商业条款仅通过单独书面协议授予，详见 [COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md)。

第三方依赖、测试数据或 vendored material 可以保留其各自许可证与 NOTICE；对应目录或文件的明确许可证优先。

## 贡献

当前欢迎通过 GitHub Issues 提交 bug、设计反馈和兼容性问题。为保持 AGPL + Commercial License 双许可能力，在 contributor licensing policy 正式建立前暂不接受外部代码或文档 Pull Request，详见 [CONTRIBUTING.md](CONTRIBUTING.md)。
