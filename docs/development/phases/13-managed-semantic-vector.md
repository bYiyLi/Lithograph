# Phase 13：Managed Semantic Vector / Embedding Provider

**状态：`in_progress`**

## 1. 目标与范围

在 Phase 00–12 已完成、v0.1.1 已发布的基线上，实现 [Design §11.6](../../design.md#116-vector) 已冻结的双轨 Vector 模型：**现有 Raw Vector Property / Vector Index / Cypher 25 `SEARCH` 完全保留**；新增 Managed Semantic Index，使一个 String Property 可以通过当前 SQLite connection 注册的 `EmbeddingProviderV1` 生成 derived Vector，并通过 `db.index.semantic.*` 查询、rebuild 与 cache maintenance 使用。

本 Phase 的产品行为只由 Design §11.6、§12、§14.3.2、§15–17 定义；本计划只安排依赖、实现顺序、验收与状态。外部证据见 [Embedding Provider 研究](../../research/embedding-provider-contract.md)。

本 Phase 包含：connection-local Provider ABI/registration、deterministic synthetic Provider oracle、独立 `openai-compatible` production/reference Provider、Node/Relationship Semantic IndexDefinition、create/query/SHOW/DROP/history/version operations、persistent Embedding Result Cache、format `3 -> 4` migration、query-memory cache、explicit rebuild、Graph View/history、external-I/O/transaction boundary、真实 SQLite extension integration 与 Raw Vector regression。

本 Phase **不**交付：OpenAI-compatible Embeddings profile 之外的 Chat/Responses API、Azure/OpenAI vendor 特例、BGE/Ollama 专有 API、Reranker、通用 AI plugin framework、新 Cypher grammar、`CREATE SEMANTIC INDEX`、`SEARCH ... FOR TEXT`、multi-source concat/chunking、Semantic arbitrary filtered top-k、Raw Vector HNSW 持久化或 KG OS 特例。Commit、push、发布/部署仍需要各自独立授权。

## 2. 前置条件与当前差距

- Phase 00–12 全部 `done`；直接依赖 Phase 01 SQLite extension boundary、Phase 07 versioned IndexDefinition、Phase 08 Raw Vector/HNSW/Search、Phase 09 version operation/Native transaction、Phase 11 storage format3/cache/read guard 与 Phase 12 provider-binding经验。
- 当前 `StandardIndexKind` 只有 Cypher 25 index families；`IndexConfiguration` 没有 Semantic provider config。
- 当前 Raw Vector 从真实 graph Property 读取向量，HNSW 是 connection-local TEMP derived cache；调用方必须自己生成/写入/query Vector。
- 提交基线 `1e6e078` 尚无 Provider ABI；当前未提交 worktree 已开始 public `EmbeddingProviderV1`、client-data registration fixture 与独立 OpenAICompatible Provider 的 13.1 实现，但尚未完成 Phase acceptance，不能按已交付能力引用。
- 当前仍没有可验收的 Semantic Index kind/config、text->Vector persistent cache、Semantic create/query/rebuild/cache procedures 或 format4 inventory；这些继续是 13.2–13.5 的主体差距。
- SQLite minimum 3.45.0 已覆盖 3.44.0 引入的 connection client-data API，因此不需要为了本 Phase 提高 SQLite minimum。

Phase 13 已开始实现，当前状态为 `in_progress`。只有 SV13 acceptance 全部有当前 worktree/revision 的真实证据后才能标记 `done`。

## 3. Feature 顺序

```text
13.1 Provider ABI + synthetic SQLite provider oracle + OpenAI-compatible Provider
 -> 13.2 Versioned Semantic IndexDefinition + create/SHOW/DROP
 -> 13.3 Format4 persistent Embedding cache + operational policy
 -> 13.4 Managed query + Graph View/history + TEMP HNSW reuse
 -> 13.5 Rebuild/batching/external-I/O/version-operation closure
 -> 13.6 Compatibility/performance/quality/docs closure
```

每个 Feature 完成 targeted verification 与本轮 review finding 修复后再进入依赖它的下一项；实现暴露的 public behavior gap 回到 Design 修订，不在 Phase 文件另建第二份产品合同。

### Feature 13.1 Provider ABI 与真实 SQLite fixture

**依赖：Phase 01/08/12；来源：Design §11.6.3、§15.1–15.2。**

定义可供普通 SQLite loadable extension 使用的 `EmbeddingProviderV1` public C header/FFI boundary：ABI version/struct size、opaque context、stable semantic identity、local `validate`、`embedBatch`、取消/错误与 destructor ownership。按 `lithograph.embedding.v1/<provider>` client-data key lookup；duplicate name 先查后拒绝，不静默 replacement。

新增最小 deterministic synthetic provider extension，真实通过 `sqlite3ext.h` / host API table 注册；支持至少两个 provider 名、不同 semantic identity/config、batch call counter、可控 invalid vector、IO/resource/interrupt failure。测试两种 extension load order 和每 connection 独立注册；生产 Core 不包含 synthetic provider name whitelist。

同时实现独立 `lithograph-openai-compatible` cdylib/SQLite extension：不依赖 `lithograph-core`，只消费 public Provider ABI；完整实现 Design 冻结的 OpenAICompatible Embeddings config surface。所有 endpoint/auth/request/header/execution 参数都来自 versioned `providerConfig`；不再用 extension-load 环境变量作为隐藏配置来源，只有显式 `api_key_env` 会在 request-time 读取它指定的环境变量。HTTP integration tests 使用本地 deterministic stub server，不需要真实 API key 或公网。

**主要位置**：公共 ABI header/ABI crate、extension bridge、`crates/lithograph-core` provider adapter、新 test-support C extension、`crates/lithograph-openai-compatible` 与真实 `.load` probe。不得把 HTTP/model dependency引入 Core，也不得增加 process-global provider registry。

**Acceptance**：SV13-01–05、SV13-29–35。

### Feature 13.2 Semantic Schema / Procedure surface

**依赖：13.1；来源：Design §11.6.2、§11.6.4、§11.3。**

新增 Semantic Index kind/config canonical encoding；实现 `db.index.semantic.createNodeIndex` / `createRelationshipIndex`，固定单 source Property、provider/config/dimensions/similarity、FLOAT32 output。Provider create validation 必须 bounded/local，不调用 `embedBatch`。接通 `SHOW ALL INDEXES`、通用 `DROP INDEX`、Schema hash/Diff/Patch/Merge/Rebase/Revert 与 Native staged schema publication；`SHOW VECTOR INDEXES` 和现有 Raw Vector DDL 不改变。

ProviderConfig 只接受 Design 允许的 canonical JSON-compatible values；未知 top-level semantic option、secret-like arbitrary data本身不做推断，但文档/fixture明确它会持久化并可见。历史/SHOW/DROP/纯 ref move 不要求 provider 当前可用；新增/改变 definition 发布前必须有 provider + local validate。

**主要位置**：`storage/schema_state.rs`、schema command/show/version paths、procedure registry/execution、semantic index module 与 tests。

**Acceptance**：SV13-06–10。

### Feature 13.3 Format 4 Persistent Embedding Result Cache

**依赖：13.2；来源：Design §11.6.6–11.6.7、§14.1、§14.3.2。**

将 `_lithograph_embedding_cache`、format4 exact inventory、`3 -> 4` migration 与 legacy read boundary落入 storage constants；实现 embedding-space/text domain-separated hash、cache lookup/batch insert、payload validation、oldest-entry/FIFO budget eviction。Raw source text不复制进 cache，query-only miss不持久化。

实现 `db.index.semantic.cache.configure/stats/clear`。Policy 只属于 database operational metadata，不进入 Schema/Commit；普通 cache hit 不更新 `main`。`clear`/eviction 不碰 graph/schema/history或 TEMP HNSW，失败/crash保持可重建状态。Read-only/legacy database、format-too-new、exact inventory/collision/integrity path全部闭合。

**主要位置**：storage schema/migration/integrity、semantic cache module、procedure registry/execution、format fixtures。

**Acceptance**：SV13-11–15。

### Feature 13.4 Managed query、历史与 Graph View

**依赖：13.3；来源：Design §11.6.5–11.6.8。**

实现 `db.index.semantic.queryNodes/queryRelationships`：目标 Snapshot 先解析历史 Semantic definition，query text使用同 provider/config，复用 persistent source cache + connection-local query LRU；source miss按 exact text key去重/batch调用 Provider，再构造 TEMP managed vector materialization并复用现有 HNSW search primitives。普通 query不写 `main`。

Graph View 必须在 top-k/skip/limit 之前约束 Node/Relationship/endpoint；on-demand fallback不得把不可见 owner 的 source text送给 Provider。只覆盖当前 Graph View 的 TEMP vector/HNSW 必须绑定 view identity 或 query lifetime，不能冒充完整 Snapshot cache。source value 仅 STRING参与，缺失/null/其它类型排除；任何必要 Embedding failure使整个 query失败。`lithograph_rows()` 因 external-I/O authority拒绝，普通 SQL scalar/Native execution可用；Native explicit transaction 在 Provider I/O 前拒绝。

**主要位置**：`query/semantic_index.rs` / `semantic_index/hnsw.rs` 的共享底层、procedure execution、Graph View/version resolution、query-local state/tests。不得改成自定义 Cypher function或给 `SEARCH` 增语法。

**Acceptance**：SV13-16–20。

### Feature 13.5 Explicit rebuild、batching 与 version-operation closure

**依赖：13.4；来源：Design §11.6.6–11.6.8、§12。**

实现 `db.index.semantic.rebuild(name, version)` 的两阶段 maintenance：读阶段 pin immutable target、枚举/去重 source、cache lookup并在不持有 SQLite single-writer ownership 时调用 Provider；provider全部成功后短 writer重新验证 target definition并原子发布缺失 persistent cache entries、执行容量淘汰，再为当前 connection构建 TEMP semantic/HNSW。全过程不产生 Commit/ref move。

覆盖 source mutation 后的新/旧 text cache复用、跨 Index/Branch/history shared embedding space、provider semanticIdentity切换、cache disabled/read-only/clear 后 fallback。Patch/Merge/Rebase/Revert publication 与 provider validation、GC/cache ownership、cancel/IO/resource/crash clean-up按 Design闭合。

**主要位置**：semantic maintenance、storage cache、version/schema publication paths、Native/SQL boundary 与 integration tests。

**Acceptance**：SV13-21–24。

### Feature 13.6 完整回归、性能、质量与文档收尾

**依赖：13.5；来源：Design §17、全局 Phase 完成标准。**

加入可计数 synthetic provider performance fixtures，证明 duplicate exact text 不线性放大 Provider calls、new connection在 persistent cache warm 后不再调用 provider、query-only repeated text在同 connection命中 memory LRU；记录 cache bytes/entry counts、batch call count与 rebuild writer hold。测试 corpus只需证明机制和资源边界；除非 finding 影响既有 scale contract，不重新跑无关 10M/100M graph tier。

运行 Raw Vector/Search/Full-text/version/storage migration/compatibility regressions与 repository gates；真实最低 SQLite 3.45.0 和冻结/current runtime执行 `.load Lithograph + .load synthetic embedding provider`。新 Provider ABI 和 format4 需要 Linux/macOS/Windows x64/arm64 compile/load artifact acceptance。实现完成时同步 guide/reference/compatibility supplemental inventory/CHANGELOG/Phase 状态；未实现前这些用户文档保持 v0.1.1 事实。

**Acceptance**：SV13-25–28。

## 4. Acceptance Matrix

| ID | 必须验证的场景 | 验收证据 / 判定 | 状态 |
| --- | --- | --- | --- |
| SV13-01 | client-data provider 注册、两种 load order、connection A/B 隔离 | 真实 SQLite extension probe；同 provider只在注册 connection可见 | `planned` |
| SV13-02 | duplicate provider name | 第二次注册明确失败，旧 pointer/state继续有效；不得触发 silent replacement | `planned` |
| SV13-03 | ABI version/struct size/callback/destructor | valid v1可调用；旧/大/缺 callback/malformed pointer fail closed；close exactly-once释放 | `planned` |
| SV13-04 | `validate` 与 `embedBatch` contract | validate无网络/无embed；batch顺序/数量/dimension/FLOAT32/finite/cancel/error受 synthetic oracle断言 | `planned` |
| SV13-05 | Raw Vector完全不依赖 Provider | Phase08 Vector/Search targeted tests无 provider仍全部通过；现有 DDL/SEARCH result不变 | `planned` |
| SV13-06 | Node/Relationship create、single source、多 Label/Type | definition round-trip；非空/重复/类型/unknown option负例完整 | `planned` |
| SV13-07 | providerConfig canonicalization | Map key order等价；非JSON-compatible value拒绝；config变化产生不同 Schema/index definition | `planned` |
| SV13-08 | create只做本地 validation | corpus/provider call counter证明 CREATE/Patch planning不执行 embed；失败不发布 Schema/head | `planned` |
| SV13-09 | SHOW/DROP 与 Raw Vector区分 | SHOW ALL返回 `SEMANTIC`；SHOW VECTOR排除；通用 DROP有效且无需 provider | `planned` |
| SV13-10 | Diff/Patch/Merge/Rebase/Revert/Native staged schema | provider/config/source变化是单一 Index slot change；publication失败原子 rollback | `planned` |
| SV13-11 | format3→4/fresh4/legacy/too-new | migration保留全部旧 hash/refs；只创建空 derived structures；旧 engine拒绝format4 | `planned` |
| SV13-12 | embedding-space/text cache identity | 同 space+exact text跨 owner/index/history共享；provider/config/dimension/semanticIdentity隔离；similarity不分裂 | `planned` |
| SV13-13 | persistent cache payload/integrity | 不保存 source raw text；dimension/FLOAT32/vector/blob/counter损坏不会产生正常 result | `planned` |
| SV13-14 | configure/stats/FIFO capacity | enabled/maxBytes持久化但不进Commit；write时逐出oldest；hit不写main；used/entries/spaces准确 | `planned` |
| SV13-15 | clear/read-only/cache-disabled/crash | clear只删embedding cache；SQLite file不承诺缩小；read-only/disabled仍可TEMP正确执行；失败无半entry | `planned` |
| SV13-16 | queryNodes/queryRelationships 基本结果 | query text→provider→vector→HNSW/scan；score排序/skip/required limit/0 limit/空图正确 | `planned` |
| SV13-17 | source type/exact text | STRING/empty/Unicode exact bytes；missing/null/non-string排除；无trim/lower/concat/truncate | `planned` |
| SV13-18 | Graph View before top-k / TEMP cache isolation | hidden Node/Relationship/endpoint不占limit，on-demand provider不接收不可见source；view-local materialization不被更宽/无view查询复用 | `planned` |
| SV13-19 | historical definition/provider missing | at Commit使用历史config；缺provider时SHOW/read可用、即使warm cache完整实际query/rebuild仍明确失败、无current-head借用 | `planned` |
| SV13-20 | adapter/transaction boundary | scalar/Native semantic query可执行；rows拒绝external-I/O；configure/clear/rebuild按maintenance边界拒绝rows/explicit tx；query/rebuild在provider I/O前拒绝explicit tx | `planned` |
| SV13-21 | batch dedup + cache hit | N个相同source同space最多一次provider input；已有persistent cache跨connection时仍验证provider/identity但零`embedBatch`调用 | `planned` |
| SV13-22 | query memory privacy | query-only miss不进main；同connection重复query命中LRU；connection close后消失 | `planned` |
| SV13-23 | rebuild两阶段外部I/O | provider调用期间不持single-writer；publish短transaction重新验证target；无Commit/ref move | `planned` |
| SV13-24 | failure/semanticIdentity drift | IO/resource/interrupt/invalid payload不negative-cache/不部分top-k；identity变化不读旧space | `planned` |
| SV13-25 | Raw Vector/HNSW/Full-text/version regression | Phase08/11/12 targeted suites与适用 compatibility inventory无未知退化 | `planned` |
| SV13-26 | provider-call/resource quantitative gate | duplicate/warm/query-LRU call counters、cache bytes、batch数、writer hold有可重复报告 | `planned` |
| SV13-27 | real SQLite + cross-platform ABI/storage | 3.45.0 + current/frozen runtime真实dual-extension smoke；六目标 artifact build/load/migration通过 | `planned` |
| SV13-28 | repository gates/docs/final review | fmt/clippy/tests/quality/CI、links/diff-check/无secret/无产物；Phase-level review finding闭合 | `planned` |
| SV13-29 | OpenAICompatible config schema / persistence | `base_url/api_key/api_key_env/model/send_dimensions/encoding_format/user/organization/project/headers/timeout_ms/max_retries/batch_size/semantic_identity` 全部 round-trip；unknown/type/range 负例；config进入 Schema/SHOW/Diff/Patch，含显式 secret 原值；省略默认与显式默认不被偷偷改写 | `planned` |
| SV13-30 | Auth / environment / header precedence | custom `Authorization` > `api_key` > `api_key_env` > no Authorization；自定义Authorization存在时不解析env；仅显式env名被读取，必要env missing明确失败；organization/project映射header；custom headers大小写不敏感覆盖且case-duplicate拒绝 | `planned` |
| SV13-31 | OpenAI Embeddings request completeness | local HTTP oracle断言 `POST <base_url>/embeddings`，body覆盖 `model/input/dimensions(optional)/encoding_format/user`；`send_dimensions` true/false均覆盖；input来自 exact String、dimension contract来自 Semantic Index，不建立重复 config field | `planned` |
| SV13-32 | float/base64 response contract | 两种 encoding format 都转成 ABI FLOAT32；response按index重排并校验count/index/dimension/finite；unknown extra response fields不破坏解析 | `planned` |
| SV13-33 | batch/token-limit/failure/retry/cancel / secret-safe diagnostics | batch_size 1–2048按item分批；不truncate/chunk/tokenize；empty/oversize由明确本地或endpoint错误；408/429/5xx/transport bounded retry，其他4xx不重试；Retry-After与cancel边界有oracle；sentinel api_key/env/header secret不出现在error/log/trace | `planned` |
| SV13-34 | cache identity / config change | 完整 canonical providerConfig + 固定实现 ABI semantic identity + dimensions隔离 space；修改endpoint/model/header/auth/runtime字段均不误复用旧space；同env名secret rotation默认复用，语义路由改变时必须通过`semantic_identity`显式隔离 | `planned` |
| SV13-35 | Provider独立 artifact / multi-config | crate不依赖`lithograph-core`；真实SQLite 3.45+/current可单独`.load`并注册；同connection两个Semantic Index可使用不同base_url/auth/model config，无process-global endpoint冻结 | `planned` |

## 5. 验证执行计划

实现时从 targeted 到 repository gate 扩大，具体 binary/test 名按实际仓库布局确定，不在计划阶段伪造尚不存在的命令。最低要求包括：

```text
Provider ABI unit/FFI tests
Semantic schema/procedure targeted tests
format4 migration/integrity/cache tests
managed query/rebuild/Graph View/history integration tests
Phase 08 Raw Vector/Search regressions
Phase 12 dual-extension/provider lifecycle regressions
applicable Cypher compatibility self-check/TCK inventory
real SQLite 3.45.0 + frozen/current dual-extension probes
cargo fmt --check
cargo clippy --locked --workspace --all-targets --all-features
cargo make quality
scripts/ci.sh
git diff --check
```

Provider fixture必须是真正 SQLite loadable extension，不以 Rust mock直接注入 Core pointer代替 ABI acceptance。OpenAICompatible 使用本地 deterministic HTTP oracle 覆盖完整 config/request/response/retry/cancel，不访问公网；测试 `api_key` 使用 synthetic value，`api_key_env` 使用 disposable test env name，不提交真实 credential。Synthetic provider继续证明通用 ABI/cache/error边界。

性能证据至少报告：unique source text数、duplicate率、provider input/call batch计数、persistent cache hit/miss/bytes、TEMP HNSW build次数、query first-row/full-consume、rebuild provider阶段耗时与writer wait/hold、长provider call期间read-guard/WAL pin。不得把 provider sleep/HTTP等待藏到未计时 setup，也不得用预热结果冒充 cold。

## 6. Review 与完成标准

Phase review必须同时检查：Raw Vector compatibility、Semantic definition/history、Provider ABI pointer/lifetime、OpenAICompatible 完整 config/secret 持久化与 header/auth precedence、external-I/O writer lifetime、Graph View provider-input isolation、cache key/容量/清理、format4 migration/integrity、failure cleanup、SQL/Native adapters与测试 oracle。

重点反例：把 String `CREATE VECTOR INDEX` 偷换为managed、在 graph mutation 内调用provider、provider duplicate静默覆盖、OpenAICompatible 在 load 时冻结单一 endpoint、偷偷读取未配置环境变量、删除/脱敏调用方显式 `api_key`、漏掉官方 Embeddings request field、custom header precedence错误、query miss隐式写main、把query-only text持久化、cache key漏model/config/identity、similarity错误进入space key、外部调用持有writer、Graph View query给hidden source做embedding、Provider错误返回部分top-k、Raw Vector regression被semantic fallback掩盖、format4表在format3静默出现。

SV13-01–35全部有当前 revision/worktree 的真实自动化/集成/人工 diff review证据，required repository gates通过，文档同步到实际实现，且当前 Phase scope无剩余 task-affecting finding后，才能把状态从 `in_progress` 改为 `done`。设计/计划完成本身不等于 Phase 13 实现完成。
