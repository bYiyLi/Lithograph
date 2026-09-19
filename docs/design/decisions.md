# 架构决策与参考依据

[设计入口](../design.md) · [开发状态与验收](../development/README.md)

本文件记录已采用架构决策的依据、备选与取舍；其中的决定摘要不另建一套行为合同。具体行为按 [设计入口的职责表](../design.md#design-ownership) 归属各专题，变更决定时同步其拥有的合同，不能只修改本页。

<a id="architecture-decisions"></a>

## 关键架构决定与取舍

<a id="d1"></a>

### D1 标准 SQLite Extension，不 Fork SQLite

- 决定：使用公开 loadable-extension API。
- 依据：产品目标是“SQLite 一个插件”，并保留 SQLite 发行版、工具和单文件生态。
- 备选：修改 SQLite parser / fork SQLite。
- 取舍：stock SQL console 不能直接新增顶层 `MATCH` grammar，因此通过 SQL Bridge 或 Native ABI 输入 Cypher。

<a id="d2"></a>

### D2 Rust Core

- 决定：Rust 负责 Engine，C ABI 只作为 SQLite / binding boundary。
- 依据：需要 native extension、内存安全、复杂 parser/executor、并发与版本数据结构。
- 备选：纯 C/C++；直接延续 GraphQLite C architecture。
- 取舍：需要维护 C ABI wrapper，但避免把整个 Engine 的生命周期放在 C ownership 中。

<a id="d3"></a>

### D3 原生 Planner/Executor，不把“Cypher -> SQL 文本”作为核心架构

- 决定：Cypher 先进入 typed logical/physical plan，再通过 Storage primitives 执行；安全的 scalar/index 部分可下推 SQLite。
- 依据：完整 Cypher 25 path、subquery、mutation、Search 与 version snapshot 很难以 clause-pattern SQL transpiler 保持一致语义和可优化性。
- 备选：复制 GraphQLite 的 SQL transpilation / dispatcher 架构。
- 取舍：Engine 工作量更大，但兼容面和优化边界清晰。

<a id="d4"></a>

### D4 Version-aware Storage 从第一天存在

- 决定：所有 graph write 从 Root Commit 起写 immutable Layer，不先做 mutable graph 再改造成版本数据库。
- 依据：Identity、transaction、Schema、index、merge 和历史查询都会被版本模型改变。
- 备选：先实现当前状态，后续追加 audit log。
- 取舍：Storage foundation 更复杂，但避免后续重写数据模型。

<a id="d5"></a>

### D5 Immutable History + Mutable Branch Ref

- 决定：Commit/Layer 永不更新；Branch 是移动指针；Checkpoint/index 为 derived cache。
- 依据：直接获得 TerminusDB/Git 风格 history、branch、time-travel、diff/merge 基础。
- 备选：在 mutable rows 上增加 `valid_from/valid_to`。
- 取舍：需要 Snapshot Resolver 和 checkpoint，但普通 Cypher 不被时间字段污染。

<a id="d6"></a>

### D6 Schema 随数据版本化

- 决定：Commit 同时引用 Graph State 与完整 Schema hash。
- 依据：历史 Snapshot 必须在当时 Schema 下自洽，Branch merge 也必须合并 Schema 变化。
- 备选：Schema 始终使用数据库当前版本。
- 取舍：merge 增加 schema conflict 类型，但 time-travel 语义完整。

<a id="d7"></a>

### D7 Full-text 使用 FTS5，Vector 使用 HNSW derived index

- 决定：FTS5 处理全文，HNSW 处理 ANN vector search；两者都不是 canonical graph truth。
- 依据：分别利用 SQLite 成熟全文能力和适合 `SEARCH` 的 ANN 结构。
- 备选：统一自研 Search engine；只使用 brute-force vector scan。
- 取舍：需要维护两类 derived index lifecycle，但可以从任意 Snapshot 重建并保持版本正确性。

<a id="d8"></a>

### D8 Engine Compatibility 与 Adapter Capability 分离

- 决定：Native ABI 是完整 execution surface，SQL Bridge 是 stock SQLite adapter；两者共享同一 parser/planner/executor。
- 依据：SQLite SQL statement 内无法可靠取得 Cypher transaction-owning clause 所需的独立 commit boundary。
- 备选：fork SQLite 或通过隐式辅助连接绕过 host transaction。
- 取舍：极少数 transaction-boundary query 必须从 Native API 调用，但 Cypher Engine 本身仍按 Profile 完整实现。

<a id="d9"></a>

### D9 Graph View 是 Execution Context，不扩展 Cypher

- 决定：通过现有 SQL Bridge / Native ABI 的 `options.graphView` 选择 query-local Property Subgraph；selector 在一次 execution 内固定，visibility 随 Cypher 当前 clause graph state 计算；完整 versioned Schema / Constraint 不被 view 投影。Cypher text、AST 与 `CY25-2026.08` grammar 不增加 Lithograph-specific syntax。
- 依据：调用方需要让完整 MATCH/path/aggregation/Search/write 在同一个逻辑子图内执行；query rewrite 或结果后过滤无法保证 `count()`、path、top-k 和 mutation correctness。Cypher 25 的 `USE` 面向 graph reference / composite-database selection，不等价于同一 Property Graph 内的子图过滤。
- 备选：重新解释 `USE`、给 Cypher 增加自定义 clause、由调用方给每条 query 注入 `WHERE`、或为每个逻辑范围复制独立 database。
- 取舍：Planner/Executor/Storage/Search 都必须继承同一个 visibility contract，并在 read-write clause 与 transaction batch 边界重新依据实际 graph state 判断 membership；但不改变 storage format、frontend、Cypher compatibility Profile，也不建立持久化 named-view registry 或 per-view Schema。

<a id="d10"></a>

### D10 Explicit Transaction 提供多 execution 的单 Commit boundary

- 决定：以 `sqlite3*` connection 作为 transaction identity，提供 `begin -> execute* -> commit | abort` 的 explicit transaction，不增加 opaque transaction handle。Native C ABI 与 SQL `lithograph_tx_*` 只是同一 state machine 的两个 adapter；多个标准 Cypher current-graph execution 共享 transaction-local staged graph/Schema/Index state，成功 transaction 只创建一个最终 Layer/Commit 并只移动 Branch 一次。caller-owned SQLite transaction 继续只负责 durability atomicity，Cypher `IN TRANSACTIONS` 继续按 batch 独立 Commit，二者都不替代 explicit transaction。
- 依据：通用数据库调用方存在一个逻辑变化需要跨多个 query 读取中间结果、分配 identity、修改 graph + Schema/Constraint/Index，但版本历史只应出现最终一致 Snapshot 的需求。只有 Native binding 才能进入该 lifecycle 会迫使普通 SQLite driver 另行绑定 C API；SQL scalar 封装能复用现有 connection identity 与 Engine-owned transaction，无需新建 server/session 层。把每个 query 自动提交后再 squash 会产生真实 intermediate history；把 raw Structural Patch 变成普通 CRUD language 又会复制 Cypher mutation semantics。
- 备选：只允许“一条 Cypher query -> 一个 Commit”；只提供 Native binding；用 caller-owned SQLite transaction 包裹多个 Commit；要求调用方构造 Structural Patch；完成后自动 squash/rewrite history；建立独立 server/session transaction layer。
- 取舍：Engine 必须维护跨 execution 的 staged snapshot、transaction-level temporal clock、final net-delta canonicalization 与 fail-closed cleanup；active transaction 持有 SQLite single-writer ownership，长事务会阻塞其它写入。换取的是标准 Cypher 25 仍为正常 mutation language，同时获得明确的 version atomicity 和 one logical change -> one Commit 语义。

<a id="d11"></a>

### D11 Merge 使用持久 Merge Session 分离冲突解决与最终 Commit

- 决定：Three-way merge 先创建 durable、非历史的 Merge Session，pin `ours/theirs`，通过分页 conflict + incremental resolution 逐步得到 candidate；Session 以单调 revision 标识 resolution state，candidate 通过 `options.mergeSession={id,revision}` 只读检查。只有 `merge.finalize(session, expectedRevision)` 可以创建 Merge Commit/fast-forward 并移动 Branch，同时删除 Session。
- 依据：大型 merge 可能存在大量冲突，需要 AI/用户跨多个调用逐步解决；在整个交互期间持有 SQLite writer 会阻塞数据库，而一次性 `merge(source,resolutions)` 又要求 caller 把所有 conflict 放入一次上下文。上层系统还需要在最终 ref move 前检查自己的业务不变量。Pinned Commit + session revision + finalize target-head CAS 可以在不嵌入上层 callback、不保持长 writer transaction 的前提下保证“检查的 candidate == 最终准备提交的 candidate”。
- 备选：一次性 merge + 全量 resolutions；长生命周期 SQLite/Native transaction；merge prepare/finalize 但 candidate 只存内存；finalize-time application callback；让上层先 merge 再 revert invalid result。
- 取舍：format 2 增加 mutable Merge Session/resolution operational storage，GC 需要把 open Session 当 root，Version API 增加 session lifecycle 与 revision concurrency；换取 resumable conflict resolution、bounded conflict pagination、crash recovery、上层 pre-commit candidate validation，以及历史中始终只有最终一次 merge/fast-forward 结果。

<a id="d12"></a>

### D12 性能优化保持版本语义，先修物理访问与生命周期

- 决定：[Adjacency Keyset Cursor](storage.md#adjacency-keyset-cursor)让邻接 cursor 匹配现有 B-tree；[Query-scoped Resolved State](query-engine.md#query-scoped-resolved-state)让同一 query 共享 owned resolved state 与 read guard；[Large-scale Invariants](runtime.md#large-scale-invariants)用实际物理工作量和统一计时验收。
- 依据：当前 scale baseline 与代码复查发现错误的邻接 access plan、逐 batch resolution 及混合计时口径；证据强度和限制见性能研究记录。
- 备选：添加重复巨型索引、单纯增大 batch、关闭 Graph View/constraint 检查、直接重写 executor。
- 取舍：需要迁移内部 cursor、resource ownership 和关键失败测试，但不改变 Cypher 语义、Commit identity 或公开 ABI；长 read guard 仍可能造成 WAL 增长。

<a id="d13"></a>

### D13 持久 derived index 复用 anchor，不为每个 Snapshot 重建全域

- 决定：[Persistent Standard Index Base + Delta](schema-and-indexes.md#persistent-standard-index)采用 committed anchor generation + query-local delta；format 3 显式容纳其 exact schema；新 DDL 与单独 rebuild procedure 负责发布，只读路径不隐式写 `main`。
- 依据：TEMP Standard Index 在 connection 关闭后丢失，且 cache identity 按 Snapshot 分裂；持久化必须同时尊重 reserved-schema、只读 adapter、staged state 与版本类型语义。
- 备选：继续只用 TEMP、每个 Commit 复制完整 index、把 physical cache 纳入 canonical history、引入后台 server。
- 取舍：付出 derived disk space、generation cleanup 与一次显式2→3迁移；换取 reopen可复用和小delta不全量重建。缓存确实缺失时仍有 fallback/build 成本，显式全量 rebuild 仍可能持有长 writer，不能隐瞒。

<a id="d14"></a>

### D14 Raw Vector 与 Managed Semantic 并列，Provider 留在 SQLite Extension

- 决定：保留 Cypher 25 Raw Vector Property/Index/`SEARCH` 全部语义，另加一个非 grammar 的 `db.index.semantic.*` managed surface。Semantic source 是一个 String Property，派生 Vector 只存在于 derived materialization；具体 Embedding 由同 connection 上的普通 SQLite loadable extension 按 `EmbeddingProviderV1` 提供，Lithograph Core 不内置 vendor/model runtime。Persistent text->Vector cache 使用 format 4，并以 embedding-space + exact-text hash 去重；普通 Semantic query 在 cache enabled 且 database 可写时自动发布校验成功的 query/source Embedding，普通 graph mutation 永不调用 Provider。
- 依据：标准 Vector Index 明确索引真实 vector property，Cypher 25 `SEARCH` 接受 query Vector/List；把 String `ON (...)` 偷换为自动 Embedding 会创建 Lithograph dialect/语义差异。SQLite 已提供 loadable extension 与 connection client-data pointer，最低 3.45.0 可直接复用；当前需求只缺 Embedding contract，不需要第二套 plugin loader。外部 API 成本又要求跨 owner/index/history 复用 exact text 的 Vector，而该结果不应成为 canonical graph truth。
- 备选：废弃 Vector Property统一改自动 Embedding；给 Cypher 增加 `CREATE SEMANTIC INDEX` / `FOR TEXT`；让 KG OS 或应用维护 hidden vector Property；在 Lithograph Core 内置 OpenAI/HTTP/GPU；要求调用方先执行 rebuild/预热；做万能 Tokenizer/Embedding/Reranker plugin ABI。
- 取舍：产品存在 Raw Vector 与 Managed Semantic 两个明确入口，首次 Semantic query 可能产生 external-I/O cost和短 derived-cache write，历史 bit-identical rebuild 依赖部署固定 Provider runtime；换取的是标准 Cypher Vector 兼容不被破坏、任意正常 Cypher text mutation 不会留下 stale hidden vector、Provider 可替换/并存、重复文本能跨 connection/process 共享 cache，且 Provider latency 不占住 writer。需要 Vector 本身成为历史真源或完全可复现时继续使用 Raw Vector。

<a id="reference-baseline"></a>

## 参考基线

外部项目只提供 evidence 和实现参考，不覆盖 [设计文档集](../design.md) 的已确认合同：

- SQLite Loadable Extensions: <https://www.sqlite.org/loadext.html>
- SQLite Virtual Table Mechanism: <https://www.sqlite.org/vtab.html>
- Cypher Manual: <https://neo4j.com/docs/cypher-manual/current/>
- Neo4j Aura August 2026 Cypher additions: <https://feedback.neo4j.com/changelog/neo4j-aura-august-2026-release>
- GraphQLite: <https://github.com/colliery-io/graphqlite>
- TerminusDB Version Control: <https://terminusdb.org/docs/version-control-operations/>
- TerminusDB architecture explanation: <https://terminusdb.org/docs/terminusdb-explanation/>
- Git merge semantics: <https://git-scm.com/docs/git-merge>

外部研究证据的快照与采用边界另见 [参考基线研究](../research/reference-baseline.md)。

性能优化研究中的实现/性能观察、SQLite row-value pagination、INDEXED BY、read-transaction 与 statement-counter 依据及不采用边界见 [性能证据](../research/phase11-performance-evidence.md)。

Embedding Provider 的 SQLite client-data、loadable-extension、Cypher 25 Vector Index/`SEARCH` 边界与 Provider 适用限制见 [Embedding Provider 研究证据](../research/embedding-provider-contract.md)。
