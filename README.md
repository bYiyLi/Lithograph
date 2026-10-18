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

本版本为 **v0.3.3**，目标发行平台为 Linux/macOS/Windows x64/arm64。运行时需要 SQLite 3.45.0+、loadable extension、FTS5 和线程安全支持。标准发行包包含主 extension、OpenAI-compatible Provider、许可证和 VERSION，并通过 SHA256SUMS 校验；实际发布进度见 [Phase 19](https://github.com/bYiyLi/Lithograph/blob/main/docs/development/phases/19-query-correctness-resources.md#v033-release-delivery)。

正式发布后，从 [v0.3.3 Release](https://github.com/bYiyLi/Lithograph/releases/tag/v0.3.3) 选择平台资产，并按 [安装指南](https://github.com/bYiyLi/Lithograph/blob/v0.3.3/docs/guide/installation.md) 校验加载。源码构建使用 Rust 1.98.1：

```sh
cargo build --locked --release -p lithograph-extension -p lithograph-openai-compatible
```

在 SQLite CLI 中加载当前平台的 shared library，再初始化：

```text
.load /absolute/path/lithograph
SELECT lithograph_init();
```

```sql
SELECT lithograph('CREATE (:Person {name: ''Alice''}) FINISH');
SELECT ordinal,event,data
FROM lithograph_rows('MATCH (p:Person) RETURN p.name AS name');
```

`.load` 注册接口，`lithograph_init()` 初始化或迁移当前 main database。应用通过标准 SQLite SQL 调用；`lithograph()` 返回完整结果，`lithograph_rows()` 以 columns → row* → summary 增量执行读写。多个 execution 可以通过 SQL explicit transaction 共同形成一个 graph Commit。

v0.3.3 修复静态名称的反引号语义和长表达式取消，增加宿主显式启用的 `memoryLimitBytes` 执行内存预算。既有 `readOnlyNoExternalIO`、`readAccess` / `writeAccess` 继续可用。语言 Profile 为 `CY25-2026.08`，fresh/current storage format 为 3，Provider SPI 为 V1。完整变化与历史 format 4 边界见 [发布说明](https://github.com/bYiyLi/Lithograph/blob/v0.3.3/docs/releases/v0.3.3.md)。

## 开发者文档

| 任务 | 固定版本入口 |
| --- | --- |
| 安装、快速入门、集成、事务、Graph View、安全与运维 | [完整产品手册](https://github.com/bYiyLi/Lithograph/blob/v0.3.3/docs/guide/README.md) |
| 完整 Cypher 语法、类型、表达式、路径、修改、Schema、搜索与函数 | [Cypher 25 使用手册](https://github.com/bYiyLi/Lithograph/blob/v0.3.3/docs/cypher/index.md) |
| SQL/options/JSON、Procedure、Provider、错误与限制 | [接口参考](https://github.com/bYiyLi/Lithograph/blob/v0.3.3/docs/reference/README.md) |
| Python/C 和真实执行验证 | [可运行示例](https://github.com/bYiyLi/Lithograph/blob/v0.3.3/docs/guide/examples/README.md) |

这些固定入口随 v0.3.3 tag 提供。同仓库维护的 [设计](https://github.com/bYiyLi/Lithograph/blob/v0.3.3/docs/design.md) 和 [开发计划](https://github.com/bYiyLi/Lithograph/blob/main/docs/development/README.md) 分别记录产品合同与验收证据。用户手册包含正常使用所需说明；已知问题与适用版本在 Reference 中明确列出。

## 项目状态

v0.3.3 已正式发布；tag CI、六平台 Matrix、正式下载资产审计与 macOS arm64 制品回验均已通过，精确源码、tag 与交付证据按 [Phase 19 发布交付记录](https://github.com/bYiyLi/Lithograph/blob/main/docs/development/phases/19-query-correctness-resources.md#v033-release-delivery) 分别记录实际结果。版本变化见 [发布说明](https://github.com/bYiyLi/Lithograph/blob/v0.3.3/docs/releases/v0.3.3.md)，历史 Release Notes 保留对应版本事实。

## 许可

Lithograph 采用双许可模式：

- **AGPL-3.0-only**：本仓库代码默认依据 [GNU Affero General Public License v3.0](LICENSE) 提供。AGPL 允许商业使用，但使用者必须遵守其开源义务。
- **Commercial License**：无法或不希望按 AGPL 使用 Lithograph 的组织，可以申请独立商业许可；商业条款仅通过单独书面协议授予，详见 [COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md)。

第三方依赖、测试数据或 vendored material 可以保留其各自许可证与 NOTICE；对应目录或文件的明确许可证优先。

## 贡献

当前欢迎通过 GitHub Issues 提交 bug、设计反馈和兼容性问题。为保持 AGPL + Commercial License 双许可能力，在 contributor licensing policy 正式建立前暂不接受外部代码或文档 Pull Request，详见 [CONTRIBUTING.md](CONTRIBUTING.md)。
