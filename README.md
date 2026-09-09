# Lithograph

Lithograph 为 SQLite 提供版本化 Property Graph 数据库能力。

它以标准 SQLite loadable extension 的形式运行：保留 SQLite 的嵌入式、单文件部署体验，同时使用 Cypher 25 查询图数据，并使用类似 Git 的 Commit 与 Branch 管理图的完整历史。

## 核心能力

- **Property Graph**：Node、Relationship、Label、Relationship Type 与 Property。
- **Cypher 25**：图查询、数据修改、路径、Schema、Constraint、Index 与现代 Cypher 类型系统。
- **Git-like 版本管理**：immutable Commit、Branch、历史查询、Time-travel、结构化 Diff/Patch、Merge、Rebase、Squash、Reset 与 Revert。
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

Lithograph 当前已经完成产品与技术设计、开发规范、可执行开发计划和 Phase 00 Engineering Foundation。Rust workspace、CI、SQLite/test fixture、Cypher compatibility harness 与可重复 dependency/vendor integrity gate 已建立并通过最终验收；graph product behavior 从 Phase 01 开始实现，目前没有可用于生产的 Release。

- [技术设计](docs/design.md)
- [开发计划](docs/development/README.md)
- 当前开发阶段：Phase 00 `done`，Phase 01 `ready`

## 许可

Lithograph 采用双许可模式：

- **AGPL-3.0-only**：本仓库代码默认依据 [GNU Affero General Public License v3.0](LICENSE) 提供。AGPL 允许商业使用，但使用者必须遵守其开源义务。
- **Commercial License**：无法或不希望按 AGPL 使用 Lithograph 的组织，可以申请独立商业许可；商业条款仅通过单独书面协议授予，详见 [COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md)。

第三方依赖、测试数据或 vendored material 可以保留其各自许可证与 NOTICE；对应目录或文件的明确许可证优先。

## 贡献

当前欢迎通过 GitHub Issues 提交 bug、设计反馈和兼容性问题。为保持 AGPL + Commercial License 双许可能力，在 contributor licensing policy 正式建立前暂不接受外部代码或文档 Pull Request，详见 [CONTRIBUTING.md](CONTRIBUTING.md)。
