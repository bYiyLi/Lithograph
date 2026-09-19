# SQLite Embedding Provider / Cypher Vector Contract 研究证据

核验记录：2026-09-18 UTC；2026-09-19 UTC 补充 HTTP dependency logging 核验。仓库检查基线最初为 `1e6e078`；补充检查基于当前 Phase 13 worktree。本页保存 Phase 13 使用的外部合同、当前仓库观察和采用限制，不定义 Lithograph 产品行为；设计真源见 [Vector](../design/vector.md)。

## 1. 权威来源

| 标识 | 来源 | 本次用途 |
| --- | --- | --- |
| S1 | [SQLite Database Connection Client Data](https://www.sqlite.org/c3ref/get_clientdata.html) | connection-local named pointer、destructor、case-sensitive name、3.44.0+ availability 与不可枚举边界 |
| S2 | [SQLite Run-Time Loadable Extensions](https://www.sqlite.org/loadext.html) | 第三方能力以普通 SQLite shared-library extension 加载、共享目标 `sqlite3*` connection |
| S3 | [Cypher 25 `SEARCH`](https://neo4j.com/docs/cypher-manual/25/clauses/search/) | `SEARCH ... VECTOR INDEX ... FOR query_vector` 的 query-vector contract 与 filtered top-k |
| S4 | [Cypher 25 Index Syntax](https://neo4j.com/docs/cypher-manual/25/indexes/syntax/) | `CREATE VECTOR INDEX` 只索引一个真实 vector property、`WITH` 只增加 filter properties；Full-text/Vector procedure 入口 |
| S5 | [Cypher 25 Vector values](https://neo4j.com/docs/cypher-manual/25/values-and-types/vector/) | `VECTOR` dimension / coordinate type 与持久 Property 语义 |
| S6 | [OpenAI Create embeddings](https://developers.openai.com/api/reference/resources/embeddings/methods/create) | OpenAICompatible reference Provider 的 `/embeddings` request/response profile、input/model/dimensions/encoding_format/user 与 token/batch limits |
| S7 | [OpenAI API authentication](https://developers.openai.com/api/reference/overview#authentication) | Bearer credential、OpenAI-Organization/OpenAI-Project 与 custom request-header boundary |
| S8 | [ureq 3.x logging](https://docs.rs/ureq/3.4.1/ureq/#log-levels) | 当前 lockfile HTTP client 的 TRACE 为 wire-level 且不保证 redaction；secret-safe Provider 不能依赖 host logger filter |
| S9 | [log compile-time filters](https://docs.rs/log/latest/log/#compile-time-filters) | `max_level_off` / `release_max_level_off` 可在编译期移除 logging invocation；适用于独立 Provider artifact 的 fail-closed secret boundary |
| S10 | [ureq agent configuration](https://docs.rs/ureq/3.4.1/ureq/config/struct.Config.html) | 默认最多跟随 10 次 redirect；`max_redirects=0` 明确关闭 redirect；默认 Config 会从环境发现 proxy，`proxy(None)` 可显式关闭 |

网页会更新。本页只记录 2026-09-18 核验到的相关边界；Lithograph 的冻结兼容目标仍由仓库 `CY25-2026.08` Profile 与 Design 决定，不把 Neo4j/OpenAI 后续 additions 自动变成 Lithograph contract。

## 2. SQLite 已提供 extension 载体与 connection-local pointer

S2 规定 SQLite loadable extension 是独立 shared library / DLL，通过目标 database connection 加载，并可注册应用函数、virtual table 等扩展能力。由此可以让 Lithograph extension 与 Embedding provider extension 作为同一个 `sqlite3*` 上的平级扩展，而不需要 Lithograph 再实现动态库 loader、插件目录或独立 server。

S1 的 `sqlite3_set_clientdata()` / `sqlite3_get_clientdata()` 可以在一个 database connection 上按 case-sensitive name 关联 pointer；replacement 或 connection close 会调用注册时提供的 destructor。该 API 从 SQLite 3.44.0 起可用，低于 Lithograph 当前最低 SQLite 3.45.0 的门槛，因此 Phase 13 不需要提高最低 SQLite 版本。

S1 同时明确 client data 不是大规模 key/value store：当前实现使用 linked list，典型设计只放少量 names，也没有 enumeration API。因此 Phase 13 只用它保存实际加载的少量 Provider pointer；不会把文本/向量 cache entry 塞进 client data，也不依赖运行时枚举来解释历史 IndexDefinition。IndexDefinition 自己给出精确 provider name，Lithograph 按 versioned key 直接 lookup。

`sqlite3_set_clientdata()` 对同名调用会替换旧 pointer并触发旧 destructor，这不适合 Provider identity。Lithograph contract 因此要求 provider 注册前先 `get_clientdata()` 检测冲突并拒绝 duplicate；该规则来自产品一致性要求，不是 SQLite 自动提供的“禁止覆盖”语义。

## 3. SQLite 没有 FTS5 同类的标准 Embedding Provider API

SQLite FTS5 已经拥有 tokenizer registration/lookup contract，因此 Full-text 可以直接委托宿主注册 tokenizer。S1/S2 提供的是通用 extension/client-data 机制，不定义 `register_embedding_provider`、Embedding input/output shape、模型配置、维度或批量结果错误语义。

因此 Phase 13 需要一个 Lithograph-owned、版本化且尽量小的 `EmbeddingProviderV1` C contract，负责 `validate`、batch text -> vector、semantic/cache identity、错误/取消和生命周期；Provider 本身仍是普通 SQLite extension。这个 ABI 只解决已经存在的 Managed Semantic 需求，不自动扩张为 Tokenizer/Reranker/通用 AI plugin framework。

## 4. 标准 Vector Index 不能被 String source 偷换

S4 的 Cypher 25 Vector Index 只索引一个 property；该 property 是 Vector/List 数值空间，`WITH` 添加的是可在 `SEARCH WHERE` 中使用的额外 filter properties，不是第二个 vector/source property。S3 的 `query_vector` 是任何最终求值为 `VECTOR` 或数值 LIST 的表达式。

所以已有 Raw Vector 的标准闭环是：

```text
caller 生成/保存 Vector Property
 -> CREATE VECTOR INDEX ON (n.embedding)
 -> SEARCH ... FOR $queryVector
```

如果 Lithograph 把 `CREATE VECTOR INDEX ... ON (n.content)` 中的 String 自动送给 Embedding Provider，就会改变该标准 observable semantics。Phase 13 因此保留 Raw Vector 不变，并把 String -> managed embedding 定义成新的 `IndexDefinition(kind=Semantic)` + `db.index.semantic.*` procedure surface；它使用既有 `CALL` mechanism，但 procedure 名不是 Cypher 25 标准能力。

## 5. 当前 Lithograph 实现基线

| 位置 | `3ef8844` 的观察 |
| --- | --- |
| `storage/schema_state.rs` | `StandardIndexKind` 已有 `Vector`，`IndexConfiguration::Vector` 保存 dimension/similarity/HNSW config；没有 Semantic kind/provider config |
| `query/semantic_index.rs` | Raw Vector 从真实 indexed Property 读取 Vector，cache miss 先 exact scan，再构建 HNSW cache |
| `query/semantic_index/hnsw.rs` | HNSW 使用 `temp._lithograph_vector_cache*` connection-local derived tables；cache key 绑定 Snapshot/IndexDefinition/dimension |
| [Full-text](../design/full-text.md) | Full-text 已证明“SQLite extension provider + versioned config + connection-local availability + history/cache isolation”的可行边界 |
| [Persistent Standard Index Base + Delta](../design/schema-and-indexes.md#persistent-standard-index)、[Performance Storage Format 3](../design/storage.md#storage-format-3) | `main` persistent derived storage 必须显式升级 storage format；普通 read 不因 cache miss 隐式写 `main` |

这些现状支持最小 Phase 13：复用现有 HNSW，不顺带把 Raw Vector HNSW 持久化；只新增 Semantic IndexDefinition、Embedding Provider ABI 与跨 connection 的 text->Vector persistent cache。Raw Vector current tests 是 Phase 13 必须保持的回归 oracle。

## 6. 适用限制与未声称事项

- S1 只提供 pointer storage/lifetime，不验证任意 Provider struct 的 ABI；Lithograph 必须自己检查 ABI version/size/callbacks。
- Provider binary、模型文件、远程 endpoint 后面的实际模型 revision 都不由 SQLite client data 版本化。`semanticIdentity` 可以隔离 cache generation，但不能凭数据库文件证明远程服务永不漂移。
- Phase 13 现在同时使用 deterministic synthetic provider extension 验证 ABI/load-order/cache/error/cancel，并实现独立 `openai-compatible` reference Provider。后者只遵循 S6 的 Embeddings JSON shape，不能据此声称任意第三方“OpenAI-compatible”服务必然兼容；真实兼容性仍由对应服务实际行为决定。
- Managed Semantic 的 source text 可能被 Provider 发送到网络；这是 Host 加载/配置该 extension 后授予的 external-I/O authority，不是 Graph View 的认证机制。
- 本研究没有证明 multi-property text concatenation、chunking、Reranker 或 provider-specific secret/config schema；Design v1 明确不在这些方向提前增加合同。

## 7. OpenAI-compatible profile 的采用边界

S6 的当前 OpenAI Embeddings API 接受 string/string-array 或 token-array `input`、`model`、可选正整数 `dimensions`、`encoding_format = "float"|"base64"` 与可选 `user`；response 的 `data[]` 提供 `index` 与 embedding。官方同时明确空字符串非法、单 input 最多 8192 tokens、单 request 总计最多 300000 tokens、input array 最多 2048 项。Managed Semantic 的 source contract 固定为 String，所以 Provider 不暴露 token-array input；其它静态 request fields 全部由 Design 映射，`dimensions` 由 Semantic Index 顶层拥有并可通过 `send_dimensions` 决定是否发到 HTTP request。

S7 使用 Bearer credential，并支持 `OpenAI-Organization`、`OpenAI-Project` 与 custom request headers；官方建议 custom header values 总量不超过 60 KiB、request headers 总量不超过 64 KiB。Reference Provider 因此支持 `api_key` / `api_key_env`、organization/project 和 custom headers。按照当前产品决定，这些都是 versioned `providerConfig`；Lithograph 不替用户移除 secret。只有 `api_key_env` 实际解析出的环境变量值不进入 SQLite。

## 8. HTTP dependency logging 与 secret-safe diagnostics

当前 Phase 13 lockfile 解析到 `ureq 3.4.1`。S8 明确区分 DEBUG 与 TRACE：DEBUG 只展示 allow-listed header，而 TRACE 是 wire-level 且 **not redacted**。因此仅保证 Lithograph/OpenAICompatible 自己的 error string 不回显 credential，并不足以满足 Design 的“secret 不进入 log/trace”合同；如果 host 安装全局 logger 并打开 TRACE，底层 HTTP dependency 仍可能观察 request wire data。

S9 提供的 compile-time filter 会让被禁用级别的 logging invocation 不进入最终 binary。OpenAICompatible 是独立 SQLite `cdylib`，不是供上层 Rust 应用链接的通用 library；因此当前最小方案是在该 Provider crate 的 dependency graph 上显式启用 `log/max_level_off` 与 `release_max_level_off`，并由 extension init 校验 `STATIC_MAX_LEVEL == Off`，否则拒绝加载。Release 构建应把 Lithograph extension 与 OpenAICompatible Provider 分开执行 Cargo build，避免 workspace 多 package feature unification 把这一 Provider-specific logging policy带入 Lithograph Core artifact。该措施不调用 `log::set_max_level`，不会在运行时修改 host process 的全局 logger。

## 9. Redirect 与 credential destination

S10 记录 ureq 默认最多跟随 10 次 redirect，并允许通过 `max_redirects=0` 禁用。进一步检查当前 lockfile 的 `ureq-proto 0.6.2` redirect implementation：重定向时会按策略移除标准 `Authorization`，并移除 Cookie/Content-Length，但其它 custom header 会从原 request 保留到 redirect request。由于 OpenAICompatible 的 `headers` 明确允许兼容 endpoint 使用 `X-API-Key` 等自定义认证字段，默认 redirect 会让这些 credential 的实际 destination 脱离 versioned `base_url`。

因此 reference Provider 将 redirect 固定关闭，不新增 redirect 配置项；3xx 直接按非成功 endpoint response 处理。这既避免 custom secret header 跨 destination 转发，也保持 `<base_url>/embeddings` 是实际 request destination。若兼容服务需要重定向，调用方应直接把最终 endpoint 写入 `base_url`。

同一个 S10 还明确说明默认 Config 会从 process environment 发现 proxy。该行为与 OpenAICompatible 的 versioned config/cache identity 模型不兼容：环境中的 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 变化会改变实际网络 route，却不进入 `providerConfig`。因此 Provider agent 固定使用 `proxy(None)`，不读取环境 proxy。v1 没有已经确认的 proxy 配置需求，所以不为此新增一组 proxy schema；需要代理的部署应把代理/网关暴露为明确的 `base_url`。
