# Changelog

Lithograph 的用户可见版本变化记录在此文件。版本遵循 Semantic Versioning；在 `1.0.0` 之前，minor release 仍可能包含不兼容的公开接口或存储合同调整。

## Unreleased

### Added

- 可选 execution option `memoryLimitBytes`：宿主显式限制受控运行时工作内存，省略保持既有行为；预算拒绝为 `RESOURCE_ERROR` / `SQLITE_TOOBIG`。预算包含中间值和结果收集，具体边界见 [Execution Options](docs/reference/execution-options.md#memory-limit)。

### Fixed

- 统一静态 Label、Relationship Type 和名称 predicate 的反引号解码，避免合法反引号名称查询错误返回空结果。
- 将 SQLite 中断检查传入长表达式和函数循环，使 `range()`、嵌套 `reduce()` 等计算响应取消并进入既有失败清理路径。
- 真实 SQLite interrupt 后核实 transaction 已实际 abort；取消 caller-owned SQLite transaction 中的写 execution 可能撤销该事务内先前未提交的宿主写入，状态确认与恢复边界见 [取消与连接池](docs/reference/transactions.md#cancellation)。

上述变化由 [Phase 19](docs/development/phases/19-query-correctness-resources.md) 管理，已完成开发验收，尚未发布；完整证据见该计划，不能用于推断 v0.3.2 发行制品已支持。

## 0.3.2

完整变化见 [发布说明](docs/releases/v0.3.2.md)；提交、远端验收与正式发布结果见 [Phase 18 发布交付记录](https://github.com/bYiyLi/Lithograph/blob/main/docs/development/phases/18-query-access-controls.md#v032-release-delivery)。

### Added

- query-local `readAccess` / `writeAccess`：查询属性可见性、公开图对象属性过滤、属性及操作写入限制，支持精确名/前缀/regex、函数与能力禁令；缺省不额外限制。
- `ACCESS_DENIED` category 对应 SQLite `SQLITE_AUTH`，scalar/rows、显式事务和独立/并发批次共用访问规则。

### Fixed

- Canonical graph/Schema 状态比较保留 signed zero、统一 canonical NaN；修复 Squash 重组保护字段、无关 Revert/Patch before 错误，以及 Diff/Merge/Rebase 和 opaque Schema definition 的相关路径。
- reduce 的规范函数识别、字符串插值内 COUNT/COLLECT/EXISTS 的图求值与策略传播、lazy branch 三值逻辑和列表/分组作用域；插值保留外联、版本组合与事务生命周期限制。
- 补齐保护写入、版本权限交集、流式失败/提前关闭/长度错误、Graph View、索引/Search、Provider/tokenizer 和 warm-cache 权限组合的回归。
- OpenAI-compatible Provider 首次并发初始化空 cache 时在 writer 仲裁后重查并验证 Schema，避免重复建表失败；严格拒绝非 Provider table/view/index/trigger 与 `sqlitex` 用户对象，保留合法 warm hit 不要求 writer 的行为。

### Compatibility and validation

- `CY25-2026.08`、storage format 3、既有 Commit identity 与 Provider SPI V1 不变；v0.3.1 的固定 tag/资产不变。
- 包含 Provider 修复的最终本机 quality/CI 均通过，各含 112 项 Core 回归；新 macOS arm64 本机包完整 smoke 与 canonical SQL 5/5 通过。后续远端与正式发布结果由上述 main Phase 18 记录维护。
- Provider 初始化修复已通过真实多进程旧红→新绿；SQLite 3.45.0 / 3.51.0 / 3.53.4 各 15 项 cache 检查通过，cache schema/key version、Provider SPI 与 graph storage 不变。已发布 v0.3.1 继续使用串行预热或默认禁用 cache 的规避。

## 0.3.1 - 2026-10-16

### Added

- `readOnlyNoExternalIO` execution option：在共享 Core prepare 阶段限制完整 AST 的写入和外部 I/O，支持 scalar、rows 与 active explicit transaction。
- 完整中文开发者手册、Cypher 语言与函数参考、分类 Procedure、Provider 配置/SPI，以及可执行文档示例、覆盖和链接门禁。

### Fixed

- UUID 字符串转换的 frontend 类型检查与实际 runtime 对齐。
- 修复 regex operator、双写引号、Map projection、子查询投影 alias/modifier、count DISTINCT-star 校验和 Full-text 纯负查询。
- INTEGER64 Vector List/String 构造保留原整数精度，严格拒绝整数文本越界。
- 修复 rows 的约束验证和 Full-text CREATE TEMP Schema 自锁；tokenizer validation 与原生 SQLite specification grammar 对齐。
- 更新 rustls 至 0.23.46，满足当前安全公告要求。

### Compatibility

- 默认执行语义、`CY25-2026.08`、storage format 3 与 Provider SPI V1 保持兼容。
- v0.2.x format 4 数据库仍须按历史升级边界处理，不能通过修改 marker 迁移。

## 0.3.0 - 2026-09-21

Phase 15 SQL-only execution、true streaming 与 Provider-owned cache release。

### Changed

- Application-facing query execution 收敛为 SQLite SQL-only；`lithograph()` 保留完整 JSON envelope，`lithograph_rows()` 改为 `columns -> row* -> summary` 的统一 execution event stream，并支持 mutation、`LOAD CSV`、Managed Semantic 与 `CALL { ... } IN TRANSACTIONS`。
- SQL explicit transaction 继续使用 `lithograph_tx_begin()` / `lithograph_tx_commit()` / `lithograph_tx_abort()`，transaction 内的 query execution 直接复用普通 `lithograph()` / `lithograph_rows()` surface；独立 `lithograph_tx_execute()` 已删除。
- Read、mutating `RETURN`、transaction subquery 与 `LOAD CSV` execution 改为 pull/backpressure streaming；必要的 semantic barrier 使用 bounded batch / TEMP spill，不再由 adapter 持有完整最终结果。
- Managed Semantic persistent text → Vector result cache 从 Lithograph Core 移到具体 Embedding Provider；`lithograph-openai-compatible` 可通过独立 SQLite cache database 提供 persistent Provider cache。

### Removed

- 删除 application-facing `lithograph_v1_*` query/transaction C ABI、`include/lithograph.h` 与相关 Native query acceptance surface；`EmbeddingProviderV1` SPI 保留。
- 删除 Lithograph-owned persistent embedding cache、format 4 cache metadata/table 与 `db.index.semantic.cache.*` procedures。

### Compatibility

- Embedding Provider ABI 仍为 V1，Cypher profile 仍为 `CY25-2026.08`。
- v0.3.0 支持 storage formats 1–3，fresh/current format 为 **3**；format 1/2 可通过 `lithograph_init()` 迁移到 3。
- **Breaking pre-1.0 change:** v0.2.0/v0.2.1 的 format 4 database 不支持直接 downgrade/migration 到 v0.3.0。需要迁移时应通过应用数据导出/重建建立新的 format 3 database。

## 0.2.1 - 2026-09-19

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
