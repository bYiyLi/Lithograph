# Search 与数据导入

适用正式 v0.2.0。以下检索 SQL 在独立空库、已加载扩展的 connection 中依次运行。**Raw Vector** 路径仍由应用提供向量及其维度、坐标类型和模型来源；**Managed Semantic** 才通过独立 Embedding Provider extension 生成 derived Vector。

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

v0.1.1 的 `fulltext.analyzer` 是 **SQLite FTS5 tokenizer specification**，默认 `unicode61`。Lithograph 不再把 `standard-no-stop-words` / `english` 当特殊别名；Porter stemming 直接写成 `porter unicode61`。FTS5 自带参数也原样声明，例如：

```cypher
CREATE FULLTEXT INDEX doc_text FOR (d:Doc) ON EACH [d.title]
OPTIONS {indexConfig:{
  `fulltext.analyzer`: "unicode61 remove_diacritics 0 tokenchars '-_'"
}}
```

外部 tokenizer 由宿主 SQLite extension 注册，不由 Lithograph 安装。必须在**使用该 Index 的每个实际 SQLite connection** 上先加载/注册对应 tokenizer；definition 只保存 specification 字符串，不把 native 实现写进数据库。一个 connection 注册成功不会让连接池中的其它 connection 自动可用。DDL 创建/修改 definition 时会在当前 connection 用真实 FTS5 constructor 验证；历史 `SHOW` 不要求插件当前存在，但真正查询/冷重建需要该历史 specification 当前可构造。

query-time `analyzer` 也是同样的 FTS5 specification，只改变本次查询文本的 tokenizer，不重写已索引文档或 IndexDefinition。例如 `{analyzer:'porter unicode61'}`。短语、布尔表达式、Property 限定与 prefix 仍由 FTS5 MATCH 解释，不要预先自行把查询拆成词集合。更多边界见 [Procedure Reference](../reference/procedures.md) 与 [Limits](../reference/limits.md)。

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

当前仓库提供独立的 `lithograph-openai-compatible` SQLite extension。源码构建：

```sh
cargo build --locked --release -p lithograph-extension
cargo build --locked --release -p lithograph-openai-compatible
```

同一个**实际执行 Semantic 操作的 SQLite connection**必须同时加载 Lithograph 和所需 Provider。以下以 macOS Cargo 文件名为例；Linux/Windows 使用对应 shared-library 后缀：

```text
.load ./target/release/liblithograph.dylib sqlite3_lithograph_init
.load ./target/release/liblithograph_openai_compatible.dylib sqlite3_lithographopenaicompatible_init
```

初始化后建立文本数据与 Semantic Index：

```sql
SELECT lithograph_init();
SELECT lithograph(
  'CREATE (:Doc {id:''a'', content:''knowledge graph''}),
          (:Doc {id:''b'', content:''sqlite database''}) FINISH'
);
SELECT lithograph(
  'CALL db.index.semantic.createNodeIndex(
     ''doc_semantic'',
     [''Doc''],
     ''content'',
     {
       provider:''openai-compatible'',
       providerConfig:{
         base_url:''https://api.openai.com/v1'',
         api_key_env:''OPENAI_API_KEY'',
         model:''text-embedding-3-small'',
         encoding_format:''float'',
         timeout_ms:30000,
         max_retries:2,
         batch_size:32
       },
       dimensions:1536,
       similarity:''cosine''
     }
   )'
);
```

`providerConfig` 属于 versioned Schema。直接写 `api_key` 或 secret custom header 会按原值进入 history / SHOW / Diff / Patch / backup；不希望保存 credential 时使用 `api_key_env`，数据库只保存环境变量名。

`openai-compatible` 请求固定发往 `<base_url>/embeddings`。Provider 不自动跟随 3xx redirect，也不读取 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 等环境 proxy；需要经过兼容网关或代理时，把实际可访问的最终 endpoint 显式写入 `base_url`。这样实际网络 destination 与 versioned Schema / cache identity 保持一致。

文本查询：

```sql
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

普通 `queryNodes/queryRelationships` 对 query text 和可见 source text 使用相同的 persistent cache key。Cache enabled 且 database 可写时，miss 会调用 Provider、完整校验结果并自动写入 persistent cache；后续同 connection、新 connection 或 process restart 可直接复用。Cache 不保存原文副本。只读数据库或 cache disabled 时仍可查询，但 miss 只进入 connection-local TEMP/LRU。

`rebuild` 是可选维护入口，不是正常搜索的前置步骤：

```sql
SELECT lithograph(
  'CALL db.index.semantic.rebuild(''doc_semantic'', ''branch/main'')
   YIELD name, commit, indexedEntities, embeddedTexts, cacheHits
   RETURN name, commit, indexedEntities, embeddedTexts, cacheHits'
);
SELECT lithograph(
  'CALL db.index.semantic.cache.stats()
   YIELD enabled, maxBytes, usedBytes, entries, spaces
   RETURN enabled, maxBytes, usedBytes, entries, spaces'
);
```

`rebuild`、`cache.configure`、`cache.clear` 是 operational maintenance：不创建 graph Commit、不移动 ref，不能从 `lithograph_rows()` 或 Native explicit transaction 中执行。普通 semantic query 的 cache publish同样只修改derived cache，summary仍是`read`，不创建 graph Commit、不移动ref。实际 semantic query/rebuild 每次都要求目标 Provider 当前可用并通过 validation，即使 persistent cache 已经 warm。详细签名见 [Procedure Reference](../reference/procedures.md)。

## 历史检索和子图检索

与普通查询相同，通过第三个参数 `options.at` 选择历史 Schema / 数据，通过 `options.graphView` 选择可见子图。历史上尚未创建的索引不能仅因为当前 Branch 有同名索引而使用。Graph View 在检索的可见结果与 top-k 边界内生效，不是结果返回后过滤。

全文和向量可以在普通 Cypher 中组合过滤、子查询和投影；应用自己定义融合分数与排序逻辑。v0.2.0 不额外提供名为 `hybridSearch` 的专用 API，也不预定义 RAG 工作流。

## 导入 CSV

先准备本地 UTF-8 文件 `people.csv`：

```csv
id,name
alice,Alice
bob,Bob
```

以下是传入 `lithograph()` 的 Cypher 文本。`$source` 绑定应用确认过的完整 `file:///.../people.csv` URI，也支持 `http://` / `https://`；不要把示意省略号当真实路径：

```cypher
LOAD CSV WITH HEADERS FROM $source AS row
MERGE (p:ImportedPerson {id:row.id})
SET p.name = row.name
RETURN count(p) AS imported
```

结果应为 `2`。导入列是文本，数值使用 `toInteger()` / `toFloat()` 等显式转换。普通导入在一次查询中执行，失败回滚这次图写入；`lithograph_rows()` 禁止 LOAD CSV，即使它只返回读取内容。

超大导入需要独立 batch transaction 时，使用 **普通 Native execute**，且 connection 没有外层事务：

```cypher
LOAD CSV WITH HEADERS FROM $source AS row
CALL (row) {
  MERGE (p:ImportedPerson {id:row.id})
  SET p.name = row.name
} IN TRANSACTIONS OF 1000 ROWS
FINISH
```

每个成功 mutating batch 形成一个 Commit。后续 batch 失败不回滚前面已经提交的 batch，重试前必须检查业务导入进度。该形式不能从 SQL Bridge 或 Native explicit transaction 内调用。

LOAD CSV 使用宿主进程的文件和网络权限，不注入凭据。上层应用必须限制来源、大小、超时与网络访问；不要把不可信 Cypher 当作没有外部 I/O 能力的表达式。更多边界见 [事务](transactions.md) 与 [部署安全](operations.md)。
