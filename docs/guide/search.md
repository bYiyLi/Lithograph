# Search 与数据导入

[产品手册](README.md) · [Cypher 语言手册](../cypher/index.md) · [接口参考](../reference/README.md)

本文适用 **v0.3.3**（`CY25-2026.08`，storage format `3`）。

Managed Semantic 从 v0.2.0 起可用。以下检索 SQL 在独立空库、已加载扩展的 connection 中依次运行。**Raw Vector** 路径仍由应用提供向量及其维度、坐标类型和模型来源；**Managed Semantic** 才通过独立 Embedding Provider extension 生成 derived Vector。

<a id="fulltext"></a>

## 准备文档与全文索引

```sql
SELECT lithograph_init();
SELECT lithograph(
  'CREATE (:Doc {id:''graph'', title:''Knowledge graph'', lang:''en'',
                embedding:vector([1.0,0.0],2,FLOAT64)}),
          (:Doc {id:''sql'', title:''SQL database'', lang:''en'',
                embedding:vector([0.0,1.0],2,FLOAT64)}) FINISH'
);
SELECT lithograph(
  'CREATE FULLTEXT INDEX doc_text FOR (d:Doc) ON EACH [d.title]'
);
SELECT lithograph(
  'CALL db.index.fulltext.queryNodes(''doc_text'', $text, {skip:0, limit:10})
   YIELD node, score RETURN node.id AS id, score',
  '{"text":"graph"}'
);
```

命中 `graph`。`node` 是图元素，不是预先展开的字段 Map；使用 `node.id` / `node.title` 投影业务字段。score 是本次检索的相关性分数，不当成概率或跨索引通用阈值。

多 Label / 多 Property 的 DDL 可以写成 `FOR (d:Article|Doc) ON EACH [d.title,d.content]`。Relationship 检索使用 `db.index.fulltext.queryRelationships`，返回列是 `relationship, score`。

全文查询是检索表达式，不是 Cypher 文本；不要拼接成另一条 Cypher。options 可包含 `skip`、`limit`、`analyzer`。

Tokenizer 的默认值、FTS5 specification、query-time analyzer 及检索表达式见 [Full-text Procedure Reference](../reference/procedures/fulltext.md)。使用宿主外部 tokenizer 时，必须在每个真正执行查询的 SQLite connection 上注册；Index definition 保存 specification，不携带本机 tokenizer 实现。新定义会在当前 connection 上验证 tokenizer；历史 SHOW 只读定义，真正查询或冷重建仍需要历史 tokenizer 可用。

<a id="vector-search"></a>

## 向量索引与 SEARCH

```sql
SELECT lithograph(
  'CREATE VECTOR INDEX doc_embedding FOR (d:Doc) ON (d.embedding)
   WITH [d.lang]
   OPTIONS {indexConfig:{`vector.dimensions`:2,
                         `vector.similarity_function`:''cosine''}}'
);
SELECT lithograph(
  'MATCH (d:Doc)
   SEARCH d IN (VECTOR INDEX doc_embedding
                FOR vector([1.0,0.0],2,FLOAT64)
                WHERE d.lang = ''en'' LIMIT 1)
   SCORE AS score
   RETURN d.id AS id, score'
);
```

最近结果为 `graph`。过滤条件放在 SEARCH 内，表达“过滤后的 top-k”，而不是取全局 top-k 后再丢弃不符合语言条件的行。索引的 `WITH` 声明可用于过滤的额外 Property。

应用参数可以传保真 Vector，不必把向量插入查询字符串：

```sql
SELECT lithograph(
  'MATCH (d:Doc)
   SEARCH d IN (VECTOR INDEX doc_embedding FOR $embedding LIMIT 1)
   RETURN d.id AS id',
  '{"embedding":{"$type":"Vector","coordinateType":"FLOAT64","dimension":2,"values":[1.0,0.0]}}'
);
```

维度、坐标类型和 similarity 必须与数据/索引相容。不要混用不同 embedding 模型的坐标空间；这是应用数据管理责任，不是数据库能够从数值自动推断的事实。

HNSW 是近似检索访问路径，缺少可用缓存时可以使用 exact scan fallback。不要假设所有 cache 状态的延迟相同，也不要把 ANN 返回结果当成任意数据集上 exact top-k 的保证。索引参数范围见 [Limits](../reference/limits.md)。

<a id="managed-semantic-text-search"></a>

## Managed Semantic 文本检索

本节从 v0.2.0 起可用。Managed Semantic 不改变上面的 Raw Vector / `SEARCH`：它为 String source Property 增加一条数据库托管 embedding 的并列路径。

首先按 [配置 Embedding Provider](embedding-providers.md) 在实际执行检索的 connection 上加载双扩展、配置可信 endpoint 和认证，然后创建示例 Semantic Index。以下调用需要可用的 Provider 和网络服务，不计入无需外部服务的 SQL 教程验收；离线集成验收使用 [Provider 开发指南](provider-development.md) 中的 synthetic Provider。

```text
SELECT lithograph_init();
SELECT lithograph(
  'CREATE (:Doc {id:''a'', content:''knowledge graph''}),
          (:Doc {id:''b'', content:''sqlite database''}) FINISH'
);
SELECT lithograph(
  'CALL db.index.semantic.createNodeIndex(
     ''doc_semantic'', [''Doc''], ''content'',
     {provider:''openai-compatible'',
      providerConfig:{base_url:''https://api.openai.com/v1'',
                      api_key_env:''OPENAI_API_KEY'',
                      model:''text-embedding-3-small''},
      dimensions:1536, similarity:''cosine''})'
);
```

`providerConfig` 是 versioned Schema；使用 `api_key_env` 等间接凭据，避免把 secret 明文写入历史。完整配置、默认值、网络和缓存边界由 [OpenAI-compatible Provider Reference](../reference/openai-compatible-provider.md) 维护。

以下为同一服务配置下的 SQL 查询模板；需先完成上面的服务依赖准备：

```text
SELECT lithograph(
  'CALL db.index.semantic.queryNodes(
     ''doc_semantic'', $query, {skip:0, limit:10}
   )
   YIELD node, score
   RETURN node.id, score',
  '{"query":"graph database"}'
);
```

source 必须是实际 String；missing、`null` 与其它类型不参与索引。String 按精确 UTF-8 bytes 发送给 Provider，不 trim、lowercase、拼接或截断。Graph View 在 provider input 与 top-k 前生效，历史查询使用目标 Commit 的历史 Semantic definition。

同一次 execution 内，重复的 exact text 可按 embedding-space 去重；跨 execution 的缓存由 Provider 自行实现。启用 Provider cache 的流程见 [Provider 使用指南](embedding-providers.md)，其文件独立于 graph repository，不参与 graph Commit、backup、integrity 或 GC。

`rebuild` 是可选维护入口，不是正常搜索的前置步骤。以下为需要实际 Provider 的 SQL 模板：

```text
SELECT lithograph(
  'CALL db.index.semantic.rebuild(''doc_semantic'', ''branch/main'')
   YIELD name, commit, indexedEntities, embeddedTexts
   RETURN name, commit, indexedEntities, embeddedTexts'
);
```

`rebuild` 只负责目标 committed Snapshot 的当前-connection TEMP Semantic/HNSW materialization：不创建 graph Commit、不移动 ref，返回中的 `commit` 是实际 pin 的 target Commit。它不报告或维护 Provider cache。普通 Semantic query 可通过 `lithograph()` 与 `lithograph_rows()` 执行，并在 active explicit transaction 中读取当前 staged source/membership/definition。实际 query/rebuild 每次都要求目标 Provider 当前可用并通过 validation。

<a id="historical-search"></a>

## 历史检索和子图检索

与普通查询相同，通过第三个参数 `options.at` 选择历史 Schema / 数据，通过 `options.graphView` 选择可见子图。历史上尚未创建的索引不能仅因为当前 Branch 有同名索引而使用。Graph View 在检索的可见结果与 top-k 边界内生效，不是结果返回后过滤。

全文和向量可以在普通 Cypher 中组合过滤、子查询和投影；应用自己定义融合分数与排序逻辑。没有额外的 `hybridSearch` Procedure；组合方式和最终排序由应用明确表达。

<a id="csv-import"></a>

## 导入 CSV

CSV、参数批量写入和 transaction batch 的完整操作流程已归入 [数据导入](import.md)。本节保留原路径供已有链接定位。LOAD CSV 语言参考见 [LOAD CSV](../cypher/load-csv.md)，batch 语义见 [Transaction Subqueries](../cypher/transaction-subqueries.md)。

<a id="reproducible-search"></a>

## 为搜索建立可重复的验证

Raw Vector 教程的向量由应用给定，`[1,0]` 与 `[0,1]` 的教学数据可直接核对最近结果。实际数据应固定模型、坐标类型、维度和过滤条件，再比较预期的业务结果；ANN 与 exact fallback 不保证在任意数据上返回相同 top-k。

Managed Semantic 的结果还受 Provider 模型和服务实现影响。需要可重复的集成测试时使用固定的 synthetic Provider，不以线上服务某一次的 score 作为永久断言。执行失败时先检查目标 Commit 的 Index definition、当前连接 Provider/tokenizer 和参数类型，再检查服务错误。

若宿主对输入启用 `readOnlyNoExternalIO:true`，Raw Vector SEARCH 仍允许；Full-text query Procedure 与 Managed Semantic 操作会在执行前拒绝。完整限制见 [安全执行](safe-execution.md)。
