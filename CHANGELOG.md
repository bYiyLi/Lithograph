# Changelog

Lithograph 的用户可见版本变化记录在此文件。版本遵循 Semantic Versioning；在 `1.0.0` 之前，minor release 仍可能包含不兼容的公开接口或存储合同调整。

## Unreleased

## 0.2.1 - Unreleased

SQL explicit transaction adapter release.

### Added

- 新增 `lithograph_tx_begin(options_json)`、`lithograph_tx_execute(query [, params_json [, options_json]])`、`lithograph_tx_commit()` 与 `lithograph_tx_abort()`，普通 SQLite driver 无需绑定 C ABI 即可把多次 Cypher execution 组合为一个最终 graph Commit。
- 新增真实 SQLite SQL transaction smoke，覆盖 staged read、read-only、empty-delta、abort、fail-closed、connection close、outer transaction、参数错误与 result-length cleanup；回滚验收同时检查 Commit、Layer、Schema object、Branch head 与 integrity。

### Changed

- SQL 与 Native explicit transaction 共用同一 connection-local state machine、staged storage、Commit finalize 和 fail-closed cleanup；普通 `lithograph()` 与 caller-owned SQLite transaction 语义不变。
- Linux/macOS/Windows x64/arm64 Release Matrix 对 SQLite 3.45.0 与 3.53.4 制品执行 Phase 14 SQL explicit transaction smoke。

### Compatibility

- Native ABI 仍为 1，Embedding Provider ABI 仍为 V1，Cypher profile 仍为 `CY25-2026.08`，storage format 仍为 4；从 v0.2.0 升级不需要 storage migration。
- 四个新 SQL function 使用 `SQLITE_DIRECTONLY`，不声明 deterministic 或 innocuous。

## 0.2.0 - 2026-09-19

Managed Semantic Vector / Embedding Provider release.

### Added

- Managed Semantic Index：Node / Relationship 的一个 String source Property 可通过 connection-local `EmbeddingProviderV1` 生成 derived FLOAT32 Vector，并通过 `db.index.semantic.queryNodes` / `queryRelationships` 查询。
- 新增独立 SQLite loadable extension `lithograph-openai-compatible`，提供 `openai-compatible` Embedding Provider；endpoint/model/auth/header/retry/batch 等配置全部来自 versioned `providerConfig`。
- 新增 persistent Embedding Result Cache 与 `db.index.semantic.cache.configure/stats/clear`、`db.index.semantic.rebuild` 运维入口；普通 semantic query 会自动缓存校验成功的 query/source Embedding 并跨 connection/process restart复用，正常搜索不依赖 rebuild/预热。

### Changed

- current main 的新数据库 storage format 提升为 **4**；`lithograph_init()` 可把 format 3 数据库原子迁移到 format 4，并保留既有 canonical Commit/Layer/Schema/refs。
- OpenAI-compatible Provider 的 HTTP route 现在完全由 versioned `base_url` 决定：不自动跟随 3xx redirect，也不从 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 等环境变量自动发现 proxy；底层 HTTP/TLS dependency logging 在 Provider artifact 中编译期关闭，避免 host TRACE 绕过 credential-safe diagnostics。
- Release packaging / artifact inspection 已扩展为同时处理 Lithograph 主 extension 与 OpenAI-compatible Provider artifact；Linux/macOS/Windows x64/arm64 六目标 hosted Release Matrix 已全部通过。

### Compatibility

- Raw Vector Property / Vector Index / Cypher 25 `SEARCH` 行为保持不变；Managed Semantic 是并列的 Lithograph procedure extension，不改变 `CY25-2026.08` 语言 coverage 分母。
- Native ABI 仍为 1，Cypher profile 仍为 `CY25-2026.08`。
- storage format 从 3 提升为 4；v0.2.0 可读取 formats 1–4，新数据库使用 format 4，`lithograph_init()` 支持 format 3 → 4 原子迁移。format 4 没有自动 downgrade，升级前必须保留完整备份。

## 0.1.1 - 2026-09-17

Full-text tokenizer provider binding update.

### Changed

- Full-text `fulltext.analyzer` 改为完整 SQLite FTS5 tokenizer specification，默认 `unicode61`；移除 `standard-no-stop-words` / `english` 特殊映射。
- Full-text DDL 在发布变更前使用当前宿主 connection 的真实 FTS5 constructor 验证 tokenizer；第三方 tokenizer 注册保持 SQLite connection-local。
- query-time analyzer override 改为原生 FTS5 QUERY/PREFIX tokenizer 委托，保留短语、布尔、prefix 与 colocated synonym 语义，不再使用去重词集合近似。

### Compatibility

- **Breaking full-text configuration change:** v0.1.0 的 `standard-no-stop-words` / `english` 不再是 Lithograph 特殊别名；已有 definition 不会被自动重写。升级后应显式改为宿主 SQLite 可构造的 FTS5 tokenizer specification。
- Native ABI 仍为 1，Cypher profile 仍为 `CY25-2026.08`，storage format 仍为 3；本版本不引入 storage migration。

## 0.1.0 - 2026-09-16

Lithograph 首个公开版本。

### Added

- 标准 SQLite loadable extension 与 Native ABI boundary，无独立 Server/Daemon。
- Property Graph：Node、Relationship、Label、Relationship Type 与 Property。
- 冻结的 Cypher 25 compatibility profile；全部 applicable inherited openCypher TCK 3,777/3,777 通过。
- Version-aware immutable storage、Commit DAG、Branch、Tag、Commit Data、Time-travel、Diff/Patch、Merge/Rebase/Squash/Reset/Revert 与 GC。
- Versioned Graph Type、Constraint、Standard Index、Full-text Index、Vector Index、`SEARCH` 与 `LOAD CSV`。
- storage format 3、persistent Standard Index、邻接 keyset、query-owned resolved state/read guard 与增量 index overlay。
- Linux x64/arm64、macOS x64/arm64、Windows x64/arm64 六个平台预编译 Release Assets。

### Compatibility and scale evidence

- SQLite 3.45.0 minimum 与 3.53.4 release-current runtime gate。
- 10M Node / 100M Relationship scale workload。
- 1M×128 与 100K×1536 Vector、1M Full-text corpus。
- 10,000-conflict Merge Session。
- 1/4/8 readers + 1 writer 各 30 分钟 mixed stress。

### Known limitations

- v0.1.0 是 pre-1.0 release；后续 minor release 仍可能调整 API、ABI 或 storage compatibility contract。
- storage format 3 没有自动 downgrade；升级已有 database 前应保留完整备份。
- 旧 26 GiB format 2 大库的完整 integrity migration 在现有测试窗口中曾超过 1 小时；v0.1.0 不承诺大型旧库迁移的低延迟。
