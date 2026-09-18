# Vector 与 Managed Semantic

[设计入口](../design.md) · [开发状态与验收](../development/README.md)

本文件拥有 Raw Vector 与 Managed Semantic 的能力边界、Semantic IndexDefinition、Embedding Provider、query 与 cache 合同。公共 Index namespace 见 [Schema / Index](schema-and-indexes.md#index-types)，persistent cache 表与格式迁移见 [Storage format 4](storage.md#storage-format-4)，external-I/O 与事务限制见 [接口合同](interfaces.md#external-io)。


<a id="raw-and-managed"></a>

## 两套并列能力

Lithograph 同时保留两套互不替代的向量能力：

1. **Raw Vector**：调用方拥有 `VECTOR` Property。Vector value 是 canonical graph data，随 Node/Relationship Property 一起进入 Commit / Layer / Diff / Merge；调用方负责生成、更新和查询向量。标准 `CREATE VECTOR INDEX`、additional filtering properties 与 Cypher 25 `SEARCH ... VECTOR INDEX ... FOR <vector-expression>` 保持现有语义不变。
2. **Managed Semantic**：调用方拥有文本 Property；Semantic IndexDefinition 指定一个 Embedding Provider，把文本映射为 derived Vector，再复用 HNSW 相似度检索。生成 Vector 不是 Node/Relationship Property，不进入 Commit/Layer，也不从 Graph View 暴露；删除所有 Semantic derived data 不改变 canonical graph/schema/history，只会使后续查询重新 materialize。

Raw Vector 仍是需要 caller-owned embedding、Vector 本身进入历史、Cypher 25 `SEARCH` filter/composition 或非文本向量数据时的完整路径。Managed Semantic 只解决“文本由数据库托管 Embedding”的当前需求，不包装或废弃 Raw Vector。

Raw Vector property 保留 dimension 与 coordinate type。Vector index 使用 HNSW ANN architecture，并支持 Cypher 25 vector index metadata、similarity、additional filtering properties 与 `SEARCH` subclause。现有 HNSW physical graph 继续是 connection-local TEMP derived cache，可按 `(index definition, commit)` 重建；cache 缺失不能改变语义，没有 cache 时可以使用 exact scan correctness fallback。Managed Semantic 不把 Raw Vector HNSW 改造成第二套 persistent truth。

<a id="semantic-index-definition"></a>

## Semantic IndexDefinition

一个 Semantic Index v1 固定绑定：

- Node 的一个或多个 Label，或 Relationship 的一个或多个 Type；
- **一个** source Property；只有最终值为 `STRING` 的 owner 参与索引，缺失、`null` 或其它类型不生成 Semantic entry；
- `provider: STRING`：精确、区分大小写的 Embedding Provider 注册名；
- `providerConfig: MAP`：Provider 自己解释并验证的 versioned 配置；Lithograph 按原值 canonicalize、持久化和参与历史，不替 Provider 删除 credential、runtime 或其它字段；
- `dimensions: INTEGER 1–4096`，与 Raw Vector Index 的 dimension contract 一致；
- `similarity: STRING`：复用 Raw Vector Index 已支持的 similarity contract；
- managed embedding 的输出 coordinate type v1 固定为 `FLOAT32`。

v1 故意只允许一个 source Property，不定义 `title + content` 等隐式拼接、字段名注入、`null` 拼接或 normalization 规则。Embedding 输入是 Property 的精确 UTF-8 String value；Lithograph 不 trim、lowercase、切块或静默截断。未来只有出现真实 multi-source/chunking requirement 时才单独设计其 versioned transform contract。

`providerConfig` 允许 JSON-compatible 的 `null`、BOOLEAN、INTEGER、有限 FLOAT、STRING、LIST 与 STRING-key MAP；Node/Relationship/Path/Point/Temporal/Vector 等运行时值不能进入配置。Lithograph 对整个 Map 做稳定 canonical encoding、Schema hash、Diff/Patch/Merge 与 cache identity 计算，但不理解 `model`、`api_key`、`headers`、`timeout_ms` 等 provider-specific key。Provider 自己定义允许的字段、默认值、优先级与验证。因为 `providerConfig` 属于 versioned IndexDefinition，其中任何字段——包括调用方选择直接写入的 credential——都会进入 SQLite database、Schema history、SHOW/Diff/Patch/Merge 和备份；Lithograph 不做 secret 识别、脱敏或自动改写。调用方若不希望 secret 进入历史，应使用 Provider 提供的 indirection 字段（例如 `api_key_env`），这是使用选择而不是 Kernel 强制策略。

同一个历史 Semantic IndexDefinition 永远绑定同一个 provider name/config/dimensions/similarity。改变任一 versioned 字段属于 Index slot 更新，走正常 Schema history；不在原 definition 上热改配置。Similarity 只决定向量比较，不参与 Embedding Space identity；同一个 Provider/config 生成的 Vector 可以被不同 similarity 的 Semantic Index 复用。

<a id="embedding-provider"></a>

## SQLite Embedding Provider Contract

Embedding 实现是普通 SQLite loadable extension，与 Lithograph extension 平级；Lithograph 不建立动态库安装器、目录扫描、下载器或通用 AI plugin framework。Host 可以在同一个 `sqlite3*` connection 上加载 OpenAI-compatible、本地模型或其它 provider extension；每个真正执行 Semantic create validation、query 或 rebuild 的 connection 必须拥有所需注册。

SQLite 3.44.0+ 的 `sqlite3_set_clientdata()` / `sqlite3_get_clientdata()` 用于 connection-local provider pointer binding；Lithograph 的最低 SQLite 3.45.0 已覆盖该 API。每个 Provider 使用 versioned client-data key `lithograph.embedding.v1/<provider-name>` 注册一个 `EmbeddingProviderV1` pointer。Provider extension 可以先于或后于 Lithograph 加载，因为两者只通过同一 SQLite connection 的公开 client-data API 会合；Lithograph 不需要枚举 provider，IndexDefinition 已给出精确 name。

注册前必须先读取同名 key；已存在时返回错误，不允许依赖 `sqlite3_set_clientdata()` 的 replacement 行为静默覆盖另一个 Provider。Provider name 是非空 UTF-8、不得含 NUL，并按 SQLite client-data 的 `strcmp()` 语义区分大小写。Provider state 的 destructor 由注册 extension 交给 SQLite，connection close 时释放。Lithograph 取得 pointer 后至少验证 ABI version、struct size 和必需 callbacks，再调用 Provider；损坏/不兼容 pointer fail closed，不能按另一个 ABI 猜测布局。

`EmbeddingProviderV1` 的最小职责固定为：

```text
provider name (来自 client-data key)
provider semanticIdentity() -> stable STRING
validate(providerConfig, dimensions, FLOAT32) -> local validation only
embedBatch(exact UTF-8 text[]) -> FLOAT32 vector[]
cancel/error/lifetime boundary required by the C ABI
```

`validate` 只能执行 bounded、本地验证；它不得联网、加载大型远程资源或做 corpus Embedding，因为 Schema writer 不能把外部等待包进 Commit transaction。`embedBatch` 可以使用网络或本地模型，必须保持 input/output 数量和顺序，返回每个恰好 `dimensions` 个有限 `FLOAT32` coordinate；数量、维度、NaN/Infinity 或 ABI shape 不符视为 Provider contract failure，不允许写入 cache 或返回部分 Semantic 结果。Provider 可以在 callback 内按自己的 API/GPU 限制进一步 batching；Lithograph 在调用前先对相同 cache key 的文本去重。

`semanticIdentity` 是 **runtime cache-generation identity**，不是 versioned Schema 字段。它必须是非空、bounded、纯本地取得并在一次 provider registration / connection lifetime 内保持不变；需要改变 identity 时应通过新的 provider registration/connection lifecycle 生效，而不是在执行中的 callback 后热切换。Provider 在“同一 provider name + providerConfig 不再代表兼容 embedding space”时必须改变它；Lithograph 把它纳入 Embedding cache key，以防 provider binary/model mapping 升级后复用旧 Vector。它不让 Lithograph 自动证明远程模型长期不漂移：要重现历史 Semantic 结果，部署仍必须固定兼容 Provider implementation、模型版本和外部资源。该边界与 Full-text 中“versioned tokenizer name/args 不等于二进制/词典快照”一致。

SQLite 没有 Full-text FTS5 那样的标准 Embedding Provider API，因此本节只定义当前 Embedding 所需的最小 ABI。未来 Reranker 如果成为真实需求，应定义独立 contract；不得提前把 Tokenizer、Embedding、Reranker 压成一个 `execute(anything)` 通用接口。Provider 外部依据与采用边界见 [Embedding Provider 研究证据](../research/embedding-provider-contract.md)。

<a id="openai-compatible-provider"></a>

### OpenAICompatible reference Provider

Lithograph 同仓库交付一个独立的 `lithograph-openai-compatible` SQLite loadable extension，作为 `EmbeddingProviderV1` 的首个 production/reference implementation。它是与 Lithograph 平级加载的单独 shared library，不链接或调用 `lithograph-core` 内部 API；只依赖公开 Embedding Provider ABI、SQLite loadable-extension ABI 与 HTTP/JSON/base64 runtime。注册名固定为 `openai-compatible`，client-data key 为 `lithograph.embedding.v1/openai-compatible`。一个 connection 上的同一个 Provider registration 可以服务多个 Semantic Index；所有 endpoint/model/auth/HTTP 行为都来自每次调用传入的 `providerConfig`，不能在 extension load 时冻结为 process-global endpoint。

“OpenAI-compatible”v1 只承诺 **Embeddings HTTP profile**：向 `<base_url>/embeddings` 发送 OpenAI Embeddings 风格请求，并解析 `data[].index` / `data[].embedding` response。它不实现 Chat、Responses、Images、Audio 或 Azure 特有 deployment/api-version contract，也不声称任意自称 compatible 的服务一定满足本 profile。

当前 OpenAI Embeddings body 的静态 request fields 全部可配置：`model`、`dimensions`、`encoding_format`、`user`；其中 `input` 是 Lithograph 每次 `embedBatch` 传入的 exact source/query text，不是静态配置，`dimensions` 已由 Semantic Index 顶层字段拥有，不在 `providerConfig` 重复定义。Provider 同时暴露 endpoint、authentication、OpenAI request-context header、custom header 与 HTTP execution 配置。v1 `providerConfig` 完整 schema 为：

```text
{
  base_url: STRING = "https://api.openai.com/v1",

  api_key: STRING?,
  api_key_env: STRING?,

  model: STRING,                         # required
  send_dimensions: BOOLEAN = true,
  encoding_format: "float" | "base64" = "float",
  user: STRING?,

  organization: STRING?,
  project: STRING?,
  headers: MAP<STRING, STRING> = {},

  timeout_ms: INTEGER = 30000,
  max_retries: INTEGER = 2,
  batch_size: INTEGER = 32,

  semantic_identity: STRING?
}
```

未知 key 返回 `INVALID_ARGUMENT`；Provider 不维护 model whitelist，也不在 `validate` 中联网探测 model/credential/endpoint，且 `validate` 只验证 `api_key_env` 字段本身，不要求对应环境变量此刻存在。所有字符串必须是合法 UTF-8 且不含 NUL；`model` 必须非空；显式 `api_key` / `api_key_env` 若出现也必须非空。`base_url` 必须是 absolute `http://` 或 `https://` URL，去除尾部 `/` 后不能为空，且不能带 query 或 fragment；请求固定发往 `<base_url>/embeddings`。Provider 不根据 URL 判断 vendor，也不阻止 caller 把 credential 发给 HTTP 或第三方 endpoint。

Authentication 完全由 config 决定，HTTP header name 按大小写不敏感语义处理。若 `headers` 已显式提供 `Authorization`，直接使用该值并且不要求解析 `api_key` / `api_key_env`；否则若 `api_key` 存在且非空，生成 `Authorization: Bearer <api_key>`；再否则若配置了 `api_key_env`，每次 HTTP request 前从该环境变量读取 credential 并生成 Bearer header，变量不存在或为空则本次调用明确失败；三者都没有时不生成 Authorization。Provider 不再隐式读取 `OPENAI_API_KEY` 或任何其它环境变量。`api_key` 名称表示直接 Bearer credential，也可以承载兼容 endpoint 接受的其它 Bearer access token。

`organization` 与 `project` 分别生成 `OpenAI-Organization` / `OpenAI-Project`。Provider 默认生成 `Content-Type: application/json`，随后应用 `headers` 中的用户值；用户自定义 header **最后覆盖**同名生成 header，包括 `Authorization`、`Content-Type`、`OpenAI-Organization` 和 `OpenAI-Project`。因此兼容服务可以使用其它认证/header 约定；例如 `X-Client-Request-Id` 也可直接通过 `headers` 传递，Provider 不自动生成 request id。Header name/value 必须是合法 HTTP header；同一个 `headers` Map 中若存在仅大小写不同的重复 header name，配置无歧义地拒绝而不是依赖 Map iteration 顺序。Custom header values 总预算最多 60 KiB，最终 request headers 总大小最多 64 KiB。

HTTP body 总是包含 `model` 与 `input`。`send_dimensions = true` 时把 Semantic Index 顶层 `dimensions` 作为 OpenAI `dimensions` request field 发送；为兼容不支持该 optional field 的旧模型，`send_dimensions = false` 时省略 HTTP 参数，但 response 仍必须严格匹配 Semantic Index 的 dimensions。`encoding_format` 按 config 原样发送，`user` 存在时发送。Managed Semantic 的 input contract 只有 exact UTF-8 String，因此不暴露 OpenAI token-array input variant；这不是缺失配置，而是上层 source-data model 的固定类型边界。OpenAI 当前单 input 8192 tokens、单 request 总计 300000 tokens、数组最多 2048 items；Provider 不引入 tokenizer 或自动 truncate/chunk，只按 `batch_size`（1–2048）按 item count 分批，超出模型/token limit 由 endpoint 返回错误。

`encoding_format = "float"` 时解析 JSON number vector；`encoding_format = "base64"` 时解析 OpenAI-compatible base64 embedding payload并解码为 ABI 所需 FLOAT32 values。无论 wire format，response 必须满足：`data` 数量等于 input 数量、每个 `index` 唯一且在范围内、按 index 恢复原 input 顺序、每个 vector 恰好等于 Semantic Index `dimensions`、转换后全部 coordinate finite；任一不满足时整批失败，不返回部分结果。Response 中其它向后兼容新增字段忽略。

`timeout_ms` 必须为正且有实现上限；`max_retries` 必须非负且有实现上限；`batch_size` 范围固定 1–2048。408、429、5xx 与 transport failure 可按 bounded retry policy 重试，其它 4xx 默认不重试；若 response 带合法 `Retry-After`，在实现定义的最大 backoff 预算内优先采用。取消在发 batch、HTTP 返回后、重试前、backoff 期间与下一 batch 前检查；阻塞中的单次 HTTP system call 只受 `timeout_ms` 上界约束，v1 不声称可异步抢占任意第三方 HTTP stack。

Provider error、log、trace 和 test diagnostics 不得主动序列化完整 `providerConfig` 或回显 `api_key`、解析后的 `api_key_env` value、`Authorization` / custom secret header value；允许报告字段名、HTTP status、bounded endpoint、request id 和不含 credential 的结构错误。调用方选择把 secret 写入 versioned config 与 Provider 自己在错误路径泄漏 secret 是两个不同边界，后者不被允许。

`semantic_identity` 是调用方可选的 versioned cache-separation salt，用于同一 `base_url/model` 名称背后的实际 embedding space 发生不兼容变化时主动隔离 cache。它与 Provider ABI 自身固定的 implementation semantic identity 是两个不同输入：OpenAICompatible registration 的 ABI identity 只表示实现/行为版本，不编码某个 endpoint/config，因此同一 connection 可以安全使用多个不同 config。最终 cache key 已包含完整 canonical `providerConfig` + ABI semantic identity + dimensions，因此 v1 **保守地把任何 providerConfig 变化都视为不同 cache space**。这可能让仅改变 timeout/retry/api_key 的配置重新计算 embedding，但不会错误复用潜在不兼容的向量；未来只有在出现真实成本证据时才考虑 provider-specific semantic projection，不提前增加第二套 config canonicalization ABI。

`api_key_env` 的**环境变量名**参与 versioned config/cache key，但其运行时 secret value 不进入 SQLite。若同一个环境变量名在不改变 config 的情况下被切换到会路由不同 embedding space 的 credential/tenant，调用方必须同步改变 `semantic_identity`；普通 credential rotation 若服务语义不变则不需要。

Lithograph v1 不要求 Provider 把默认值回写成另一份 canonical config：例如省略 `base_url` 与显式写入 `https://api.openai.com/v1`、省略 `timeout_ms` 与显式写入 `30000`，虽然 Provider 执行语义相同，但原始 versioned `providerConfig` 不同，因此可以形成不同 Schema/cache identity。这是保守的正确性取舍；不为节省少量重复 cache 提前增加 provider-specific config-rewrite ABI。

由于整个 `providerConfig` 都进入 versioned Schema，直接使用 `api_key`、secret custom header 或其它敏感值会把该值保存到 Commit history、Diff/Patch、备份和 SHOW 可观察结果；这是调用方选择。使用 `api_key_env` 时数据库只保存环境变量名，实际 credential 不进入 SQLite history。

<a id="create-show-drop"></a>

## 创建、展示与删除

Managed Semantic 不新增 Cypher grammar。创建使用标准 `CALL <procedure>` 形式调用 Lithograph database procedures：

```cypher
CALL db.index.semantic.createNodeIndex(
  'document_semantic',
  ['Document'],
  'content',
  {
    provider: 'openai-compatible',
    providerConfig: {
      base_url: 'https://api.openai.com/v1',
      api_key_env: 'OPENAI_API_KEY',
      model: 'text-embedding-3-small',
      encoding_format: 'float',
      timeout_ms: 30000,
      max_retries: 2,
      batch_size: 32
    },
    dimensions: 1536,
    similarity: 'cosine'
  }
)
```

Relationship 使用：

```text
db.index.semantic.createRelationshipIndex(
  indexName :: STRING,
  relationshipTypes :: LIST<STRING>,
  sourceProperty :: STRING,
  options :: MAP
)
```

Node 的完整签名固定为 `db.index.semantic.createNodeIndex(indexName :: STRING, labels :: LIST<STRING>, sourceProperty :: STRING, options :: MAP)`；Relationship 对应上面的同形签名。Node 版本第二参数 `labels` 同样是非空 `LIST<STRING>`。Label/Type/name/property 为空、重复 target token、未知 option、非法 config type、dimensions/similarity 不合法、同名 Index 冲突或 Provider 未注册/ABI 不兼容/`validate` 失败，均在发布新 Schema/Branch head 前失败。CREATE 只做本地 Provider validation，不遍历 source data、不调用 `embedBatch`、不创建 persistent Embedding cache entry。

普通 auto-commit 调用形成正常 Schema Commit；Native explicit transaction 可以 staged create/drop Semantic Index，因为 create validation 无 external I/O，最终仍只发布一个 transaction Commit。Patch/Merge/Rebase/Revert 等若产生新增或改变的 Semantic definition，也必须在发布新 Schema 前执行同一 provider/validate 检查；纯历史 inspection、`SHOW INDEXES`、`DROP INDEX`、纯 ref move 和真正没有改变该 definition 的路径不要求 Provider 当前可用。

Semantic Index 与其它 Index 共用名称 namespace、Schema hash、Diff/Patch/Merge slot。`SHOW ALL INDEXES` 返回 source、provider 与完整 versioned `providerConfig` / options，因此调用方直接写入的 `api_key`、secret custom header 等也按原值可观察；只有 `api_key_env` 实际解析出的环境变量值、Provider pointer 和内部 cache stats 不属于 Schema introspection。`DROP INDEX name` 删除逻辑 definition 并形成正常 Schema history；Embedding result cache 不是某个 Index 私有资源，因此 DROP 不扫描或同步删除共享 cache entry。

<a id="semantic-query"></a>

## 文本查询 surface

Managed Semantic query 同样不扩展 `SEARCH` grammar，不提供 `lithograph.vector.embed()` 之类 Lithograph-specific expression。Raw Vector 继续使用 Cypher 25 `SEARCH`；文本查询通过：

```cypher
CALL db.index.semantic.queryNodes(
  'document_semantic',
  $query,
  {skip: 0, limit: 10}
)
YIELD node, score
RETURN node, score
```

完整签名固定为 `db.index.semantic.queryNodes(indexName :: STRING, queryString :: STRING, options :: MAP) :: (node :: NODE, score :: FLOAT)` 与 `db.index.semantic.queryRelationships(indexName :: STRING, queryString :: STRING, options :: MAP) :: (relationship :: RELATIONSHIP, score :: FLOAT)`。`limit` 是 `options` 中必需的非负 INTEGER，`skip` 省略时为 `0`；未知 key、错误类型和显式 `null` 返回 `INVALID_ARGUMENT`。v1 不提供 semantic-specific arbitrary filter map、query-time provider/model override 或 additional filtering properties：需要 Cypher 25 `SEARCH WHERE`、复杂 top-k filtering/composition 时使用 Raw Vector path。Post-`YIELD` filter 只过滤 procedure 已返回结果，不得描述成“filtered top-k”。

查询先按目标 Snapshot 解析 Semantic IndexDefinition，再使用该 definition 的 provider/config 生成 query Vector；调用方不能在 query 时重复或覆盖 model/dimensions/similarity。当前 Graph View 必须在候选可见性与 top-k/skip/limit 之前生效；Relationship 继续检查 relationship 与 endpoints 的可见性。`options.at` 选择历史 Snapshot 时，使用历史 IndexDefinition；不能借用 current Branch 的 provider config 或 HNSW cache。历史 definition 的 Provider 当前缺失时，仅实际 Semantic query/rebuild 失败，普通 graph read、Schema inspection 与 `SHOW INDEXES` 仍可工作。

Managed Semantic query 是拥有 network/local-model external-I/O authority 的 execution surface，因此 `lithograph_rows()` 返回 `READ_ONLY_ADAPTER`；普通 `lithograph()` 与 Native execution 可用。Native explicit transaction 从 `tx_begin` 起持有 single-writer ownership，`tx_execute` 不接受 `db.index.semantic.query*` / `rebuild`，在开始 Provider I/O 前返回 `TRANSACTION_BOUNDARY_REQUIRED`；普通 graph mutation 与 Semantic definition 的纯本地 Schema validation 不受影响。

查询不能为了 cache miss 隐式写 `main`。它优先读取 persistent source Embedding cache，再使用 connection-local query Embedding LRU / TEMP Semantic materialization；仍缺少的 exact text 才调用 Provider。query-only text 的新 Embedding 默认只进入 connection-local memory cache，不写 persistent table，避免任意搜索词成为数据库持久痕迹。Source cache 未预热时，查询可以对当前 Graph View 中实际需要的 source text 做 bounded provider batching 并在 TEMP 中完成正确 fallback；失败、取消或任一必要文本无法 Embedding 时整个 query 失败，不把相关 owner 静默漏掉形成“成功的部分 top-k”。

由 on-demand Provider miss 构建的 TEMP owner/vector/HNSW 如果只覆盖当前 Graph View，可见性 identity 必须进入 cache key，或直接限制为 query-local lifetime；它绝不能被标记成“完整 Snapshot + IndexDefinition”的全图 HNSW 供更宽 Graph View / 无 view 查询复用。反过来，如果全部 owner 的 embedding 已来自不需要 external call 的完整 persistent cache，可以构建完整 TEMP HNSW，再按现有 Vector search 的 visibility-before-top-k 规则查询。

Semantic query/rebuild 保持现有 read guard pin 住目标 immutable Snapshot。Provider 等待不持有 SQLite single-writer ownership，但长 external call 可能延长 read view / WAL pin；[Managed Semantic 验收](../development/phases/13-managed-semantic-vector.md) 必须测量这项 mixed-workload/resource 成本，不能把“没有 writer hold”描述成零并发代价。

<a id="embedding-result-cache"></a>

## Persistent Embedding Result Cache

为避免不同 owner、Index、Branch/Commit 中的相同文本反复调用外部 Provider，format 4 增加 database-local persistent **Embedding Result Cache**。它和 HNSW 是两层不同 derived data：Embedding cache 回答“某个 embedding space 下这段 exact text 的 Vector 是什么”；TEMP HNSW 回答“某个 Snapshot + IndexDefinition 中哪些 owner 最相似”。两者都不是 correctness truth，但 Embedding cache 可跨 Index/Commit/connection 复用。

Cache key 固定由：

```text
space_hash = H(
  provider name,
  canonical providerConfig,
  dimensions,
  coordinate type = FLOAT32,
  provider semanticIdentity,
  embedding-cache encoding version
)

text_hash = H(exact UTF-8 text bytes)

key = (space_hash, text_hash)
```

Index name、Commit/Branch、Node/Relationship identity 与 similarity 不作为独立输入参与 key。完整 `providerConfig` 仍参与 `space_hash`，其中任何显式字段都不能被排除；因此 Provider 配置中包含的 credential、HTTP headers、timeout、retry 等也会影响 cache identity。Provider 在运行时解析的 secret value 不会被 Engine 追加到 key；OpenAICompatible 的 `api_key_env` 与语义路由规则见 [OpenAICompatible reference Provider](#openai-compatible-provider)。相同 embedding space + exact text 即使来自不同 Semantic Index/owner/history，也只需要一个 cached Vector；不同 provider/model/config/semanticIdentity 必须隔离。Cache 不保存 query-only 原文，也不要求复制 source 原文；`text_hash` 使用 Lithograph 既有 domain-separated cryptographic hashing discipline，payload 额外保存 text byte length、dimension/coordinate metadata 与 Vector 以做结构检查。

Persistent cache 只在拥有明确 maintenance/write boundary 的路径填充。`db.index.semantic.rebuild(name, version)` 先 pin 目标 immutable Snapshot，枚举该 definition 的 String source、按 cache key 去重并读取已有 cache；Provider miss 的 `embedBatch` 发生在 SQLite single-writer ownership 之外。全部需要的结果成功后，短 write transaction 重新验证目标 Commit/definition 仍可解析，原子插入缺失 cache entries、执行容量淘汰并完成当前 connection 的 TEMP Semantic/HNSW materialization；它不创建 Commit、不移动 ref。Provider failure/cancel、target 被 GC、config mismatch 或 cache publish failure 不留下可复用半成品。

普通 `query*` 只读 persistent cache；其 Provider miss 使用 bounded connection-local memory/TEMP，不隐式更新 `main`。这样普通 query 继续满足“read 不持久化 derived data”的现有 invariant；需要跨 connection 消除 API 重算时由调用方显式 `rebuild`/预热。一个 Semantic Index source 改变后，旧 exact text cache entry仍可被其它 owner/history复用；新的 text 在下一次 rebuild 前可以由 query 临时计算。

构建/重建前先对 source text 去重，并以 Provider batch contract 调用；Provider 自己可以再按 remote/GPU limit 分批。只缓存完整成功、维度和有限值都通过检查的 Vector；timeout、429/5xx、interrupt、resource failure、ABI violation 等错误不做 negative cache。

<a id="cache-policy-and-maintenance"></a>

## Cache policy、清理与运维

Persistent Embedding cache 的容量策略是 **database-level operational config**，不属于任何 IndexDefinition，不进入 Commit/Schema hash、Diff/Patch/Merge。Format 4 在 `_lithograph_meta` 声明两项受支持 key：`semantic.embedding_cache.enabled`（BOOLEAN，默认 `true`）和 `semantic.embedding_cache.max_bytes`（positive INTEGER，默认 `1_073_741_824`，即 1 GiB）。用户显式设置后该值持久化到 database；更换 Branch/历史 Snapshot 不改变它。这个默认值是当前版本的 operations policy，不参与 Commit/Schema/storage identity，未来版本可以调整 fresh/default policy，但已显式保存的 database 配置不得被升级静默覆盖。

公开维护 procedures：

```text
db.index.semantic.cache.configure(options :: MAP)
  -> effective enabled/maxBytes

db.index.semantic.cache.stats()
  -> enabled, maxBytes, usedBytes, entries, spaces

db.index.semantic.cache.clear()
  -> deletedEntries, releasedPayloadBytes

db.index.semantic.rebuild(name :: STRING, version :: STRING)
  -> name, commit, indexedEntities, embeddedTexts, cacheHits
```

`configure` 只接受 `enabled` / `maxBytes`；未知 key、错误类型或 enabled=true 且非正 maxBytes 返回 `INVALID_ARGUMENT`。把 `maxBytes` 降到当前 `usedBytes` 以下时，configure 在同一 operational transaction 内按 oldest-entry/FIFO 立即逐出到新预算以内再返回。v1 不公开 TTL、refresh-after、per-provider quota 或严格 LRU 配置：Embedding Space identity 已处理 provider/config 变化，TTL 只会让相同输入周期性重新产生外部费用。Persistent cache 采用写入时的 bounded oldest-entry/FIFO eviction；普通 cache hit 不更新 `last_used_at`，避免 read query 为 LRU 触碰 `main`。

`enabled=false` 时 persistent cache 不读不写，现有 rows 保留直到显式 `clear` 或后续重新启用后的容量维护；Semantic query/rebuild 仍可用 Provider + TEMP/memory 正确执行。`clear` 只删除 Embedding Result Cache，不删除 canonical graph/schema/history，也不要求删除当前 connection 已经 ready 的 TEMP HNSW；后续 rebuild/query miss 可以重新生成。DELETE 后 SQLite page 可供后续复用但文件不保证立即缩小，Lithograph 不自动运行 `VACUUM`。

Query text 使用独立的小型 connection-local memory LRU；同 exact text 若已经存在 persistent source cache 可以直接命中，但 query-only miss 默认不写 persistent cache。Connection close 后 query LRU 消失。Canonical `lithograph.gc()` 继续可以清理不可达 Commit 对应的 derived index generation；共享 Embedding Result Cache 不以单一 Index reachability 作为 ownership，主要通过 maxBytes eviction 与显式 `clear` 回收，避免为“某个 Index 被 DROP”扫描并误删其它 Index 仍可复用的 embedding space。

`cache.configure`、`cache.clear` 与 `rebuild` 是明确的 operational-write / maintenance procedures：只能由 `lithograph()` 或普通 Native execution 独立调用，`lithograph_rows()` 返回 `READ_ONLY_ADAPTER`，Native explicit transaction / transaction-owning subquery / Merge candidate 返回 `TRANSACTION_BOUNDARY_REQUIRED`；它们不接受 `graphView`，`rebuild` 自己的 `version` 使用现有 `commit/`、`branch/`、`tag/` descriptor 并拒绝同时使用 `options.at`。这三类成功执行都不创建 Commit、不移动 ref，`summary.queryType = "version"`、`summary.commit = null`、graph/schema mutation counters 为 `0`。`cache.stats()` 是无副作用 read procedure，可从 streaming adapter 调用。`queryNodes/queryRelationships` 的 result contract仍是 graph read，`summary.queryType = "read"`，`summary.commit` 是执行开始时 pin 的目标 Commit；其 external Provider 调用不改变这一点。

<a id="failures-and-reproducibility"></a>

## Failure、历史与可复现性边界

Provider 未注册、ABI/version 不兼容或 config validation 失败：新增/改变 Semantic definition 的 Schema publication 失败；已有 definition 的普通历史/SHOW inspection 不受阻。**每次实际 Semantic query/rebuild 都必须先解析并验证目标 Provider 与当前 `semanticIdentity`，即使相关 persistent/TEMP cache 看起来已经完整也不能让“Provider 未加载时能否查询”取决于偶然 cache 状态。** Provider 缺失时明确失败，不静默换 provider/model，不返回其它 Commit 的 cache，也不把旧向量当成当前 source 的结果。

Remote Provider error 通过既有 `IO_ERROR`，内存/大小/磁盘资源问题通过 `RESOURCE_ERROR`，取消保留 SQLite interrupt；非法用户 options/config shape 使用 `INVALID_ARGUMENT` / Schema command 对应的稳定错误。Provider 返回违反 ABI、数量、维度或 finite-number contract 的 payload fail closed，不写 cache；内部 cache payload 损坏视为 derived corruption，失效并回到可正确重算路径，不能用坏 Vector 返回结果。

Versioned Semantic IndexDefinition 固定“应该使用的 provider name/config 和向量合同”，但不快照第三方二进制、本地模型文件或远程模型服务。相同 definition 在不同时间若运行时 provider 对同一 semanticIdentity 实际产生不同 Vector，Lithograph 无法从 SQLite 文件独立证明 bit-identical reproducibility；需要历史精确复现的应用必须 pin provider implementation、model/revision 与外部资源，或者使用 Raw Vector 把 Vector 本身作为 canonical Property 进入历史。

<a id="design-tradeoffs"></a>

## 设计取舍

Managed Semantic 使用单独 Index kind + procedures，而不是重载 `CREATE VECTOR INDEX`：Cypher 25 Vector Index 明确定义为对一个真实 vector property 建索引，`SEARCH` 的 `FOR` 接受 Vector/List expression；把 String property 偷换为自动 Embedding 会改变标准 observable semantics。Procedure surface 使用既有 `CALL` mechanism，但 `db.index.semantic.*` 名称本身是 Lithograph 扩展，不宣称属于 Cypher 25 compatibility matrix。

把 Provider 实现留给 SQLite extension、只让 Lithograph 定义最小 Embedding ABI，可以同时安装多个 provider并隔离 vendor-specific config，同时避免 Core 直接依赖 OpenAI/HTTP/GPU runtime。Persistent cache 只保存 text->vector derived result，不把外部 I/O 塞进 graph write；代价是 Semantic query 在未预热 cache 时可能产生外部延迟/费用，且跨 runtime 的可复现性取决于 Provider identity discipline。需要完全可控的历史向量仍使用 Raw Vector。
