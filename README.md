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

Lithograph 当前已经完成产品与技术设计、开发规范和可执行开发计划，尚未开始 Engine 实现，也没有可用于生产的 Release。

- [技术设计](docs/design.md)
- [开发计划](docs/development/README.md)
