# Vector 与 Managed Semantic

[设计入口](../design.md) · [开发状态与验收](../development/README.md)

本文件拥有 Raw Vector 与 Managed Semantic 的能力边界、Semantic IndexDefinition、Embedding Provider、query 与 Provider-owned cache 合同。公共 Index namespace 见 [Schema / Index](schema-and-indexes.md#index-types)，Lithograph storage 边界见 [Storage](storage.md)，external-I/O 与事务限制见 [接口合同](interfaces.md#external-io)。


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

`providerConfig` 允许 JSON-compatible 的 `null`、BOOLEAN、INTEGER、有限 FLOAT、STRING、LIST 与 STRING-key MAP；Node/Relationship/Path/Point/Temporal/Vector 等运行时值不能进入配置。Lithograph 对整个 Map 做稳定 canonical encoding、Schema hash 与 Diff/Patch/Merge，但不理解 `model`、`api_key`、`headers`、`timeout_ms`、`cache` 等 provider-specific key。Provider 自己定义允许的字段、默认值、优先级、cache 与验证。因为 `providerConfig` 属于 versioned IndexDefinition，其中任何字段——包括调用方选择直接写入的 credential、本机 cache path 或其它 runtime 配置——都会进入 SQLite database、Schema history、SHOW/Diff/Patch/Merge 和备份；Lithograph 不做 secret 识别、脱敏或自动改写。调用方若不希望 secret 进入历史，应使用 Provider 提供的 indirection 字段（例如 `api_key_env`），这是使用选择而不是 Kernel 强制策略。

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

`validate` 只能执行 bounded、本地验证；它不得联网、加载大型远程资源或做 corpus Embedding，因为 Schema writer 不能把外部等待包进 Commit transaction。`embedBatch` 可以使用网络或本地模型，必须保持 input/output 数量和顺序，返回每个恰好 `dimensions` 个有限 `FLOAT32` coordinate；数量、维度、NaN/Infinity 或 ABI shape 不符视为 Provider contract failure，不返回部分 Semantic 结果。Provider 可以在 callback 内按自己的 API/GPU 限制进一步 batching、cache lookup/publish 或其它 provider-specific optimization。

Lithograph 可以、并且对 Managed Semantic 大 work set 必须维护 **execution-local embedding work materialization**，用于同一次 execution内的 exact-text de-dup。Core 不调用 Provider-specific cache-key projection，也不新增 ABI；它使用自己能够确定的保守 execution embedding-space identity：

```text
execution_space = H(
  provider name,
  canonical full providerConfig,
  dimensions,
  coordinate type = FLOAT32,
  Provider semanticIdentity,
  execution-work encoding version
)
```

`similarity` 不改变 embedding vector，因此不进入 text->vector execution key；owner/HNSW state仍按完整 IndexDefinition包含 similarity。已经由本 execution成功取得的 `(execution_space, exact text) -> vector` 可以在后续 source batch/owner、甚至同一 execution后续 graph-state revision再次出现时复用，而不再次调用 Provider。由于 Core 对 `providerConfig` opaque，timeout/cache-path 等纯 operational config变化也会保守地产生新的 execution_space；这只可能少一次本 execution去重，不会错误复用向量。Provider自己的 persistent cache可以按 [OpenAI-compatible Provider](#openai-compatible-provider) 做更精确的 effective-request projection，两层 identity 不要求相同。

该 text->vector work state 生命周期绑定整个 execution，内存超过预算时使用 TEMP/disk spill，并在 execution terminal/cancel/drop后删除；它不是跨 query/connection/process 的 cache，不进入 Lithograph `main`、storage format、integrity/GC，也不对下一个 execution承诺 hit。跨 execution/process 的持久/长寿命 text -> Vector复用完全属于具体 Provider。

与之分开的 owner membership / score candidate / TEMP HNSW materialization 必须绑定具体 graph-state identity/revision 与 Graph View，因为 source/membership/create/delete 会改变“哪些 owner 使用哪些 text/vector”。Graph revision变化可以复用相同 embedding-space/text 的 execution-local vector，但不能复用旧 revision 的 owner/HNSW集合。

`semanticIdentity` 是 **runtime materialization identity**，不是 versioned Schema 字段。它必须是非空、bounded、纯本地取得并在一次 provider registration / connection lifetime 内保持不变；需要改变 identity 时应通过新的 provider registration/connection lifecycle 生效，而不是在执行中的 callback 后热切换。Lithograph 可以用它隔离 connection-local Semantic/HNSW derived materialization，避免 provider implementation/model mapping 改变后复用旧 TEMP 向量集合；Lithograph 不用它建立 persistent text -> Vector cache。它也不让 Lithograph 自动证明远程模型长期不漂移：要重现历史 Semantic 结果，部署仍必须固定兼容 Provider implementation、模型版本和外部资源。该边界与 Full-text 中“versioned tokenizer name/args 不等于二进制/词典快照”一致。

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

  semantic_identity: STRING?,

  cache: {
    enabled: BOOLEAN = false,
    path: STRING?,
    max_bytes: INTEGER = 1073741824
  } = {enabled: false}
}
```

未知 top-level key 返回 `INVALID_ARGUMENT`；`cache` 必须是 MAP，当前只允许 `enabled/path/max_bytes` 三个成员，内部 unknown key 同样返回 `INVALID_ARGUMENT`。`enabled` 必须是 BOOLEAN；`path` 若提供必须是 STRING；`max_bytes` 必须是 `1..=9223372036854775807` 的 INTEGER，省略使用默认 1 GiB。Provider 不维护 model whitelist，也不在 `validate` 中联网探测 model/credential/endpoint，且 `validate` 只验证 `api_key_env` 字段本身，不要求对应环境变量此刻存在。所有字符串必须是合法 UTF-8 且不含 NUL；`model` 必须非空；显式 `api_key` / `api_key_env` 若出现也必须非空。`cache` 省略或 `enabled=false` 时不读写任何 cache database；disabled 时即使提供 `path/max_bytes` 也只做静态 type/range validation，不访问 filesystem。`enabled=true` 时 `path` 必须提供且非空。Provider 不自动推导主数据库旁路文件、用户目录或 TEMP 路径，避免未配置的持久文件副作用。

`base_url` 必须是 absolute `http://` 或 `https://` URL，去除尾部 `/` 后不能为空，且不能带 query 或 fragment；请求固定发往 `<base_url>/embeddings`。Provider 不根据 URL 判断 vendor，也不阻止 caller 把 credential 发给 HTTP 或第三方 endpoint。HTTP redirect **不自动跟随**：任何 3xx 都按 endpoint error 返回，避免 custom authentication header 被底层 client 保留并转发到重定向目标，也保持 versioned `base_url` 是实际 credential/data destination 的明确合同。Provider 同时显式关闭 HTTP client 的环境 proxy autodiscovery；`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 等 process environment 不得成为未进入 `providerConfig` 的隐藏路由配置。v1 不提供 proxy config；需要代理时调用方应把可直接访问的兼容 endpoint 放在 `base_url` 前面。

Authentication 完全由 config 决定，HTTP header name 按大小写不敏感语义处理。若 `headers` 已显式提供 `Authorization`，直接使用该值并且不要求解析 `api_key` / `api_key_env`；否则若 `api_key` 存在且非空，生成 `Authorization: Bearer <api_key>`；再否则若配置了 `api_key_env`，每次 HTTP request 前从该环境变量读取 credential 并生成 Bearer header，变量不存在或为空则本次调用明确失败；三者都没有时不生成 Authorization。Provider 不再隐式读取 `OPENAI_API_KEY` 或任何其它环境变量。`api_key` 名称表示直接 Bearer credential，也可以承载兼容 endpoint 接受的其它 Bearer access token。

`organization` 与 `project` 分别生成 `OpenAI-Organization` / `OpenAI-Project`。Provider 默认生成 `Content-Type: application/json`，随后应用 `headers` 中的用户值；用户自定义 header **最后覆盖**同名生成 header，包括 `Authorization`、`Content-Type`、`OpenAI-Organization` 和 `OpenAI-Project`。因此兼容服务可以使用其它认证/header 约定；例如 `X-Client-Request-Id` 也可直接通过 `headers` 传递，Provider 不自动生成 request id。Header name/value 必须是合法 HTTP header；同一个 `headers` Map 中若存在仅大小写不同的重复 header name，配置无歧义地拒绝而不是依赖 Map iteration 顺序。Custom header values 总预算最多 60 KiB，最终 request headers 总大小最多 64 KiB。

HTTP body 总是包含 `model` 与 `input`。`send_dimensions = true` 时把 Semantic Index 顶层 `dimensions` 作为 OpenAI `dimensions` request field 发送；为兼容不支持该 optional field 的旧模型，`send_dimensions = false` 时省略 HTTP 参数，但 response 仍必须严格匹配 Semantic Index 的 dimensions。`encoding_format` 按 config 原样发送，`user` 存在时发送。Managed Semantic 的 input contract 只有 exact UTF-8 String，因此不暴露 OpenAI token-array input variant；这不是缺失配置，而是上层 source-data model 的固定类型边界。OpenAI 当前单 input 8192 tokens、单 request 总计 300000 tokens、数组最多 2048 items；Provider 不引入 tokenizer 或自动 truncate/chunk，只按 `batch_size`（1–2048）按 item count 分批，超出模型/token limit 由 endpoint 返回错误。

`encoding_format = "float"` 时解析 JSON number vector；`encoding_format = "base64"` 时解析 OpenAI-compatible base64 embedding payload并解码为 ABI 所需 FLOAT32 values。无论 wire format，response 必须满足：`data` 数量等于 input 数量、每个 `index` 唯一且在范围内、按 index 恢复原 input 顺序、每个 vector 恰好等于 Semantic Index `dimensions`、转换后全部 coordinate finite；任一不满足时整批失败，不返回部分结果。Response 中其它向后兼容新增字段忽略。

`timeout_ms` 必须为正且有实现上限；`max_retries` 必须非负且有实现上限；`batch_size` 范围固定 1–2048。408、429、5xx 与 transport failure 可按 bounded retry policy 重试，其它 4xx 默认不重试；若 response 带合法 `Retry-After`，在实现定义的最大 backoff 预算内优先采用。取消在发 batch、HTTP 返回后、重试前、backoff 期间与下一 batch 前检查；阻塞中的单次 HTTP system call 只受 `timeout_ms` 上界约束，v1 不声称可异步抢占任意第三方 HTTP stack。

Provider error、log、trace 和 test diagnostics 不得主动序列化完整 `providerConfig` 或回显 `api_key`、解析后的 `api_key_env` value、`Authorization` / custom secret header value；允许报告字段名、HTTP status、bounded endpoint、request id 和不含 credential 的结构错误。OpenAICompatible artifact 还必须把其 HTTP/TLS dependency graph 的 `log` compile-time level 固定为 `Off`，并在 extension load 时 fail closed 校验该条件；不得通过修改 host process 的全局 logger/max-level 来实现。这样 host 即使启用 TRACE，也不能让底层 HTTP wire trace 绕过 Provider 的 secret-safe diagnostics contract。调用方选择把 secret 写入 versioned config 与 Provider 自己在错误路径泄漏 secret 是两个不同边界，后者不被允许。

`semantic_identity` 是调用方可选的 versioned embedding-space salt，用于同一 `base_url/model` 名称背后的实际 embedding space 发生不兼容变化时主动隔离 Provider cache 与 Lithograph TEMP semantic materialization。它与 Provider ABI 自身固定的 implementation semantic identity 是两个不同输入：OpenAICompatible registration 的 ABI identity 只表示实现/行为版本，不编码某个 endpoint/config，因此同一 connection 可以安全使用多个不同 config。

OpenAI-compatible Provider 自己定义 persistent cache identity，不把完整 `providerConfig` 机械作为 key。Cache key 必须包含 Provider implementation semantic identity、cache-key encoding version、Semantic Index dimensions、调用方 `semantic_identity`、exact UTF-8 input text，以及所有可能改变实际 embedding vector 的**有效请求语义**；必须排除只改变执行方式或缓存运维的 `timeout_ms`、`max_retries`、`batch_size`、`cache.enabled/path/max_bytes`。Endpoint/model/request body 字段与最终 effective request headers 都属于保守的 embedding-space 输入；secret/header value 只以 cryptographic digest 参与 identity，不把原文写入 cache database。

Cache identity 在 Authentication/header precedence **全部解析完成后**计算，只纳入真正会发出的 effective header/value。若 custom `headers.Authorization` 已覆盖认证，则未使用的 `api_key` / `api_key_env` 不参与 cache key，也不为计算 cache key去解析对应环境变量；只有实际选择 `api_key_env` 作为 Bearer source 时才读取环境变量并把 secret digest纳入 effective Authorization identity，变量缺失/空值即使可能存在旧 cache hit也明确失败，因为本次有效 request identity无法成立。这样不同 config 只要最终有效 request完全一致就可以共享 Provider cache；credential/tenant/route真正变化则产生保守 cache miss，不能因同文本跨不同 effective request复用 Vector。改变 cache-key 编码规则时必须同时改变 encoding/schema version，不能让旧 entry 被新实现按不同语义解释。

Provider 可以把省略默认值和显式默认值规范化到同一有效请求语义，例如省略 `base_url` 与显式写入默认 URL 可以共享 cache；这种 normalization 只属于 OpenAI-compatible Provider 内部 cache implementation，不改变 Lithograph 保存的原始 versioned `providerConfig`，也不增加新的 Provider ABI。

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
      batch_size: 32,
      cache: {
        enabled: true,
        path: './openai-embedding-cache.db',
        max_bytes: 1073741824
      }
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

Node 的完整签名固定为 `db.index.semantic.createNodeIndex(indexName :: STRING, labels :: LIST<STRING>, sourceProperty :: STRING, options :: MAP)`；Relationship 对应上面的同形签名。Node 版本第二参数 `labels` 同样是非空 `LIST<STRING>`。Label/Type/name/property 为空、重复 target token、未知 option、非法 config type、dimensions/similarity 不合法、同名 Index 冲突或 Provider 未注册/ABI 不兼容/`validate` 失败，均在发布新 Schema/Branch head 前失败。CREATE 只做本地 Provider validation，不遍历 source data、不调用 `embedBatch`，也不打开或创建 Provider cache database。

普通 auto-commit 调用形成正常 Schema Commit；SQL explicit transaction 可以 staged create/drop Semantic Index，因为 create validation 无 external I/O，最终仍只发布一个 transaction Commit。Patch/Merge/Rebase/Revert 等若产生新增或改变的 Semantic definition，也必须在发布新 Schema 前执行同一 provider/validate 检查；纯历史 inspection、`SHOW INDEXES`、`DROP INDEX`、纯 ref move 和真正没有改变该 definition 的路径不要求 Provider 当前可用。

Semantic Index 与其它 Index 共用名称 namespace、Schema hash、Diff/Patch/Merge slot。`SHOW ALL INDEXES` 返回 source、provider 与完整 versioned `providerConfig` / options，因此调用方直接写入的 `api_key`、secret custom header、`cache.path` 等也按原值可观察；只有 `api_key_env` 实际解析出的环境变量值、Provider pointer 与 Provider cache database 的动态 stats/content 不属于 Schema introspection。`DROP INDEX name` 只删除逻辑 definition 并形成正常 Schema history；Lithograph 不打开、扫描或清理 Provider-owned cache。

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

Semantic query 按**调用该 procedure 时的当前 execution graph/schema state**解析 Semantic IndexDefinition 和 source Property，再使用该 definition 的 provider/config 生成 query Vector；调用方不能在 query 时重复或覆盖 model/dimensions/similarity。普通 committed read 的 state 是 pinned immutable Snapshot；read-write query / active SQL explicit transaction 则是 base Snapshot + 已完成前序 clause/execution 的 staged graph/Schema/Index state，因此前序新建/删除 owner、修改 source text、Label/Type membership，或前一个 transaction execution staged create/drop Semantic Index，都必须被随后 Semantic query正确观察。不能因为 Provider/HNSW 实现方便而回退读取 committed base definition/source。

当前 Graph View 必须在候选可见性与 top-k/skip/limit 之前生效，并针对该 staged revision 的实际 element membership 计算；Relationship 继续检查 relationship 与 endpoints 的可见性。`options.at` 选择历史 Snapshot 时，使用历史 IndexDefinition；不能借用 current Branch 或 staged transaction 的 provider config / HNSW cache。历史 definition 的 Provider 当前缺失时，仅实际 Semantic query/rebuild 失败，普通 graph read、Schema inspection 与 `SHOW INDEXES` 仍可工作。

Managed Semantic query 可以通过 `lithograph()` 与 `lithograph_rows()` 执行；network/local-model external I/O 本身不构成 streaming adapter 拒绝理由。普通 SQLite autocommit execution、caller-owned transaction 与 Lithograph explicit transaction 分别遵守各自事务语义；只有与 `CALL { ... } IN TRANSACTIONS` 等独立 transaction boundary 或其它明确 lifecycle 冲突时才返回 `TRANSACTION_BOUNDARY_REQUIRED`。只读 Lithograph `main` database 也可以执行 Semantic read query，因为 Lithograph 不再为 embedding result cache 写 `main`。

查询对 query text 与当前 Graph View 实际可见、需要参与候选计算的 source text 先查本 execution 的 spillable work materialization，再对尚未解析的 exact text 做 bounded batch de-dup并调用 Provider `embedBatch`；Provider 是否命中自己的 persistent/memory cache 对 Lithograph 不可见。Lithograph 对新返回结果验证数量、顺序、维度、FLOAT32 finite value 与 similarity 前置条件，成功后写入本 execution work state，使同一 exact text跨后续 source batch也不重复 Provider call。任一必要文本无法生成合法 Embedding时整个 query失败，不能静默漏掉 owner形成“成功的部分 top-k”。

由当前 execution 生成的 owner/vector/TEMP HNSW 仍属于 Lithograph Search derived data，与 Provider 的 text -> Vector cache 是不同职责。Committed state 的 TEMP Semantic/HNSW materialization 必须绑定目标 Commit/IndexDefinition、Provider semanticIdentity 与实际 Graph View/query lifetime；staged state 还必须绑定 transaction/execution state identity + 单调 state revision + staged IndexDefinition identity。任一前序 source/membership/definition mutation使相关 materialization失效或形成新 revision，不能把 committed base cache 或旧 staged revision 当成当前 state。只覆盖当前 Graph View 的 materialization不能冒充完整 Snapshot cache供更宽 view 复用。删除全部 TEMP/HNSW derived state 只会使后续 query 重新调用 Provider/materialize，不改变 canonical graph/schema/history。

Committed read Semantic query 与 `rebuild` 用 read guard pin 住目标 immutable Snapshot。普通 read-write execution 的 Semantic query 使用该 execution 已有 staged write/savepoint context，active Lithograph explicit transaction 则使用 transaction-local staged state；Provider 等待期间不能为了缩短 writer hold 偷换到另一 connection/旧 committed Snapshot。纯 committed read 的 Provider等待不要求 Lithograph 取得 `main` writer，但长 external call 可能延长 read view / WAL pin；read-write / explicit transaction 已经拥有 writer boundary 时会自然延长 writer hold，这是调用方选择该 query/transaction composition 的直接成本。

`db.index.semantic.query*` 是 graph-read procedure，可以按正常 Cypher composition 与同一 execution 的前后 graph mutation组合；它在自己的 clause boundary 只读取当时可见的 current staged state，不自己创建 Commit或独立 transaction boundary。若后续 graph mutation / constraint / serialization 使整个 ordinary execution rollback，Provider 调用和合法 Provider-owned cache entry仍按 [External I/O](interfaces.md#external-io) 的独立副作用边界处理。`db.index.semantic.rebuild` 则仍是 committed-target maintenance surface，不与 graph mutation或 active explicit transaction组合。

<a id="embedding-result-cache"></a>

## Provider-owned Embedding Result Cache

Lithograph **不拥有** `text -> vector` result cache：Core 不定义 persistent embedding cache table、connection-local Embedding LRU、space/text cache hash、cache publish/eviction、sibling SQLite connection 或 cache configure/stats/clear procedure。Lithograph 每次需要 Vector 时只通过已解析的 Provider 调用：

```text
embedBatch(exact UTF-8 text[])
  -> vectors[]
```

Provider 可以不缓存，也可以自行选择 memory、SQLite 或其它内部实现。Provider cache 不是 graph correctness source，不进入 Lithograph storage format、Commit/Layer/Schema hash、Diff/Patch/Merge、Branch/Tag、integrity check 或 GC；命中与否不能改变返回 Vector 的合同。Lithograph 也不向 Provider 暴露 graph owner、Commit 或 Index name 作为缓存必需输入，避免把上层 graph identity反向耦合进通用 embedding implementation。

`db.index.semantic.rebuild(name, version)` 保留为 Lithograph Semantic/HNSW maintenance surface，但不再承担“填充 Lithograph Embedding cache”的职责。它 pin 目标 committed Snapshot、枚举该 definition 的 String source、对本次 work set 做 exact-text 去重并调用 Provider，然后构建/替换当前 connection 的可重建 TEMP Semantic/HNSW materialization；不写 Lithograph `main`、不创建 Commit、不移动 ref。结果固定返回 `name, commit, indexedEntities, embeddedTexts`；`summary.queryType = "read"`，`summary.commit` 是实际 pin 的 target Commit，Provider 内部是否 cache hit 不属于 Lithograph result contract。因为 `version` 显式选择 committed target，rebuild 不在 active Lithograph explicit transaction 或 Merge candidate 中执行，避免把 staged/candidate context 与另一 committed version 混合；这项限制来自 version context，而不是 external I/O。

<a id="cache-policy-and-maintenance"></a>

## OpenAI-compatible Provider SQLite Cache

OpenAI-compatible Provider 在 `providerConfig.cache.enabled=true` 时使用 `cache.path` 指向的**独立、filesystem-backed SQLite database**保存 persistent embedding result cache。v1 的 `path` 是普通 filesystem path，不接受空字符串、内存数据库标记或 SQLite URI；相对路径按宿主进程工作目录解析，Provider 不自动创建父目录。Provider 打开/操作该 cache DB 仍必须使用加载该 Provider extension 的 **host SQLite runtime / loadable-extension API table**；不得为 cache 方便启用 bundled SQLite、静态/动态链接第二个私有 SQLite runtime，或让两份 SQLite library 同时进入同一 Provider artifact。

Cache DB 不能 `ATTACH` 到 Lithograph `main`，Provider 也不能在 Lithograph reserved namespace 创建 object、不能把 cache lifecycle 交给 Lithograph init/integrity/GC。Provider registration 必须保存 host connection 的 `main` database identity；enabled cache 首次打开后、写任何 cache schema/data 之前，必须确认 cache file 与 host `main` 不是同一底层文件，命中同一文件时返回 configuration error。cache omitted 或 `enabled=false` 时 Provider 不打开、不创建、不读取、不写入 cache file；enabled 时 path 必须由调用方显式提供。

Cache database 至少拥有 provider-specific marker/schema version 与一张 embedding entry table，逻辑数据为：

```text
cache metadata
  magic
  schema_version
  used_payload_bytes INTEGER

embedding entry
  entry_id       INTEGER PRIMARY KEY AUTOINCREMENT  -- never-reused FIFO order
  space_hash     BLOB(32)
  text_hash      BLOB(32)
  text_bytes     INTEGER
  dimension      INTEGER
  vector_blob    BLOB
  payload_bytes  INTEGER
  UNIQUE(space_hash, text_hash)
```

Cache DB 不保存 source/query 原文，也不保存 API key、Authorization/custom secret header 原文。`vector_blob` 使用 cache schema version 固定的 IEEE-754 binary32 little-endian coordinate sequence，长度必须精确等于 `dimension * 4`；这样同一 Provider cache file 不依赖 host CPU endian。Provider 对 effective request semantics 计算 `space_hash`，对 exact UTF-8 text 计算 `text_hash`；命中后必须同时验证 stored `text_bytes == current exact UTF-8 input byte length`、dimension、payload length 与 finite FLOAT32 values，任一不符都不能返回给 Lithograph。这个长度校验不把原文写入 cache，但能让损坏 row / 非预期 hash-key reuse fail closed。`used_payload_bytes` 必须是非负、可表示全部 retained entry payload sum 的 SQLite INTEGER；Provider 自己的 insert/delete/eviction 在同一个 transaction 中同步更新它，算术 overflow/negative/非法 metadata 视为 cache structural/I/O failure，不静默 wrap。单个 entry 可安全判定损坏且其 accounting delta仍可确定时，Provider 可以在短 transaction 中删除该 entry、同步扣减 accounting并按 miss重算；如果 payload/accounting 本身损坏到无法安全确定 counter delta，则按 cache structural/I/O failure处理而不是猜测修复。cache database marker/schema 不匹配、不是 Provider 自己创建的 database、无法安全打开/写入或 SQLite structural failure时必须显式失败，不能覆盖任意用户 database，也不能偷偷降级成 no-cache mode。

`entry_id` 只用于 Provider cache 的 FIFO age，不进入 `space_hash/text_hash`、不对外作为业务 identity。使用 `AUTOINCREMENT` 是为了删除/eviction 后也不复用旧高水位；不同 process共享同一 cache file 时，SQLite transaction负责分配全局单调 insertion ordinal。达到 SQLite sequence 极限导致无法继续插入时按 cache resource/I/O failure处理，不回退复用旧 ordinal。

`max_bytes` 是本次 Provider configuration 要求的 **vector payload budget**：`payload_bytes` 固定等于该 entry 的 `vector_blob` byte length，used payload 是全部 entry 的该值之和；SQLite page、B-tree、hash/metadata、freelist/WAL 等物理开销不计入。Provider cache schema 必须以 transactionally maintained used-payload accounting（metadata counter 或等价 O(1) state）支持 budget 判断，不能为了每次 query 的容量检查全表 `SUM` 扫描所有 entry；insert/delete/eviction 与该 accounting 在同一个 cache SQLite transaction 内更新。

每次 enabled `embedBatch` 打开/验证 cache 后、执行 lookup 前，若当前 used payload 已超过**本次** `max_bytes`，先在一个短 cache transaction 中按 oldest insertion/FIFO eviction 到预算内；如果已经在预算内，纯 cache hit 不产生 usage/last-access write。成功生成 miss 后的 publish transaction 再执行一次同样的 budget enforcement，保证函数返回时 retained payload 不超过本次 budget。SQLite 文件实际大小可以大于 `max_bytes`，DELETE/eviction 后也不承诺立即缩小或自动 `VACUUM`。不同 process/connection 共享同一路径时依赖 SQLite transaction/UNIQUE constraint做并发仲裁；若不同 Semantic Index 对同一路径配置不同 `max_bytes`，较小 budget 的调用会先逐出较大 budget 先前保留的 entry，这只影响 cache retention/performance，不影响 vector correctness。v1 不持久化一个覆盖 `providerConfig` 的独立 max-bytes policy source，也不增加 TTL、background worker、daemon、per-model quota 或严格 LRU。

Cache connection 对 `SQLITE_BUSY/SQLITE_LOCKED` 使用 Provider 内部的 bounded SQLite busy/retry policy，使正常的短并发 lookup-maintenance/publish transaction 有机会串行完成；该 policy 不是 HTTP `max_retries`，也不新增 providerConfig key。实现必须有有限总等待上限并观察 Provider cancellation；预算耗尽后按 cache `IO_ERROR` 返回，不能无限阻塞或偷偷绕过 enabled cache。并发 correctness 仍由 SQLite transaction + UNIQUE constraint决定，retry 只处理 transient lock contention。

容量不足本身不把合法 embedding 变成 Provider failure。若单个新 vector 的 `payload_bytes > max_bytes`，该 vector 正常返回给 Lithograph但不写 persistent cache；若同一成功 batch 的全部新 entry 总量超过 budget，可以先原子插入可缓存 entries再按 FIFO逐出（包括该 batch较早 entry），最终只保留满足本次 budget 的集合。无论 eviction结果如何，本次 `embedBatch` 已生成且校验成功的 vectors都按原 input完整返回；只有真实 cache SQLite I/O/structural failure 才按 enabled-cache failure contract使调用失败。

Provider 调用顺序是：

```text
validate effective request/cache config
  -> open/validate cache + enforce current max_bytes (if enabled)
  -> cache lookup exact texts
  -> de-duplicate misses
  -> OpenAI-compatible HTTP for misses
  -> validate whole returned batch
  -> atomic cache publish + eviction
  -> merge hits/misses back to original input order
  -> return vectors
```

HTTP/Provider failure、cancel、invalid response 或 vector validation failure不写 partial/negative cache。Cache publish 是 Provider 自己的 implementation step；enabled cache 的 SQLite I/O failure使该 `embedBatch` 失败，因为调用方明确要求使用这个 cache configuration。Lithograph 只看到 Provider success/failure，不观察 hit count、entry count、eviction 或 cache DB transaction。

<a id="failures-and-reproducibility"></a>

## Failure、历史与可复现性边界

Provider 未注册、ABI/version 不兼容或 config validation 失败：新增/改变 Semantic definition 的 Schema publication 失败；已有 definition 的普通历史/SHOW inspection 不受阻。**每次实际 Semantic query/rebuild 都必须先解析并验证目标 Provider 与当前 `semanticIdentity`，即使当前 connection 已有 TEMP Semantic/HNSW materialization 也不能让“Provider 未加载时能否查询”取决于偶然缓存状态。** Provider 缺失时明确失败，不静默换 provider/model，也不把另一个 Provider/materialization 的旧向量当成当前 source 的结果。

Remote Provider / Provider-owned cache error 继续只使用现有 `EmbeddingProviderV1` status，不为 cache 增加新 ABI：cache path 指向 Lithograph `main`、existing database 缺少正确 Provider marker、schema/cache-key encoding version 不兼容或其它明确配置冲突返回 Provider `INVALID_CONFIG`，经 Lithograph 映射为 `INVALID_ARGUMENT`；cache SQLite open/read/write/corruption 等外部 cache-file failure 返回 Provider `IO_ERROR` -> Lithograph `IO_ERROR`；OOM、disk-full、size/resource exhaustion 返回 `RESOURCE_ERROR`；Provider cancellation 返回 `CANCELLED` -> Lithograph `INTERRUPTED` + `SQLITE_INTERRUPT`。`STORAGE_ERROR` 保留给 Lithograph `main` / reserved internal storage，不用来描述独立 Provider cache file。

非法用户 Semantic options/config shape仍使用 `INVALID_ARGUMENT` / Schema command 对应的稳定错误。Provider 返回违反 ABI、数量、维度或 finite-number contract 的 payload fail closed，不能返回部分结果。Provider 自己可以把可安全识别的单个 cache entry payload corruption 当 miss重算，但 Lithograph 不感知、修复或校验 Provider cache database。

Versioned Semantic IndexDefinition 固定“应该使用的 provider name/config 和向量合同”，但不快照第三方二进制、本地模型文件或远程模型服务。相同 definition 在不同时间若运行时 provider 对同一 semanticIdentity 实际产生不同 Vector，Lithograph 无法从 SQLite 文件独立证明 bit-identical reproducibility；需要历史精确复现的应用必须 pin provider implementation、model/revision 与外部资源，或者使用 Raw Vector 把 Vector 本身作为 canonical Property 进入历史。

<a id="design-tradeoffs"></a>

## 设计取舍

Managed Semantic 使用单独 Index kind + procedures，而不是重载 `CREATE VECTOR INDEX`：Cypher 25 Vector Index 明确定义为对一个真实 vector property 建索引，`SEARCH` 的 `FOR` 接受 Vector/List expression；把 String property 偷换为自动 Embedding 会改变标准 observable semantics。Procedure surface 使用既有 `CALL` mechanism，但 `db.index.semantic.*` 名称本身是 Lithograph 扩展，不宣称属于 Cypher 25 compatibility matrix。

把 Provider 实现留给 SQLite extension、只让 Lithograph 定义最小 Embedding ABI，可以同时安装多个 provider并隔离 vendor-specific config，同时避免 Core 直接依赖 OpenAI/HTTP/GPU runtime。text -> Vector cache 与 remote/local model execution 同属具体 Provider：Lithograph 因此不需要 storage-format cache table、cache maintenance API 或写 `main` 的 publish path；代价是 cache policy/文件生命周期不再由 Lithograph 统一管理，不同 Provider 可以选择不同实现。Semantic query 在 Provider cache miss 时仍可能产生外部延迟/费用，且跨 runtime 的可复现性取决于 Provider identity discipline。需要 Vector 本身成为历史真源或完全可控复现时继续使用 Raw Vector。
