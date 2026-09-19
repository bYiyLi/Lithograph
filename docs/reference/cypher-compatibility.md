# Cypher 25 Compatibility — v0.1.0

Lithograph v0.1.0 的语言基线是冻结的 **`CY25-2026.08`**：Cypher 25 current-graph surface，以及截至 2026.08 公开 additions；证据于 2026-09-09 冻结。它不是“任意未来 Cypher 25 文档页面上的所有内容”，也不是 Neo4j Server 的全部产品能力。

## 支持范围

| 能力族 | 包含的入口或语义 |
| --- | --- |
| 基本查询 | MATCH、OPTIONAL MATCH、WHERE/FILTER、RETURN/WITH/LET、ORDER BY/SKIP/LIMIT |
| 行与查询组合 | UNWIND/FOR、隐式/显式 GROUP BY、UNION、WHEN、NEXT、CALL subquery、EXISTS/COUNT/COLLECT expression subquery |
| Mutation | CREATE/INSERT、SET/REMOVE、DELETE/DETACH DELETE、MERGE 与 ON MATCH/ON CREATE、FOREACH |
| Pattern / Path | 方向、Label/Type expression、variable-length / quantified patterns、Match/Path modes、shortest / selector 组合 |
| 类型与表达式 | null、比较与排序、List/Map、Node/Relationship/Path runtime values、Temporal/Duration、Point、VECTOR、UUID、字符串插值 |
| Schema | open Graph Type、KEY/UNIQUE/existence/type constraints、LOOKUP/RANGE/TEXT/POINT/FULLTEXT/VECTOR indexes |
| Search | 全文 queryNodes/queryRelationships、VECTOR SEARCH、额外 Property filtering、SCORE、top-k |
| 导入与 batch | LOAD CSV、CALL IN TRANSACTIONS、IN CONCURRENT TRANSACTIONS / DISJOINT BY 的 profile 语义 |
| 诊断与发现 | EXPLAIN、PROFILE、SHOW FUNCTIONS/PROCEDURES/INDEXES/CONSTRAINTS/CURRENT GRAPH TYPE |

完整 clause/feature 验收清单由 [Compatibility Matrix](../development/cypher25-compatibility.md) 管理。本页是用户侧范围摘要，不另设第二套验收状态。每个输入仍需满足准确 syntax、类型、作用域、adapter 与事务限制。

[Function Reference](functions.md) 从 v0.1.0 真实 SHOW 输出生成，包含全部 172 条签名（含重载）；[Procedure Inventory](procedure-inventory.md) 保留 33 个 v0.1.0 历史签名，并补充 v0.2.0 的 8 个 Managed Semantic procedure。客户端可以通过 SHOW 自检，但不能因为名称登记存在就忽略 [Known Issues](known-issues.md)。

## 不属于此 compatibility profile

Neo4j DBMS 的 database/alias/server/cluster/user/role/privilege/auth 管理、system database 操作、Java UDF 部署 API，以及 `USE` / `graph.byName()` / `graph.names()` 的 composite-database graph selection 不在本产品合同内。

Lithograph 不提供 Bolt/HTTP 数据库服务器或 Neo4j driver 协议。APOC 等外部过程库不因支持 Cypher 而自动存在。Kernel 不内置 embedding 模型、RAG 工作流、Agent 或 KG OS 领域对象；v0.2.0 的 `lithograph-openai-compatible` 是独立 SQLite Provider extension，不属于 Cypher compatibility profile。

因此，将现有 Cypher 应用迁移过来时，query 本身与 transport、账号权限、procedure 库、Schema 管理和事务入口要分开核对，不能把“支持 Cypher 25”解释成整个 Neo4j 部署的无改动替换。

## 相同语言、不同宿主入口

图查询文本仍是 Cypher；SQLite 通过 `lithograph()`、`lithograph_rows()` 或 Native API 承载执行。版本管理通过 `CALL lithograph.*` procedures 扩展数据库能力，不创造新的 Cypher grammar。

`CALL ... IN TRANSACTIONS` 虽然是 profile 的 query 能力，但需要能拥有 batch transaction boundary 的普通 Native 入口，不能放进 SQL scalar 或 Native explicit transaction。`IN CONCURRENT TRANSACTIONS` 不意味着一个 SQLite 文件支持多个物理 writer 同时提交。

`lithograph_rows` 只读且无外部 I/O；历史 at 只读；Graph View 约束执行可见子图。Graph Type / Constraint / Index 与图状态一起 versioned，历史查询解析历史 Schema，而不是最新 schema catalog。

## 验收证据与本次文档验证的区别

仓库记录的 release-level inherited openCypher TCK evidence 为：3,897 个 scenario 中 3,777 个 applicable 全部通过，其余 120 个有明确的 profile 排除或 superseded 原因；另有 Cypher 25 专项 fixtures 与 feature/regression suites。该证据记录于 [兼容矩阵](../development/cypher25-compatibility.md)，不是本次编写手册重新运行出来的全套测试结果。

本次文档验证另行使用已发布 v0.1.0 制品执行指南 SQL、Python 与 C 示例，并记录实际发现的 adapter / introspection 差异；范围见 [验证记录](../guide/examples/README.md)。测试通过不构成任意输入、任意数据规模下绝无缺陷的保证。

未来 profile 更新应明确新的版本和变化；v0.1.0 文档不根据外部手册的后续更新静默改变原有能力范围。
