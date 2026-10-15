# Index

适用 Lithograph v0.3.1、`CY25-2026.08`。[Cypher 手册](index.md) · [兼容范围](../reference/cypher-compatibility.md) · [Schema 操作指南](../guide/schema-and-indexes.md)

Index definition 随图版本化，物理索引内容可以重建。标准索引有 LOOKUP、RANGE、TEXT、POINT、FULLTEXT、VECTOR 六类；Managed Semantic 是通过 [Semantic Procedure](../reference/procedures/semantic.md) 创建的独立扩展类型，仍由通用 SHOW/DROP 管理。

<a id="syntax"></a>

## 名称与通用规则

```text
CREATE [<kind>] INDEX [name] [IF NOT EXISTS]
FOR <pattern> ON <target> [WITH [<filter-property>, ...]]
[OPTIONS {indexConfig: {...}}]

DROP INDEX name [IF EXISTS]
SHOW [ALL | LOOKUP | RANGE | TEXT | POINT | FULLTEXT | VECTOR] {INDEX | INDEXES}
```

省略 kind 表示 RANGE。name 是标识符，不是 String expression；省略时获得自动名称，可从 SHOW 获取。名称与 Constraint 共用 namespace；不同 Index kind 也不能同名。参数不替换 Schema 标识符。Index 创建不会生成或转换图属性，也不强制该属性存在；需要类型、存在性或唯一性保证时使用 [Constraint](constraints.md)。

普通 CREATE 遇到同名或等价索引返回 `SCHEMA_ERROR`。`IF NOT EXISTS` 可在同名或等价普通 Index 已存在时成功保留旧定义；它不会更新配置，且不会绕过配置形状检查、Constraint 名称冲突或 Constraint backing-index 冲突。成功的无变化 DDL 仍记录 Schema write intent，不保证不新增 Commit。

DROP 不删除节点、关系、属性或历史定义。不存在时失败，加 IF EXISTS 则成功。KEY/UNIQUE 拥有的 RANGE index 不能单独 DROP，应删除 owning Constraint。Index 不能用 ALTER 修改；需要改变配置时 DROP/recreate，或通过正常 Branch/Schema 版本工作流操作。

<a id="lookup"></a>

## LOOKUP

```text
CREATE LOOKUP INDEX [name] [IF NOT EXISTS]
FOR (n) ON EACH labels(n)

CREATE LOOKUP INDEX [name] [IF NOT EXISTS]
FOR ()-[r]-() ON EACH type(r)
```

LOOKUP 为 Label/Relationship Type 访问提供索引，不按单个 Property 建立。每种实体的同类等价 LOOKUP 不能重复创建。查询通常仍写普通 MATCH，由 Planner 选择访问路径。

<!-- example: schema-index-lookup -->
```cypher
CREATE LOOKUP INDEX relationship_types FOR ()-[r]-() ON EACH type(r)
```

<a id="range"></a>

## RANGE

```text
CREATE [RANGE] INDEX [name] [IF NOT EXISTS]
FOR (n:Label) ON (n.property [, n.property ...])

CREATE [RANGE] INDEX [name] [IF NOT EXISTS]
FOR ()-[r:TYPE]-() ON (r.property [, r.property ...])
```

RANGE 支持等值、IN、范围和复合 Property 访问。字段顺序属于 definition；`(a,b)` 不等于 `(b,a)`。索引支持访问不等于查询一定使用它：Planner 必须保留跨类型比较、null 和错误语义，缺少足够的 Property Type proof 时可能选择 scan/filter。

<!-- example: schema-index-range -->
```cypher
CREATE RANGE INDEX person_age FOR (p:Person) ON (p.age)
```

<!-- example: schema-index-data -->
```cypher
CREATE (:Person {name:'Ada', age:36}), (:Person {name:'Bo', age:22}) FINISH
```

<!-- example: schema-index-query -->
```cypher
MATCH (p:Person) WHERE p.age >= 30 RETURN p.name AS name ORDER BY name
```

在索引和数据建立后返回 `'Ada'`。是否使用索引不会改变结果；通过 [EXPLAIN/PROFILE](diagnostics.md) 观察计划。

<a id="text-point"></a>

## TEXT 与 POINT

```text
CREATE TEXT INDEX [name] [IF NOT EXISTS] FOR (n:Label) ON (n.text)
CREATE TEXT INDEX [name] [IF NOT EXISTS] FOR ()-[r:TYPE]-() ON (r.text)
CREATE POINT INDEX [name] [IF NOT EXISTS] FOR (n:Label) ON (n.location)
CREATE POINT INDEX [name] [IF NOT EXISTS] FOR ()-[r:TYPE]-() ON (r.location)
```

两者都要求一个 Property。TEXT 服务 String predicate，例如 STARTS WITH、ENDS WITH、CONTAINS；它不是分词或相关性搜索，后者使用 FULLTEXT。非 String 的 String predicate 仍遵守返回 null 的语言语义。

POINT 服务 Point 等值、距离/空间条件。CRS、维数和空间函数含义见 [Spatial functions](functions/spatial.md)。Index 不能使两个不同 CRS 的值自动转换，也不把错误类型悄悄当作同一坐标系。

<!-- example: schema-index-text -->
```cypher
CREATE TEXT INDEX person_name FOR (p:Person) ON (p.name)
```

<!-- example: schema-index-point -->
```cypher
CREATE POINT INDEX place_location FOR (p:Place) ON (p.location)
```

v0.3.1 不提供可调整的标准 LOOKUP/RANGE/TEXT/POINT provider 或空间范围配置；不要从 Neo4j 的服务器配置推导本扩展的效果。

<a id="fulltext"></a>

## FULLTEXT

```text
CREATE FULLTEXT INDEX [name] [IF NOT EXISTS]
FOR (n:Label1|Label2|...) ON EACH [n.property1, n.property2, ...]
[OPTIONS {indexConfig: {
  `fulltext.analyzer`: <literal-string>,
  `fulltext.eventually_consistent`: <literal-boolean>
}}]

CREATE FULLTEXT INDEX [name] [IF NOT EXISTS]
FOR ()-[r:TYPE1|TYPE2|...]-() ON EACH [r.property1, r.property2, ...]
[OPTIONS {indexConfig: {...}}]
```

多个 Label/Type 是候选域的并集；列出的多个 Property 共同提供文本。缺失/非文本值不作为普通文本检索项；String/List<String> 文本抽取与检索表达式的准确规则见 [Full-text Procedure](../reference/procedures/fulltext.md)。

| 配置 | 类型与默认值 | 行为 |
| --- | --- | --- |
| `fulltext.analyzer` | 非空 String，默认 `'unicode61'` | 宿主原生 FTS5 tokenizer specification；不含 NUL，名称和参数作为完整字符串保存 |
| `fulltext.eventually_consistent` | Boolean，默认 `false` | 保存兼容 metadata；当前查询仍保证目标 Snapshot 的正确可见数据 |

OPTIONS 必须是 literal Map，只支持 `indexConfig`；config 未知 key、null、错误类型均失败。不支持 `indexProvider` 选择。默认 tokenizer 来自实际宿主 SQLite；自定义 tokenizer 需要在每个实际连接注册。普通 CREATE 在发布 Schema 前验证构造能力，缺失 tokenizer 返回 `SCHEMA_ERROR`；不会静默换成其它 tokenizer。

<!-- example: schema-index-fulltext -->
```cypher
CREATE FULLTEXT INDEX article_text
FOR (a:Article|Note) ON EACH [a.title, a.body]
OPTIONS {indexConfig: {
  `fulltext.analyzer`: 'unicode61',
  `fulltext.eventually_consistent`: false
}}
```

Specification 使用 FTS5 的参数语法，例如 Cypher String `"unicode61 tokenchars '-_'"`、`'porter unicode61'`。字符串内部参数的顺序、引号和大小写被保留在历史中；`english` 和 `standard-no-stop-words` 没有特殊映射。改变 analyzer 需 DROP/recreate，历史 Commit 保留旧 specification 和运行时依赖。

`EXPLAIN CREATE FULLTEXT INDEX ...` 只静态检查，不构造 tokenizer，也不证明实际 runtime 可用。实际 query 和 rebuild 还会处理全文 corpus，可因 tokenizer、TEMP、I/O 或资源错误失败。查询语法、query-time analyzer、skip/limit、score 及错误详见 [Full-text Procedure](../reference/procedures/fulltext.md)。

<a id="vector"></a>

## VECTOR

```text
CREATE VECTOR INDEX [name] [IF NOT EXISTS]
FOR (n:Label1|Label2|...) ON (n.embedding)
[WITH [n.filterProperty1, n.filterProperty2, ...]]
[OPTIONS {indexConfig: {...}}]

CREATE VECTOR INDEX [name] [IF NOT EXISTS]
FOR ()-[r:TYPE1|TYPE2|...]-() ON (r.embedding)
[WITH [r.filterProperty1, r.filterProperty2, ...]]
[OPTIONS {indexConfig: {...}}]
```

必须恰好一个 indexed vector Property。WITH 声明 [SEARCH WHERE](search.md#filters) 可用的 additional filtering properties；只能用于 VECTOR，字段不能重复或等于 vector Property。该列表参与 versioned definition，不会把过滤字段拼进向量。

Raw Vector 数据由应用提供，Property 可以是 VECTOR 或有效数值 List。索引只纳入满足向量维度/数值/相似度条件的值；缺失或不适合该向量空间的属性不会生成有效候选。VECTOR Property 的 coordinate type 与 dimension 保持其值语义；Index 不自动生成 Embedding。

| indexConfig key | 类型、范围、默认值 |
| --- | --- |
| `vector.dimensions` | 可省略；提供时 INTEGER 1–4096。省略时按 query vector 维度检索对应维度的候选 |
| `vector.similarity_function` | String `'cosine'`（默认）或 `'euclidean'`，值不区分大小写 |
| `vector.quantization.type` | String `'none'`、`'scalar'` 或默认 `'binary'` |
| `vector.quantization.enabled` | 兼容 Boolean；未指定 type 时 `true` 对应 scalar，`false` 对应 none；同时提供时 type 优先 |
| `vector.default_search_expansion_factor` | 有限 Number 1–10000；none 默认 1、scalar 默认 1.5、binary 默认 3 |
| `vector.hnsw.m` | INTEGER 1–512，默认 16 |
| `vector.hnsw.ef_construction` | INTEGER 1–3200，默认 100 |

`indexConfig` 只能使用上述 key；未知 key、显式 null、错误类型或越界值返回 `SCHEMA_ERROR`。配置为 literal Map；不以参数或任意函数计算 OPTIONS。quantization/HNSW/expansion 是检索资源与近似召回的选择，不改变 canonical Vector Property。

<!-- example: schema-index-vector -->
```cypher
CREATE VECTOR INDEX doc_embedding FOR (d:Doc) ON (d.embedding)
WITH [d.lang, d.published]
OPTIONS {indexConfig: {
  `vector.dimensions`: 2,
  `vector.similarity_function`: 'cosine',
  `vector.quantization.type`: 'none',
  `vector.hnsw.m`: 16,
  `vector.hnsw.ef_construction`: 100
}}
```

<!-- example: schema-index-vector-invalid -->
```cypher
CREATE VECTOR INDEX invalid_vector FOR (d:Doc) ON (d.embedding)
OPTIONS {indexConfig: {`vector.dimensions`: 0}}
```

后例返回 `SCHEMA_ERROR`。Managed Semantic 使用 String source + Provider，它的 create/query Procedure 不属于本 DDL，也不能将 VECTOR Index 的 source String 解释成自动 Embedding。

<a id="show"></a>

## SHOW INDEXES 的列与过滤

```text
SHOW [ALL | <kind>] {INDEX | INDEXES}
  [YIELD column [AS alias] [, ...] | YIELD *]
  [ORDER BY ...] [SKIP ...] [LIMIT ...]
  [WHERE predicate] [RETURN ...]
```

`SHOW ALL INDEXES` 包含 `SEMANTIC`，`SHOW VECTOR INDEXES` 只包含标准 VECTOR。省略 ALL 的 SHOW INDEXES 同样列出当前图索引；YIELD 可选择扩展列和 aliases，RETURN 承担表达式计算。

| 列 | 当前行为 |
| --- | --- |
| `id`, `name` | 当前目录序号与稳定的 definition name；id 不用于跨版本引用 |
| `type`, `entityType` | Index kind 与 `NODE`/`RELATIONSHIP` |
| `labelsOrTypes`, `properties` | 目标域与字段；VECTOR properties 包含 indexed Property 之后的 additional properties |
| `state`, `populationPercent` | versioned definition 已可用，当前展示 `ONLINE`、100；不证明 TEMP cache 已预热 |
| `indexProvider` | Lithograph 后端标识；不表示安装了 Neo4j server provider |
| `owningConstraint` | backing index 的 constraint name；普通索引为 null |
| `lastRead`, `readCount` | 当前为 null、0，不是持久读取统计 |
| `trackedSince`, `options`, `failureMessage`, `createStatement` | 扩展列：当前 trackedSince 为 null，options 显示保存配置，failureMessage 为空，createStatement 展示基础创建形式 |

默认列到 `readCount`，扩展列可显式 YIELD 或 `YIELD *` 获取。Full-text/Vector 默认值在新 definition 中明确保存；SHOW options 反映目标 Snapshot 的实际配置。

当前 `createStatement` 不能单独作为完整备份或迁移输入：FULLTEXT/VECTOR 的该字符串可能省略 OPTIONS 和 VECTOR additional properties。重建时还须保存 `options`、完整 `labelsOrTypes` 与 `properties`，按本页 DDL 还原，不能只复制这一列。同一 database 内的版本比较与重放可以使用 [Diff/Patch](../reference/procedures/versioning.md#proc-lithograph-diff)；这些版本工具不支持跨 database Patch。跨 database 迁移应由应用迁移数据，并按完整索引字段重新构建 DDL；完整数据库备份与恢复见 [运维指南](../guide/operations.md)。

<!-- example: schema-index-show -->
```cypher
SHOW VECTOR INDEXES YIELD name, type, properties
RETURN name, type, properties ORDER BY name
```

在 `doc_embedding` 建立后返回 `['doc_embedding','VECTOR',['embedding','lang','published']]`。

<!-- example: schema-index-drop -->
```cypher
DROP INDEX doc_embedding
```

<a id="history-errors"></a>

## 历史、执行上下文与失败

历史 Snapshot 选择当时的 Index definition。旧定义的 cache 缺失时使用该 Snapshot 构建或正确 fallback，不借用当前 Branch 的 definition。物理 cache 删除不删除索引定义；显式维护入口见[维护 Procedure](../reference/procedures/maintenance.md)。

普通成功 DDL 发布 Schema Commit；显式事务内参与 transaction-local Schema。DDL 验证及构建失败不会发布部分 Schema/Branch 状态。非空 Graph View、只读 Snapshot、SQLite 只读连接和 `readOnlyNoExternalIO` 按对应执行边界拒绝修改；SHOW 仍可检查 definition。

语法错误为 `PARSE_ERROR`，名称/目标/配置问题为 `SCHEMA_ERROR`，已有数据违反 Schema 为 `CONSTRAINT_ERROR`；执行中的 SQLite busy、I/O、resource、中断保留其具体错误分类，不能当成零结果。参见[错误参考](../reference/errors.md)和 [SEARCH](search.md)。
