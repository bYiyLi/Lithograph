# Phase 15：SQL Execution Surface、True Streaming 与 Provider-owned Cache

**状态：`ready`**

## 1. 目标与范围

本 Phase 让当前 Phase 14 实现基线对齐最新设计：Application-facing execution 收敛为 SQLite SQL-only；`lithograph_rows()` 从 read-only row adapter 改为真正的统一 execution event stream；SQL explicit transaction 直接复用 `lithograph()` / `lithograph_rows()`；删除 application-facing Native query ABI 与 `lithograph_tx_execute()`；Managed Semantic 的 text -> Vector result cache 从 Lithograph Core 移到具体 Embedding Provider，OpenAI-compatible Provider 使用独立 SQLite cache database。

产品合同只由以下设计真源定义，本计划不复制行为细节：

- [SQLite SQL execution、streaming、结果与错误](../../design/interfaces.md#sql-bridge)
- [Explicit Transaction](../../design/storage.md#native-explicit-transaction)
- [Cypher Transaction Subqueries](../../design/storage.md#transaction-subqueries)
- [Query Engine cancellation / read guard / resolved state](../../design/query-engine.md#cancellation-and-connection-state)
- [Managed Semantic / Embedding Provider / Provider-owned Cache](../../design/vector.md)
- [Lithograph Storage Format](../../design/storage.md#storage-format-3)
- [Large-scale Invariants / Performance Evidence](../../design/runtime.md#large-scale-invariants)

本 Phase **不**修改 `CY25-2026.08` grammar/Profile，不删除 Raw Vector、HNSW、Full-text、Standard Index 或 `EmbeddingProviderV1` SPI，不修改 KG OS，也不引入 server/daemon/background worker。用户已明确当前产品没有旧 format 4 / Native query ABI 的历史兼容负担，因此本 Phase 不实现 format4 migration、Native compatibility shim 或旧 `lithograph_tx_execute()` alias。

## 2. 当前差距与前置条件

Phase 00–14 已完成，是本 Phase 的实现前置；它们的历史 acceptance 继续保留，不按新目标倒改为未完成。

当前实现与最新设计之间已确认的主要差距：

- `crates/lithograph-extension/src/rows.rs` 仍公开 `ordinal/columns/row`，并用 `READ_ONLY_ADAPTER` 拒绝 mutation、external I/O 与 transaction-owning execution；
- 简单 direct read path 已能按 bounded batch 消费，但部分 prepared program/composed/procedure read 仍通过 `program_rows` 完整保存结果；mutating `QueryCursor` 与 transaction program 同样会先保存完整最终 result rows，再由 adapter 分批返回；
- `lithograph_tx_execute()` 与 `lithograph_v1_tx_execute` 仍是独立 execution adapter，active explicit transaction 反而拒绝普通 `lithograph()` / `lithograph_rows()`；
- `CALL { ... } IN TRANSACTIONS` 当前只允许 Native autocommit path，SQL surface 仍返回 transaction-boundary error；
- application-facing `lithograph_v1_*` query ABI、`include/lithograph.h`、Native smoke/benchmark/release symbol gates 仍存在；
- Core 仍包含 format4 `_lithograph_embedding_cache`、metadata policy、persistent/LRU cache、sibling publish 与 `db.index.semantic.cache.*`；
- `lithograph-openai-compatible` 尚未实现 `providerConfig.cache` 与独立 SQLite persistent cache。

开始实现前必须重新检查这些路径及其调用方、测试和 release scripts，不能只按本清单机械删除；真实仓库 inventory 决定最终最小 diff。

## 3. Feature 顺序

```text
15.1 SQL transaction-boundary feasibility gate + shared incremental execution core
 -> 15.2 lithograph_rows event stream + cursor lifecycle
 -> 15.3 SQL transaction/external-I/O boundary closure
 -> 15.4 remove application Native query ABI
 -> 15.5 move embedding result cache into OpenAI-compatible Provider
 -> 15.6 compatibility/performance/release/docs closure
```

### Feature 15.1 SQL transaction-boundary gate 与 Shared incremental execution core

- **先执行 runtime feasibility gate，再重构 write/transaction program**：用真实 loadable extension / 最小 targeted probe 在最低 SQLite 3.45.0 与 current runtime 上先证明三件独立宿主事实：① eponymous `lithograph_rows()` virtual-table cursor 可以在同一个 host `sqlite3*` 上于第一次 runtime side effect 时建立普通 write SAVEPOINT、跨多次 `xNext` 保持该 boundary、写 `main`，并在 success terminal release、error/interrupt/`xClose` early-stop 时正确 rollback/release；② 自定义 rows `xClose` 返回非 `SQLITE_OK` 时，真实 host 通过 `sqlite3_step` / statement finalize 的可观察 result/error channel稳定传播该 cleanup failure，而不是静默丢失；③ 处于 autocommit mode 的 scalar callback 与 rows cursor 都可以在同一个 host connection 内执行 transaction-owning query所需的连续 inner `BEGIN/COMMIT/ROLLBACK` boundary。三项都不得依赖辅助 graph connection、SQLite fork、未公开 API或隐藏 Native query path；同时覆盖 outer statement teardown / connection usability。任一 gate 不通过时停止依赖它的 rows-write / transaction-owning SQL implementation，先修订 Design；
- 收敛 scalar collector 与 streaming consumer 到同一个 execution core；`lithograph()` 可以显式 collect 完整 envelope，但 Core 不为 `lithograph_rows()` 保留第二套执行语义；
- 把现有 `program_rows`/composed/procedure read 与 mutating execution 的公开 result production 从“完整执行后持有全部 rows”改为随 execution 增量推进；在 success summary 前保留可 rollback 的 write boundary；
- 把 transaction-program result 从完整 `Vec` materialization 改为按 inner batch 增量推进；inner batch 是 outcome barrier：batch 内 successful-row candidate增量写 bounded TEMP/spill，只有最终 attempt成功 commit后才逐行公开；CONTINUE/BREAK/RETRY failure丢弃 provisional successful rows并输出 Profile要求的 failure/status rows，不能为了真流破坏 transaction error semantics；
- transaction subquery 的 **upstream input 也必须增量进入 batch assembler**，不能继续先把 prefix/input 全部收成 `RowSet.rows/all_rows` 再切片。只有 prefix 自身含 ORDER BY/aggregation/DISTINCT 等 semantic barrier 时才允许对应 spill；`ON ERROR BREAK` 后未执行的 remaining input/status rows应继续从 upstream cursor增量转成 Profile要求的状态结果，不以预知全部 remaining rows 为前提；LOAD CSV -> IN TRANSACTIONS 同样使用 bounded batch/pipeline state；
- 把 execution terminal 改成 shared two-phase protocol：rows结束后先产生 terminal-ready summary candidate，普通 side effect仍可rollback；scalar 完整 envelope或rows summary payload编码/length/resource检查通过后才 finalize-success / release / install connection-local state。禁止 scalar 为了保留 late-failure rollback 再复制一套完整 executor；
- 区分 preflight failure 与不可避免的 post-finalize host handoff failure：普通 row/columns handoff error仍在 finalize前rollback；scalar 已 finalize 后的 `sqlite3_result_*` failure、或 rows summary row已 finalize 后的 `xColumn`/host NOMEM 不能反向撤销 durable side effect，必须返回真实 primary code并要求调用方通过 state/history确认后再决定是否重放；
- 对会创建 Commit 的 ordinary write，把 Storage finalization preparation 接入 terminal-ready：先冻结 `committed_at` / prospective Commit ID 并用于 summary preflight，adapter检查通过后才 publication/ref move；preflight失败不留下该 Commit，重试重新取得 timestamp/ID。`lithograph_tx_commit()` 使用同一 prepare-result-publication discipline；
- 同步 metrics boundary：`elapsedMicros` 在 terminal-ready 冻结，finalize-success 与 adapter serialization 由 benchmark 分阶段计量；scalar/rows 的同一 query 必须产生同口径 summary metrics；
- 保留真正需要 `ORDER BY`、`DISTINCT`、aggregation 等 semantic barrier 的 TEMP/disk spill；不得用无界 final-row collection 代替 barrier implementation；
- cancellation / interrupt / error / drop 必须能从任何未完成状态进入明确 cleanup，不留下 active SAVEPOINT、writer ownership、半 Commit/ref move 或不可复用 connection state。

### Feature 15.2 `lithograph_rows()` execution event stream

- 把 eponymous-only virtual table 改为设计定义的 `ordinal/event/data + hidden inputs` schema；
- 用 SQLite `SQLITE_VTAB_DIRECTONLY` 固定 rows module，只允许 top-level/direct invocation，拒绝持久化 view/trigger/schema-stored SQL 间接触发；
- 每个 scan 固定输出一次 `columns`、零到多次 `row`、成功后一次 `summary`；零 row 和无 RETURN mutation 也必须有 `columns` 与 `summary`；
- `xFilter` prepare 一次 execution 并先返回纯 metadata `columns` event，不提前触发 mutation/I/O/transaction batch；`xNext` 才增量推进该 execution，`xClose` / rescan / outer early-stop 正确 cancel/cleanup；同一 outer SQL 多次 scan 明确形成多次 execution；
- 当前 rusqlite vtab wrapper 的 `xClose` 只 drop cursor并固定返回 `SQLITE_OK`，不能承载 side-effecting stream 的 fail-closed cleanup。Phase 15 必须用最小 host-SQLite FFI/module shim接管 rows `xClose`（或等价可返回 error 的机制），正常 early-close 显式调用 execution cancel/rollback并把 cleanup failure映射回 SQLite；Rust `Drop` 只做不能报错的最终兜底，不得吞掉正常路径 rollback failure；
- public `summary` row 暴露后 cursor 立即标记 terminal-success；随后 `xClose` / statement finalize 只能释放资源，不能重复 cancel/rollback。覆盖普通 write、caller-owned tx、active Lithograph explicit tx statement 与 transaction-subquery outer success；
- `xBestIndex` 只把 hidden query/params/options 当 execution inputs；不把 visible `ordinal/event/data` 的 WHERE/LIMIT 等约束下推成跳过 Cypher work。验证 outer `LIMIT 1` 只消费 columns且无副作用、`WHERE event='row'` 会完整推进execution但可由SQLite隐藏summary、`WHERE event='summary'` 仍执行全部 row-producing work；
- 保留“顺序 multiple scans = multiple executions”，但增加同 connection execution-lifecycle guard：纯 read cursor 可共存，side-effecting stream 与另一个 graph/version execution 不得重叠形成 nested SAVEPOINT/staged-state ownership；
- 删除 rows adapter 的 `READ_ONLY_ADAPTER` query classification，并删除已经没有公开语义的 `READ_ONLY_ADAPTER` error category / mapping / tests；read/write authority 由 SQLite connection 与 Engine execution context决定；
- streaming write 在 summary 前输出的 row 是 provisional execution result；普通 execution failure/cancel/early-close 回滚未 durable write，不产生 success summary。
- version/connection-state procedure 同样遵守 terminal summary：例如通过 rows 执行 `branch.checkout` 时，summary 前的 early-close/error 必须恢复原 connection checkout，不能留下无 success summary 的 connection-local side effect；

### Feature 15.3 SQL transaction、transaction subquery 与 external-I/O closure

- 复用 15.1 已通过的 same-connection boundary evidence，把 transaction-program incremental state 接入两个 SQL execution surface；不得在本 Feature 重新引入第二 connection 规避 host boundary；
- 删除 SQL `lithograph_tx_execute()` 注册和实现；active `lithograph_tx_begin()` 后，普通 `lithograph()` / `lithograph_rows()` 自动进入同一 staged state，最终 `tx_commit` 只产生一个 Commit；
- active explicit transaction 中任一 scalar/stream execution failure、interrupt、cancel 或 incomplete cursor close 整体 fail-closed abort；
- lifecycle result failure：`tx_begin` 在 baseCommit result preflight失败时必须rollback且不留下 active tx；`tx_abort` rollback成功后即 terminal，即使结果 handoff失败也不能恢复/假装 transaction仍 active；`tx_commit` 继续遵守 prospective Commit/result preflight -> publication/COMMIT -> result handoff；
- active explicit transaction 中 `lithograph_validate/init/integrity_check` 按 Design 在产生歧义或维护副作用前拒绝，`lithograph_version` 保持纯 metadata 可用；
- 普通 SQLite autocommit SQL surface 支持 `CALL { ... } IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS`，并证明逐 batch transaction/Commit、ON ERROR/status、stream early-close 与 committed-batch preservation；caller-owned SQLite transaction 与 active Lithograph explicit transaction 继续拒绝该独立 boundary；
- `LOAD CSV`、Managed Semantic query 与其它普通 external I/O 可以通过两种 SQL execution surface；external I/O 本身不触发 rows adapter rejection。Semantic/Standard Index rebuild 仍按各自 committed-target maintenance lifecycle 决定是否允许 active explicit transaction；
- Managed Semantic query 在 read-write / explicit transaction 中必须读取当前 staged graph/Schema/Index revision：覆盖 prior execution/clause staged source-property、owner membership/create/delete，以及 staged Semantic Index create/drop；TEMP Semantic/HNSW 绑定 staged state revision，不能借 committed base cache 得到旧结果；
- read-only `main` connection 的纯读 execution 正常工作，mutation 由 SQLite/storage write boundary fail closed；Managed Semantic Provider cache 独立写入不能反向写 `main`。

### Feature 15.4 删除 Application-facing Native query ABI

- 删除 `lithograph_v1_execute`、`lithograph_v1_validate`、`lithograph_v1_tx_begin/execute/commit/abort`、`lithograph_v1_free` 及其实现/header/export；
- 删除只服务该 query ABI 的 Native callback/event/error ownership、smoke、examples、bindings、symbol checks 与 release acceptance；现有 Native scale/stream/mixed/semantic benchmark 中仍覆盖有效 workload 的部分，必须先迁移为 Core 或 SQL `lithograph_rows()` 等价 workload并证明口径不弱化，再删除旧 Native harness，不能通过删 adapter 顺手删掉性能/压力门禁；
- 把原 Native structured error 中仍属于 SQL public contract 的 source location 保留到唯一 SQLite error channel，严格输出 Design 定义的 category prefix + optional `[line=N,column=N]` suffix；删除 `READ_ONLY_ADAPTER` 后同步 Core/Extension error enum/mapping/tests；
- 修正 interrupt category：Core `Interrupted` / `SQLITE_INTERRUPT` 对外映射稳定 `INTERRUPTED`，不再误用 `RESOURCE_ERROR`；read/write/Provider/transaction-subquery cancel 都覆盖相同 primary code + category；
- `lithograph_version()` 不再返回 application query ABI version；同步 version/result fixture、reference 与 artifact inspection，避免删除 symbols 后仍通过 metadata 宣称存在该 ABI；
- 保留并回归 `EmbeddingProviderV1`、`include/lithograph_embedding_provider.h`、SQLite extension entrypoint 和 SQLite/Provider FFI；不能因名称中同样有 C ABI 就误删 Provider SPI；
- 更新 release artifact inspection，使“旧 Native query symbols 不存在”成为正向 acceptance，而不是继续要求这些 symbol。

### Feature 15.5 Provider-owned embedding result cache

- 删除 Core `embedding_cache` storage/module、format4 table/metadata/integrity/migration、connection-local Embedding LRU、sibling publish 与 `db.index.semantic.cache.configure/stats/clear` registry/tests；
- fresh/current Lithograph storage 回到 design 的 format3 exact inventory；不实现旧 format4 migration/compatibility；
- Managed Semantic 使用 execution-local spillable `(embedding-space,text)->vector` work materialization 对整个 execution跨 source batch/revision去重；text work state只绑定 execution + embedding space并在terminal后删除，owner/HNSW materialization另绑定 graph-state revision/Graph View；不恢复任何跨 execution Core cache。随后对 miss调用 Provider `embedBatch`并校验返回 vector，Provider persistent cache hit/miss对 Core不可见；
- 在 `lithograph-openai-compatible` 实现 `providerConfig.cache`：disabled 不触碰文件；enabled 要求显式 path，使用独立 SQLite DB、provider marker/schema、space/text hash、atomic publish、FIFO budget eviction、multi-connection/process SQLite concurrency 与 secret-safe key；
- Provider cache connection 必须继续走 host SQLite loadable-extension ABI/runtime，不启用 bundled/private SQLite；artifact dependency/symbol inspection证明 Lithograph 与 Provider 都没有第二份 SQLite runtime；
- cache identity 排除 timeout/retry/batch/cache policy 等纯 operational 参数，包含所有可能改变 embedding vector 的有效 endpoint/body/header/auth/request semantics；secret 只以 digest 进入 identity，不保存原文；
- 单 entry corruption 可安全失效并 miss/recompute；marker/schema/path/open/write 等无法安全处理的 enabled-cache failure 明确传播，不覆盖任意 DB、不静默切换 no-cache；
- `db.index.semantic.rebuild` 只负责当前 connection TEMP Semantic/HNSW materialization，不再报告/维护 Lithograph embedding cache；结果移除 `cacheHits`，固定为 `name/commit/indexedEntities/embeddedTexts`，summary 使用 read + target Commit contract。

### Feature 15.6 Regression、性能、发布与文档闭环

- 更新 SQL extension probes、compatibility supplemental inventory、performance runner 与 release scripts，删除 Native query / format4 旧门禁并增加 Phase 15 gate；旧 Native benchmark 的 traversal/streaming/mixed/Semantic 有效 workload 必须在新 Core/SQL runner 中有明确接替关系；
- 真实 SQLite minimum 3.45.0 + current runtime 覆盖 scalar/stream、write、explicit transaction、transaction subquery、external I/O、read-only main、Provider dual-extension/cache DB；
- Linux/macOS/Windows x64/arm64 Release Matrix 构建实际 artifacts 并验证 SQL-only symbol surface、Provider SPI、真实 `.load`；
- 对 large read、large mutating RETURN 与 transaction-subquery stream 记录 first-event、full-consume、peak RSS/TEMP/disk；不得用 `count(*)`、缩小结果或预 materialization 掩盖 true-streaming memory；
- 对 mutating rows 额外记录 writer-hold 与 caller backpressure：正常快消费、刻意慢消费、early-close 三类分别验证；不把“consumer 很快”冒充 writer lifetime与流式无关；
- 实现完成后再同步根 `README.md`、guide、reference、examples 与 release-facing 文档到真实行为：删除当前 `docs/reference/native-api.md` / Native transaction example 等不再存在的 application API 入口，更新 rows event schema、SQL explicit transaction、errors、Managed Semantic/Provider cache 用法和 procedure inventory；本设计/计划写入阶段不把尚未实现的行为标成已发布。

## 4. Acceptance Matrix

| ID | 必须验证的场景 | 验收证据 / 判定 | 状态 |
| --- | --- | --- | --- |
| EX15-01 | event schema / inputs / outer SQL / zero-row / no-RETURN | 真实 `.load` 后严格得到 `columns -> row* -> summary`，`ordinal/event/data` 与 tagged value/summary 合同一致；scalar/rows 对 required query、1–3/hidden inputs、omitted `{}` defaults、explicit NULL、non-TEXT、params/options JSON object/unknown-key 错误完全一致；visible-column constraints不改变execution：LIMIT 1只取columns且无副作用，WHERE row可隐藏summary但完整terminal，WHERE summary仍执行全部work；schema view/trigger间接调用被DIRECTONLY拒绝 | `planned` |
| EX15-02 | read true streaming | simple scan 与至少一组 program/composed/procedure 1M/10M 结果完整消费，first row 在 full execution 完成前可观察，额外 peak RSS 不与最终 row count 同阶增长 | `planned` |
| EX15-03 | mutating / ref / maintenance / connection-state true streaming | 大量 `... RETURN ...` 不保留完整 final row Vec；terminal-ready 冻结 prospective Commit ID 后的 adapter encoding/length failure、interrupt/early-close 均在 publication/finalize-success 前 rollback且无 durable Commit/public summary，重试取得新 finalization timestamp；scalar/rows 对 `branch.checkout` 等 connection-local mutation恢复原 state，对 Tag/Branch/Commit-Data/ref mutation不留下 ref/data side effect，对 Standard Index rebuild等 Lithograph-owned maintenance不发布新 generation；public summary已暴露后立即 xClose 不撤销成功状态；fault injection证明 rows real xClose rollback/release failure可传播，且 ordinary row-handoff failure在finalize前rollback、scalar/summary host-handoff failure在finalize后不反向撤销durable state | `planned` |
| EX15-04 | transaction-subquery streaming | minimum/current SQLite先通过 15.1 same-connection repeated-inner-transaction probe；ordered/concurrent/DISJOINT/RETRY fixtures保持 Phase08 语义；SQL scalar/rows不物化整个 program或无 barrier 的全部 upstream input，10M upstream / LOAD CSV 按 bounded batch推进，超大单 batch结果通过 TEMP/spill保持 bounded memory；successful rows仅在对应 batch最终 attempt commit后公开，CONTINUE/BREAK/RETRY failure不泄漏 provisional success rows而返回正确 failure/status rows；early-close对未 commit batch rollback、对已 commit/正在drain结果的 batch保留 durability并停止后续 batch；drain committed batch时故障注入 row encoding/result-handoff failure也不撤销该 batch；scalar最终 envelope/length failure同样不撤销 earlier durable batch | `planned` |
| EX15-05 | multiple scans / overlap | 一个 scan 恰好一个 execution；顺序 rescan 的 N scans 产生 N 次可证明 execution/side effect，不被 adapter 隐式去重；多个纯 read cursor 可共存，nested-loop fixture 若尝试在未 terminal side-effecting cursor 内重入 graph/version execution则稳定 `TRANSACTION_BOUNDARY_REQUIRED`，且不存在“内层已成功、外层 rollback 又撤销内层”的状态 | `planned` |
| EX15-06 | transaction-context durability | `tx_begin -> lithograph/rows* -> tx_commit` 观察 staged state并只产生一个最终 Commit；任一 execution/cancel/incomplete stream 整体 abort，但 rows statement 已暴露 summary 后的 xClose 不 abort active tx；begin result-preflight failure无 active tx，tx_commit在 publication前完成 prospective ID/result preflight且 failure无 durable Commit，abort一旦rollback成功即使结果-handoff failure也保持 terminal；`tx_execute`不存在。另验证 caller-owned SQLite transaction 中 rows mutation收到 summary后 outer `ROLLBACK` 仍删除对应 Commit/ref，outer `COMMIT` 才对其它 connection durable；普通 autocommit/explicit/IN TRANSACTIONS 四种 summary含义与 Design一致 | `planned` |
| EX15-07 | transaction boundary negatives | caller-owned/active explicit transaction 中 `IN TRANSACTIONS` 在 side effect 前稳定拒绝；普通 autocommit path完整执行 | `planned` |
| EX15-08 | external I/O / read-only/staged state | `LOAD CSV` 与 Managed Semantic 可经 rows执行；read-only main 的 Semantic read成功且不写 main，mutation按实际 SQLite/storage boundary失败；explicit tx / read-write query 中 Semantic query观察 staged source/membership/definition create/drop并不复用旧 committed/staged HNSW；外部 request/Provider cache 与 Lithograph main rollback边界分离，失败 execution 不留下 canonical write但不要求撤销已完成 Provider cache entry/网络费用 | `planned` |
| EX15-09 | scalar/stream semantic parity | 同 query 的 columns/rows/summary/value/error/Commit semantics一致，只允许 consumption shape 与 scalar resource limit 不同；当前公开 error inventory 不再含 `READ_ONLY_ADAPTER`，parse/semantic/type/schema error 的 SQL text 保留精确 category 与 1-based line/column suffix，interrupt 统一为 `INTERRUPTED` + `SQLITE_INTERRUPT` 而非 `RESOURCE_ERROR` | `planned` |
| EX15-10 | Native query ABI removed | artifact 不导出 `lithograph_v1_*` query symbols/header contract，`lithograph_version()` 不再报告 application query ABI，repository/release gate无旧要求；`EmbeddingProviderV1` dual-extension smoke继续通过 | `planned` |
| EX15-11 | format3 / Core cache removal | fresh/init/integrity exact inventory无 format4/cache table/meta，Core 无 persistent/LRU/sibling/cache procedures；同一 execution 跨多个 source batch 的重复 exact text只调用 Provider一次并使用 bounded/TEMP work state，execution结束即消失；Semantic rebuild新结果/summary合同、Raw Vector/HNSW/Standard Index regressions通过 | `planned` |
| EX15-12 | OpenAI cache config/lifecycle | cache 子对象 unknown/type/range validation闭合；omitted/disabled不触碰文件；enabled path通过 host SQLite runtime创建/复用独立DB且拒绝 host main同一文件，artifact无 bundled/private SQLite，固定 little-endian FLOAT32 payload、non-negative/overflow-safe transactional `used_payload_bytes` accounting、lookup前与publish后都执行当前 max_bytes/FIFO enforcement（已在budget内的纯hit无写）、文件可大于budget、单vector>budget时仍成功返回但不缓存、oversized batch最终保留集合≤budget、reopen、不同max_bytes共享path、multi-connection/process bounded BUSY/LOCKED retry + cancel、atomic failure通过 | `planned` |
| EX15-13 | OpenAI cache identity / secrets | request-semantic change正确隔离，timeout/retry/batch/cache-policy change可复用；runtime credential/header route安全隔离，cache DB/log/error不含 secret/raw text | `planned` |
| EX15-14 | Provider cache corruption/failure | hit校验覆盖 text byte length/dimension/blob length/finite values；accounting delta可安全确定的 single-entry damage可删除后miss/recompute，counter本身/entry accounting无法安全修复则fail closed；same-main/wrong marker/schema/version 配置冲突映射 `INVALID_ARGUMENT`，cache SQLite open/read/write/corruption 映射 `IO_ERROR`，OOM/full 映射 `RESOURCE_ERROR`，cancel映射 `INTERRUPTED + SQLITE_INTERRUPT`；不使用 `STORAGE_ERROR` 污染 Lithograph main-storage语义，不覆盖用户 DB、不影响 canonical state | `planned` |
| EX15-15 | compatibility regression | frozen applicable TCK/CY25 fixtures、Graph View、Full-text、Raw Vector、version operations、Merge/GC/explicit tx全部保持既有语义，无 unexplained skip | `planned` |
| EX15-16 | true-streaming performance/resource | 固定 large-read、large-write-return、transaction-subquery workloads 达成 Runtime memory invariant并记录 first-event/full consume/TEMP/peak RSS；mutating rows分别测快/慢consumer与early-close的 writer-hold/backpressure，确认没有额外full-result materialization/后台completion；Phase 11/13旧Native scale/stream/mixed/Semantic workload有效覆盖已迁到Core/SQL runner，无删除API导致验收降级 | `planned` |
| EX15-17 | minimum/current SQLite + repository gates | SQLite 3.45.0/current real-load、targeted tests、format/clippy/coverage、`cargo make quality`、`scripts/ci.sh`、diff checks 全部通过 | `planned` |
| EX15-18 | 六目标 release artifacts | Linux/macOS/Windows x64/arm64 hosted matrix 验证 SQL-only Lithograph artifact + Provider artifact 的 build/load/symbol/runtime | `planned` |
| EX15-19 | 文档与 review closure | Design/Development/compatibility、根 README、guide/reference/examples 与真实实现状态一致；旧 Native API reference/example 已删除或替换，rows/tx/error/Provider-cache 用法可运行，本 Phase review findings 全闭合，无旧 Native/read-only rows/format4 当前合同残留 | `planned` |

## 5. Review 重点

Phase-level review 必须至少检查：

- “分批返回”是否只是完整 `Vec` 后切片，尤其 program/composed/procedure read、mutation、transaction program；
- row 已流出后 failure/cancel 的 transaction rollback / committed-batch preservation 是否与 summary contract 一致；transaction-subquery failed/retried batch是否错误泄漏 provisional successful rows，单 batch outcome barrier是否用TEMP/spill而非完整内存集合；
- `xFilter/xNext/xClose`、LIMIT/early-stop、rescan、connection teardown 是否泄漏 read guard、SAVEPOINT、writer ownership 或 staged transaction；
- rows `xClose` 是否真正有可失败/可传播的 cleanup callback；不能因为 rusqlite 默认 wrapper固定 `SQLITE_OK` 就把 rollback error隐藏在 Drop；
- concurrent/nested cursor 是否让两个 side-effecting execution 共享或嵌套 transaction ownership；顺序 rescan 与重叠 execution 必须分开验证；
- scalar/rows 是否真的复用同一 execution semantics，而不是复制 write/transaction code；
- terminal-ready / finalize-success 是否真正共享：scalar envelope 构造失败与 rows summary encoding失败都必须发生在 ordinary side effect finalization 前，不能为 scalar late-failure rollback保留第二套 executor；
- active explicit transaction 是否通过正常 execution surface看 staged state，且没有第三套 tx-execute adapter；
- Native query API 是否从 source/header/tests/bench/release/symbol/documentation 全链路删除，同时 Provider SPI 完整保留；
- Lithograph Core 是否还残留 embedding result cache ownership、format4 metadata/table、cache procedure 或 sibling write；
- Provider cache 是否完全与 Lithograph main DB隔离，cache key 是否错误包含纯 operational config或遗漏会改变 embedding space 的 request semantics；
- read-only main、Provider external I/O、Graph View source isolation、historical Semantic definition、Raw Vector 与 HNSW correctness 是否回归；
- 大结果内存证据是否覆盖 Engine retained collection，而不是只证明 adapter 每次取少量 rows。

## 6. 完成条件

只有 EX15-01–19 全部有真实自动化/运行证据，Phase-level review 无剩余 correctness、transaction、resource、security、compatibility、release 或文档 finding，且最终 diff 无临时文件、无无关重构、无 secret/generated junk 时，Phase 15 才能从 `ready/in_progress` 标记为 `done`。

本计划当前只完成设计与开发计划冻结，**没有开始 Phase 15 代码实现，也没有提交、推送或发布**。
