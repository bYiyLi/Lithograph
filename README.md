# Lithograph

Lithograph 为 SQLite 提供版本化 Property Graph 数据库能力。

它以标准 SQLite loadable extension 的形式运行：保留 SQLite 的嵌入式、单文件部署体验，同时使用 Cypher 25 查询图数据，并使用 immutable Commit DAG、Branch 与 Tag 管理同一张图的状态演进。Git / TerminusDB 是版本机制参考，不限定上层如何解释这些状态。

## 核心能力

- **Property Graph**：Node、Relationship、Label、Relationship Type 与 Property。
- **Cypher 25**：图查询、数据修改、路径、Schema、Constraint、Index 与现代 Cypher 类型系统。
- **版本化状态管理**：immutable Commit、Branch、Tag、可修改 Commit Data、显式 empty-delta Commit、可分页历史查询、Time-travel、结构化 Diff/Patch、Merge、Rebase、Squash、Reset 与 Revert。
- **全文与向量搜索**：Full-text Index、Vector Index 与 Cypher 25 `SEARCH`。
- **SQLite 原生部署**：作为 loadable extension 使用同一个 SQLite database file，不需要独立数据库 Server。
- **大规模单机图**：版本感知存储、索引化邻接访问、流式查询执行与 checkpointed history 面向大规模本地图数据设计。

## 示例

```cypher
CREATE (alice:Person {name: 'Alice'})
CREATE (company:Company {name: 'OpenAI'})
CREATE (alice)-[:WORKS_AT]->(company)
```

```cypher
MATCH (person:Person)-[:WORKS_AT]->(company:Company)
RETURN person.name, company.name
```

同一张图可以在不同 Commit 上查询，也可以在不同 Branch 上独立演化，而不需要复制 SQLite 数据库文件。

## 项目状态

Lithograph 当前已经完成产品与技术设计、开发规范、可执行开发计划，以及 Phase 00–09。现有实现已具备标准 SQLite loadable-extension / Native ABI boundary、version-aware immutable storage、Graph View、Cypher 25 parser/type/value、真实 read planner/executor、mutation/Commit path、versioned Graph Type/Constraint、lookup/range/text/point/full-text/vector index、`SEARCH`、`LOAD CSV`、Cypher transaction batching，以及 Branch/Tag/Commit Data/Diff/Patch/Merge/Rebase/Squash/Reset/Revert/GC 等完整 Version operations。Phase 10 正在执行最终 compatibility、recovery、scale 与 cross-platform release hardening。目前没有可用于生产的 Release。

- [技术设计](docs/design.md)
- [开发计划](docs/development/README.md)
- 当前开发阶段：Phase 00–09 `done`，Phase 10 `in_progress`

## 许可

Lithograph 采用双许可模式：

- **AGPL-3.0-only**：本仓库代码默认依据 [GNU Affero General Public License v3.0](LICENSE) 提供。AGPL 允许商业使用，但使用者必须遵守其开源义务。
- **Commercial License**：无法或不希望按 AGPL 使用 Lithograph 的组织，可以申请独立商业许可；商业条款仅通过单独书面协议授予，详见 [COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md)。

第三方依赖、测试数据或 vendored material 可以保留其各自许可证与 NOTICE；对应目录或文件的明确许可证优先。

## 贡献

当前欢迎通过 GitHub Issues 提交 bug、设计反馈和兼容性问题。为保持 AGPL + Commercial License 双许可能力，在 contributor licensing policy 正式建立前暂不接受外部代码或文档 Pull Request，详见 [CONTRIBUTING.md](CONTRIBUTING.md)。
