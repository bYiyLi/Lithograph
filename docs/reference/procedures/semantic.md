# Managed Semantic Procedure

适用于 Lithograph v0.3.1 · [产品手册](../../guide/README.md) · [Procedure 总入口](../procedures.md) · [搜索工作流](../../guide/search.md) · [Provider 配置](../openai-compatible-provider.md)

Managed Semantic 将一个 String Property 的精确文本交给当前 connection 注册的 Embedding Provider，再做向量相似度检索。文本和 IndexDefinition 随 graph history 版本化；生成的 Vector 不成为 Property 或 Commit 数据。自己管理 Vector Property 的调用方使用 [Raw Vector SEARCH](../../cypher/search.md)。

<a id="create-options"></a>

## 创建参数与公共规则

两项 create 接口都必须作为单独的 `CALL` 执行，不能接 `YIELD`、RETURN 或其它 clause。参数可以来自 `$params`；没有 `IF NOT EXISTS` overload。返回 `columns:[]`、`rows:[]`，普通 auto-commit 成功创建一个 Schema Commit，active Lithograph explicit transaction 中则 staged，到 tx commit 时统一发布。

| 参数 | 类型与要求 |
| --- | --- |
| `indexName` | 非空、无 NUL 的 `STRING`；与所有其它 Index 共用名称空间，同名失败 |
| `labels` / `relationshipTypes` | 非空 `LIST<STRING>`，元素非空、无 NUL、不得重复；元素符合任意一个 Label/Type 即属于候选 target |
| `sourceProperty` | 非空、无 NUL 的 `STRING`；v1 只支持一个 Property |
| `options` | 必需 `MAP`，只允许下表四个 key，均必需 |

| create options key | 类型与含义 |
| --- | --- |
| `provider` | 非空、无 NUL、区分大小写的 `STRING`；当前 SQLite connection 的注册名 |
| `providerConfig` | `MAP`；值可递归包含 JSON-compatible null、Boolean、Integer、有限 Float、String、List、String-key Map；Provider 自行验证其 key 和默认值 |
| `dimensions` | `INTEGER`，范围 `1..4096`；Provider 输出固定 FLOAT32 |
| `similarity` | `STRING`，输入按 ASCII 大小写不敏感，规范为小写 `'cosine'` 或 `'euclidean'` 保存 |

create 只调用 Provider 的本地 `validate`，不扫描数据、不执行 `embedBatch`、不创建 Provider cache。Shape、范围、目标重复、同名 Index、额外 clause/YIELD 等返回 `SCHEMA_ERROR`；Provider 缺失/配置无效返回 `INVALID_ARGUMENT`，ABI/registration 无效返回 `INTERNAL_ERROR`，Provider 错误按[失败规则](#errors)传播。

`providerConfig` 原值进入 Schema、SHOW、Diff/Patch 和备份，包括直接写入的 credential 或本机 cache path。需要凭据间接引用时使用 Provider 的 `api_key_env` 等字段。它没有由 Kernel 自动剔除 secret 的步骤。

<a id="proc-db-index-semantic-createnodeindex"></a>

## `db.index.semantic.createNodeIndex`

```text
CALL db.index.semantic.createNodeIndex(indexName, labels, sourceProperty, options)
```

Mode `WRITE`。索引目标为 Node，只有目标 Node 的 sourceProperty 当前为 String 时参与；空字符串也是 String，缺失、null、数字和 String List 均不参与。输入文本不 trim、lowercase、截断、拼接或分块。

下列例子使用仓库附带的 deterministic synthetic Provider，便于无需网络验证接口。先在同一 connection 加载 `synthetic_embedding_provider` 的 `sqlite3_syntheticembedding_init` entrypoint；正式产品使用自己的 Provider 和对应配置，不把 `synthetic-a` 当成内置模型。

<!-- example: proc-sem-data -->
```cypher
CREATE (:Doc {id:'a', text:'alpha'}) RETURN count(*) AS created
```

<!-- example: proc-sem-node-create -->
```cypher
CALL db.index.semantic.createNodeIndex('doc_sem', ['Doc'], 'text',
  {provider:'synthetic-a', providerConfig:{}, dimensions:4, similarity:'COSINE'})
```

<a id="proc-db-index-semantic-createrelationshipindex"></a>

## `db.index.semantic.createRelationshipIndex`

```text
CALL db.index.semantic.createRelationshipIndex(
  indexName, relationshipTypes, sourceProperty, options)
```

Mode `WRITE`。目标改为 Relationship，参数规则同 createNodeIndex。

<!-- example: proc-sem-rel-data -->
```cypher
CREATE (a:Person), (b:Person),
       (a)-[:KNOWS {text:'alpha'}]->(b)
RETURN count(*) AS created
```

<!-- example: proc-sem-rel-create -->
```cypher
CALL db.index.semantic.createRelationshipIndex('rel_sem', ['KNOWS'], 'text',
  {provider:'synthetic-a', providerConfig:{}, dimensions:4, similarity:'cosine'})
```

<a id="query-options"></a>

## 查询参数、score 与结果

两项 query 接口为 `READ`；`indexName :: STRING` 必须指向该 Snapshot 中的 Semantic Index，family 与 Procedure 一致；`queryString :: STRING` 为精确 UTF-8 输入。`options :: MAP` 必需，仅包含：

| key | 类型与默认 |
| --- | --- |
| `limit` | 必需，非负 `INTEGER`；0 返回零行，仍要求 Provider 存在且 validation 成功 |
| `skip` | 可选，非负 `INTEGER`，默认 0；skip+limit 不能溢出 |

未知 key、缺少 limit、null、错误类型和负数返回 `INVALID_ARGUMENT`；没有用 `{}` 表示无限结果的 overload。query 不接受 provider/model/dimensions 覆盖，始终采用目标 IndexDefinition。

返回 `node :: NODE, score :: FLOAT` 或 `relationship :: RELATIONSHIP, score :: FLOAT`；排序为 score descending、同分按元素 identity。cosine score 是 `(1+cosine)/2`，截在 `[0,1]`；euclidean score 是 `1/(1+平方距离)`。cosine 要求有限、非零 norm 的 Embedding。相似度不是业务概率，不承诺跨模型或不同配置比较。

<a id="proc-db-index-semantic-querynodes"></a>

## `db.index.semantic.queryNodes`

```text
CALL db.index.semantic.queryNodes(indexName, queryString, options)
YIELD node, score
```

在本页 Node 数据与 create setup 后执行：

<!-- example: proc-sem-node-query -->
```cypher
CALL db.index.semantic.queryNodes('doc_sem', 'alpha', {skip:0, limit:1})
YIELD node, score RETURN node.id AS id, score > 0.99 AS similar
```

返回 `['a', true]`。

Provider 已注册且配置有效时，零 limit 合法，返回零行：

<!-- example: proc-sem-zero-limit -->
```cypher
CALL db.index.semantic.queryNodes('doc_sem', 'alpha', {limit:0})
YIELD node RETURN node.id AS id
```

<a id="proc-db-index-semantic-queryrelationships"></a>

## `db.index.semantic.queryRelationships`

```text
CALL db.index.semantic.queryRelationships(indexName, queryString, options)
YIELD relationship, score
```

在本页 Relationship 数据与 create setup 后执行：

<!-- example: proc-sem-rel-query -->
```cypher
CALL db.index.semantic.queryRelationships('rel_sem', 'alpha', {limit:1})
YIELD relationship, score
RETURN relationship.text AS text, score > 0.99 AS similar
```

返回 `['alpha', true]`。

<a id="proc-db-index-semantic-rebuild"></a>

## `db.index.semantic.rebuild`

```text
CALL db.index.semantic.rebuild(name, version)
YIELD name, commit, indexedEntities, embeddedTexts
```

两个必需参数均为 `STRING`。`version` 是[完整版本 Descriptor](../versioning.md#descriptors)，目标必须为已经提交的版本，且该版本存在名为 name 的 Semantic Index。无 Procedure options。

| 返回列 | 类型和含义 |
| --- | --- |
| `name` | `STRING`，Index name |
| `commit` | `STRING`，实际 pin 的 `commit/<id>` |
| `indexedEntities` | `INTEGER`，符合 target 且 source 为 String 的元素数量 |
| `embeddedTexts` | `INTEGER`，这次 rebuild 解析的去重 source 文本数量；不等于 Provider 网络请求数或 cache miss 数 |

始终返回一行。Mode `READ`，`summary.queryType='read'`、summary commit 为显式 target Commit；重建当前 connection 的 TEMP Semantic/HNSW materialization，不写 Lithograph main、不创建 Commit、不移动 Branch、不清理 Provider cache。多个元素使用同一文本时 indexedEntities 可以大于 embeddedTexts。

<!-- example: proc-sem-rebuild -->
```cypher
CALL db.index.semantic.rebuild('doc_sem', 'branch/main')
YIELD name, indexedEntities, embeddedTexts
RETURN name, indexedEntities, embeddedTexts
```

本页单 Node setup 下返回 `['doc_sem',1,1]`。

rebuild 不接受 `options.at` 或非全图 `options.graphView`；历史目标应放在 version 参数。不能在 active Lithograph explicit transaction 或 Merge candidate 中执行，也不能和 graph mutation 或 transaction-owning subquery 组合。两个 SQL surface 均可在兼容的普通上下文调用。索引不存在/不是 Semantic 返回 `INVALID_ARGUMENT`，Descriptor 无效或不存在使用相应版本错误。

<a id="context"></a>

## Query 的 Snapshot、事务、Graph View 与缓存

普通 query 读取 execution pin 的 Snapshot；`options.at` 使用历史 definition/config/text。active Lithograph explicit transaction 支持 staged create/drop、source/membership 修改及随后 Semantic query；单次读写 execution 也允许前后 graph mutation，Procedure 读取它所在 clause boundary 的 staged state。它自身不发布 Commit，后续失败按整体 execution rollback。

Graph View 在 top-k/skip/limit 之前筛选，Relationship 同时检查两端可见性；只对实际可见的必要 source text 调用 Provider。普通 Semantic query 也可以在合法的只读 Merge candidate graph 上执行；显式 rebuild 则拒绝 candidate。查询不要求先 rebuild，也不要求 Lithograph main 可写。

每个实际执行 create/query/rebuild 的 connection 都需注册 Provider，另一 connection 的注册不生效。即使 TEMP cache 完整，实际 query/rebuild 仍要求目标历史 Provider 存在并通过 validation；SHOW/DROP/普通 graph read 不要求 Provider。Index definition 恢复或更改的 Patch/Merge/Rebase/Revert 发布前，会重新验证新增或改变的 Provider definition。

Core 仅在本次 execution 内对精确文本去重，按 graph state/IndexDefinition/Provider identity 管理可重建 TEMP 检索内容。跨 query/connection/process 的 text→Vector cache 完全由 Provider 维护；Core 不承诺 hit，不提供 `db.index.semantic.cache.*` Procedure。Provider cache 命中、淘汰和保留不改变 Lithograph Commit。

<a id="errors"></a>

## 失败与安全限制

| 情况 | category 与结果 |
| --- | --- |
| query 参数/options 不合法、Provider 未注册或配置无效 | `INVALID_ARGUMENT` |
| query 索引缺失/不是 Semantic、Node/Relationship family 不符、cosine 零向量 | `SEMANTIC_ERROR` |
| Provider ABI/registration 或返回数量/shape/非有限坐标不合法 | `INTERNAL_ERROR` |
| Provider I/O、资源耗尽、取消 | `IO_ERROR`、`RESOURCE_ERROR`、`INTERRUPTED` |
| rebuild 与 staged/事务边界冲突 | `TRANSACTION_BOUNDARY_REQUIRED`；options.at/graphView 为 `INVALID_ARGUMENT` |

失败不会形成“少一些候选”的成功结果。create failure 不移动 Branch；ordinary execution failure 回滚未发布 graph/Schema；active explicit transaction failure 自动 abort。外部 Provider 调用和已成功生成的 Provider cache 不能由 SQLite rollback 撤销。

`readOnlyNoExternalIO:true` 在 prepare 阶段拒绝五项 Semantic Procedure（`INVALID_ARGUMENT`），包括 create 的本地 validation、零 limit、EXPLAIN 和不会执行的嵌套分支。

<!-- example: proc-sem-missing-limit -->
```cypher
CALL db.index.semantic.queryNodes('doc_sem', 'alpha', {})
YIELD node RETURN node.id AS id
```

该例返回 `INVALID_ARGUMENT`。

<!-- example: proc-sem-invalid-dimensions -->
```cypher
CALL db.index.semantic.createNodeIndex('bad_sem', ['Doc'], 'text',
  {provider:'synthetic-a', providerConfig:{}, dimensions:0, similarity:'cosine'})
```

该例返回 `SCHEMA_ERROR`，不发布 definition。
