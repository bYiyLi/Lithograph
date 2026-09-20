# Cypher 25 兼容合同

[设计入口](../design.md) · [开发状态与验收](../development/README.md)

本文件拥有冻结的 Cypher 25 Profile、范围与 oracle。SQLite SQL execution context 见 [接口合同](interfaces.md)，具体值与查询执行语义见 [查询引擎](query-engine.md)。


<a id="compatibility-profile"></a>

## Compatibility Profile

Lithograph 的首个完整兼容基线命名为 `CY25-2026.08`。

该 Profile 冻结以下来源截至 **2026-09-09** 可观察到的 Cypher 25 当前图能力：

- Neo4j Cypher Manual 的 Cypher 25 当前语言、值与类型、函数、查询、Pattern、Path、Mutation、Schema、Constraint、Index、Procedure 与 `LOAD CSV` 语义；
- Neo4j 2026.07 已发布 Cypher 25 语义；
- Neo4j Aura 2026.08 已公开的 Cypher 25 新能力，包括 native `UUID` 与 string interpolation。

Cypher 25 会继续演进。Lithograph 的“完整兼容”始终针对一个冻结 Profile 判断；新的 Cypher 25 能力通过新的 Compatibility Profile 升级，不改变旧 Profile 的测试基线。

Lithograph 默认且只执行 Cypher 25。查询接受显式 `CYPHER 25` version prefix，也接受 Cypher 自身的 `CYPHER <query-options>` preamble；两者可以组合为 `CYPHER 25 <query-options>`。与 `EXPLAIN` / `PROFILE` 组合时，兼容 Cypher 25 当前可接受的 preamble 顺序。显式选择 `CYPHER 5` 或其它非 25 version 必须拒绝。这里的 Cypher query options 属于语言 frontend，与[Query Options](interfaces.md#query-options)通过 Extension API 传入的 Lithograph execution-level Query Options 是两套独立接口，不得互相解释或覆盖。

<a id="compatibility-scope"></a>

## 完整兼容范围

`CY25-2026.08` 覆盖当前 graph/database 语义：

- `MATCH`、`OPTIONAL MATCH`、`FILTER`、`WHERE`、`RETURN`、`WITH`、`LET`、`UNWIND`、`FOR`；
- `UNION`、`WHEN`、`NEXT`、subquery、subquery expressions、aggregation、`GROUP BY`、ordering 与 pagination；
- node / relationship patterns、quantified patterns、variable-length patterns、path selectors、shortest paths、match modes 与 path modes；
- `CREATE`、`INSERT`、`MERGE`、`SET`、`REMOVE`、`DELETE`、`DETACH DELETE`、`FOREACH`；
- Cypher 25 runtime value/type system、property types、temporal、spatial、`VECTOR`、`UUID`、list、map、node、relationship、path 与 `null` semantics；
- Cypher 25 built-in scalar、predicate、list、string、temporal、spatial、vector、aggregation 与 conversion functions；
- Graph Type、element type、property type、key、unique、existence / `NOT NULL` 与其它 current-graph constraints；
- lookup、range、text、point、full-text、vector indexes，以及 `SHOW INDEXES`；
- vector `SEARCH` subclause，以及 current Cypher full-text query procedures；
- current-graph `SHOW` surfaces，例如 functions、procedures、indexes、constraints 与 current graph type；
- `LOAD CSV`；
- `EXPLAIN` 与 `PROFILE`；
- `CALL { ... } IN TRANSACTIONS` 与 `IN CONCURRENT TRANSACTIONS` 的 query-engine semantics。

Neo4j DBMS 自身的多数据库管理、数据库 alias、用户/角色/权限、cluster/server、OIDC/ABAC、Java UDF 部署和系统数据库管理命令属于 Neo4j 产品管理面，不属于 Lithograph 的 current-graph Cypher compatibility Profile。`USE`、`graph.byName()`、`graph.names()` 等依赖 DBMS/composite-database graph selection 的 surface 同样不进入该 Profile；Lithograph 的 version selection 使用[SQLite Extension 接口](interfaces.md#sqlite-extension)、[Versioned Graph Model](versioning.md)的 Branch/Commit/Tag context，而不是伪装成 Neo4j composite database。

Lithograph 的 `graphView` execution context（[Query Options](interfaces.md#query-options)、[Graph View Execution Boundary](query-engine.md#graph-view)）是 Adapter / Engine 层对**同一个 Versioned Property Graph 的 query-local 可见子图**进行约束的通用能力，不属于 Cypher 25 grammar 或 compatibility Profile。它不得把 Cypher 25 `USE` 重新解释成子图过滤，也不得引入 Lithograph-specific Cypher clause。

SQL explicit transaction（[SQL Bridge](interfaces.md#sql-bridge)、[Explicit Transaction](storage.md#native-explicit-transaction)）同样是 Lithograph execution/version boundary，不属于 Cypher 25 grammar 或 compatibility Profile。它只改变多个标准 Cypher execution 何时共同 finalize 为 Commit，不改变单个 execution 的 Cypher 语义，也不把 transaction lifecycle 注入 Cypher text。

Full-text 的 FTS5 provider binding 由[Full-text](full-text.md)明确限定：沿用 Cypher 25 DDL、procedure、配置 key 和值类型，不新增 grammar；但 analyzer 字符串的取值、默认分词行为和评分数值依赖 backend。FTS5 tokenizer specification 不是 Neo4j analyzer 名称的可移植替代，不能仅凭相同字段名宣称两种引擎的分词、stop words 或相关性分数完全一致。这一 binding 的破坏性调整单列在 [compatibility inventory](../development/cypher25-compatibility.md)；不重写冻结 Cypher 语言 fixture 的预期结果来掩盖差异。

<a id="compatibility-oracle"></a>

## Compatibility Oracle

兼容性验证使用三层证据：

1. openCypher TCK 作为继承语义基线；
2. `CY25-2026.08` feature matrix 对 Cypher 25 新增/改变语义逐项建立 fixture；
3. Neo4j 对同一 fixture 的可观察结果作为差异核对 oracle，涉及 Neo4j 专有管理面时不进入 current-graph Profile。

GraphQLite v0.6.0 已证明 SQLite extension + Cypher + openCypher TCK 路线可行，并达到 97.7% openCypher TCK；Lithograph 复用其测试方法和实现经验，但目标是 `CY25-2026.08` 的 100% applicable matrix，而不是 GraphQLite 当前 coverage。
