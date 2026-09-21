# 版本边界、类型与配置限制

当前 v0.3.0 发布基线。本页列出应用可依赖的入口约束，不把内部 cache budget、性能实测或机器容量当作通用硬上限。v0.2.0/v0.2.1 的历史 format4 行为仅适用于对应 release tag。

## 平台与存储

SQLite 最低 3.45.0，必须可加载扩展并支持 FTS5。仅 connection 的 main 承载 graph repository；单文件多 Branch 仍共享 writer。预编译包提供 Linux/macOS/Windows x64/arm64，不等于保证任意 Linux libc、任意旧 OS 或所有 SQLite binding 都相容；必须在实际部署运行时 smoke-test。

当前 fresh/current Lithograph storage format 为 **3**。Phase 15 不保留 format4 migration/compatibility；高于当前支持范围的数据库必须使用对应历史 binary/tag 处理，不能手工改 storage marker。内部 `_lithograph_*` namespace 保留，不插入、删除、重命名或加 trigger/index。

## 名称、Descriptor 与 JSON

Branch/Tag name 是大小写敏感 UTF-8，1–255 bytes。禁止 NUL / ASCII control、开头或结尾的 `/`、空路径段、`.` / `..` 路径段。main 不可删除。不要把这一名字限制自动套用于所有 Cypher Label / Property identifier。

Commit Descriptor 为 `commit/<64 hex>`；branch/tag Descriptor 为对应前缀加 name。query `branch` 只接受 name。revision/分页 limit 必须为正整数，cursor 原样传回同一查询起点/Session revision。

INTEGER 是 signed 64-bit；跨 JSON 的大整数用 [Integer tag](values.md)。SQL scalar 总 envelope 与每个单独 SQLite value 受宿主 `SQLITE_LIMIT_LENGTH` 等限制；没有一个与运行时配置无关的“无限 JSON 返回值”。过大结果应投影必要字段、分页或使用 streaming，但单行仍需要可编码。

Vector 支持 I8/I16/I32/I64/F32/F64 坐标；输入也接受相应 `INTEGER8/16/32/64`、`FLOAT32/64` 等别名。输出 canonical coordinateType 为 `I8` 等短名，不保证保留输入的别名拼写。Vector dimension 必须与 values 长度一致，坐标必须在对应类型允许范围内。

## Full-text indexConfig

| Key | 默认 | 允许值 |
| --- | --- | --- |
| `fulltext.analyzer` | `unicode61` | 非空、无 NUL、且当前 SQLite connection 能由 FTS5 成功构造的完整 tokenizer specification |
| `fulltext.eventually_consistent` | `false` | BOOLEAN |

通过 `OPTIONS {indexConfig:{...}}` 声明。specification 直接遵守 FTS5 tokenizer grammar，例如 `porter unicode61` 或 `unicode61 remove_diacritics 0 tokenchars '-_'`；Lithograph 不再把 `standard-no-stop-words` / `english` 映射到内置实现。第三方 tokenizer 由宿主 SQLite extension 在**每个实际 connection** 上注册；数据库只版本化 specification 字符串，不持久化 native 实现。派生缓存和 eventual-consistency metadata 不改变历史 Snapshot 的正确性要求。

## Vector indexConfig

| Key | 默认 | 允许值 / 范围 |
| --- | --- | --- |
| `vector.dimensions` | 未显式固定 | INTEGER 1–4096 |
| `vector.similarity_function` | `cosine` | `cosine`、`euclidean` |
| `vector.quantization.type` | `binary` | `none`、`scalar`、`binary` |
| `vector.quantization.enabled` | 未给出 | legacy BOOLEAN；未给 type 时 true→scalar、false→none |
| `vector.default_search_expansion_factor` | none:1.0；scalar:1.5；binary:3.0 | 有限数值，1.0–10000.0 |
| `vector.hnsw.m` | 16 | INTEGER 1–512 |
| `vector.hnsw.ef_construction` | 100 | INTEGER 1–3200 |

提供 type 时以 type 为准；新应用避免同时提供 legacy quantization flag。Full-text / Vector OPTIONS 只接受 `indexConfig` Map，其中仅允许各 family 的已知 key，值必须为 DDL 支持的 literal。不要把 Query options 放进 Index OPTIONS，反之亦然。

上述参数决定索引配置，不是延迟 SLA。合理维度、过滤条件、数据分布、Recall 与内存取舍应由应用自己的 workload 验证。

## Managed Semantic 限制

| 项目 | 当前限制 |
| --- | --- |
| source Property | exactly one Property；只有实际 `STRING` 值参与 |
| source transform | exact UTF-8 bytes；不 trim/lower/concat/chunk/truncate |
| dimensions | INTEGER 1–4096 |
| output coordinate type | 固定 FLOAT32，全部 coordinate 必须 finite |
| similarity | `cosine`、`euclidean` |
| query options | 必需 `limit >= 0`；可选 `skip >= 0`；未知 key 拒绝 |
| Lithograph Core persistent cache | 不提供；Core 只有 execution-local 去重/TEMP work state |
| OpenAI-compatible Provider cache | omitted/disabled 不触碰文件；enabled 必须显式 `path`，使用独立 SQLite DB |
| Provider cache budget | `max_bytes > 0`；FIFO enforcement，单 entry 超预算时结果仍可返回但不缓存 |
| OpenAI-compatible provider batch_size | 1–2048 items；不提供 tokenizer/chunk/truncate |
| OpenAI-compatible timeout/retry | bounded positive timeout 与有限 retry；具体范围由 Provider 本地 validation 固定 |
| OpenAI-compatible redirect | 不跟随；任何 3xx 作为 endpoint error 返回 |
| OpenAI-compatible proxy | 不自动读取 process environment proxy；v1 无独立 proxy config |

OpenAI-compatible Provider cache identity 包含会改变 embedding vector 的有效 endpoint/model/request/header/auth semantics 与 exact text；timeout/retry/batch/cache policy 等纯 operational 参数不分裂 embedding space。secret/header value 只以 digest 进入 cache identity，cache DB 不保存 raw text 或 credential。任何实际 semantic query/rebuild 即使 cache 已 warm 仍要求目标 Provider 当前可用。Provider config 中直接写入的 `api_key` / secret header 会进入 versioned history；`api_key_env` 只保存变量名。

## 不属于此产品表面的能力

没有独立 Server、远程图同步协议、Neo4j Bolt/HTTP 服务、账号角色管理、APOC 安装或 SDK 包安装承诺。v0.2.0 提供的是**独立 SQLite extension** `lithograph-openai-compatible`，不是 Kernel 内嵌模型服务。Graph View 是执行子图边界，不是安全授权。

更多兼容范围见 [Cypher Compatibility](cypher-compatibility.md)。依据：[Index config parser](../../crates/lithograph-core/src/query/schema/command/index_config.rs)、[Value types](../../crates/lithograph-core/src/cypher/value.rs)、[设计](../design.md)。
