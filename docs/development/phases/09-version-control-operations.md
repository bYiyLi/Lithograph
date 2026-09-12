# Phase 09：Versioned State Operations

**状态：`planned`**

## 1. 目标

在 Phase 02/05 已存在的 immutable history 与单-query auto-commit foundation 上，完成 transaction -> Commit contract 并交付完整用户级版本化状态能力：Native explicit transaction（多个标准 Cypher execution -> 一个 Commit）、Commit Data、Tag、explicit Commit、可分页 History/DAG traversal、Branch、Time-travel、Diff、Patch、Merge、Rebase、Squash、Reset、Revert 与 GC。Git / TerminusDB 只提供机制参考，不把 Commit 限定为软件版本或时间点。

## 2. 依赖

- Phase 05 `done`；
- Phase 07–08 `done`，确保 Schema/Search definition 一起进入 merge/version semantics。

## 3. Design Inputs

- `docs/design.md` 第 4.3–4.4、7.5–7.7、8–10、12–14 节。

## 4. Features

### Feature 09.1 Version sidecar storage、Branch 与 Tag lifecycle

在不重写既有 Commit/Layer/Schema 的前提下实现 storage format `1 -> 2` migration：

- 新增 Commit Data sidecar storage；
- 新增 Tag ref storage；
- 既有 Commit ID/hash/history 全部保持不变；
- fresh DB 在最终 release 直接创建 format `2`；
- migration 失败整体 rollback，旧 Engine 对 format `2` 按 `FORMAT_TOO_NEW` contract 拒绝需要新格式语义的访问。

实现：

```text
lithograph.branch.create
lithograph.branch.checkout
lithograph.branch.list
lithograph.branch.delete
lithograph.tag.create
lithograph.tag.list
lithograph.tag.move
lithograph.tag.delete
```

- connection-local active branch；
- branch from any version descriptor；
- no data copy；
- stable version descriptors。
- Branch name validation 与保留 `main` 规则；
- 删除 Branch 后其它 connection 的 stale checkout 行为。
- active Branch 不能由当前 connection 删除；checkout 在 caller-owned SQLite transaction 中返回 `TRANSACTION_BOUNDARY_REQUIRED`。
- Tag 与 Branch 使用相同 name validation、独立 namespace；Tag 不参与 checkout，不随 graph write 自动移动，只能显式 move；
- Tag 是 canonical-history GC root，Tag create/move/delete 自身不产生 Commit。

### Feature 09.2 Commit、Commit Data、Log and time-travel

- `lithograph.commit.get(version)`；
- `lithograph.commit.create([data])` 显式创建 single-parent empty-delta Commit，不引入 working tree/staging；
- `lithograph.commit.data.set/clear` 对已有 Commit 的 mutable JSON sidecar 做 set/replace/clear，不产生新 Commit、不改变 Commit ID/Snapshot；
- Commit Data 不进入 Commit hash、graph/Schema/Index Snapshot、Diff/Patch/Merge；需要 versioned/queryable 的业务事实仍保存为 graph/Schema 数据；
- Commit DAG log 使用 bounded page + opaque continuation cursor，cursor pin 初始 immutable Commit 与 traversal frontier；
- parent metadata；
- author/message/timestamp；
- per-query `options.at` 接受 commit/branch/tag descriptor，并在 query 开始时 pin immutable Commit；
- detached historical read-only enforcement。

`log` 默认不展开 Commit Data；需要业务 annotation 时单独 `commit.get`。Rebase rewritten Commit、Squash/Merge/Revert 新 Commit 默认不自动继承或合并旧 Commit Data，避免 Lithograph 猜测任意业务 JSON 的语义；Tag 也不被这些操作隐式移动。

`options.graphView` 可以与 graph-data historical read 的 `options.at` 组合，visibility 使用目标 Commit 的 Label membership；Version Procedure 自身不在 Graph View 中执行。

### Feature 09.3 Native Explicit Transaction

在 Native ABI 增加 additive explicit-transaction family，落实 `docs/design.md` 第 4.3、4.4、7.5–7.7、9.2、12、14.2 节：

- public lifecycle：`lithograph_v1_tx_begin/execute/commit/abort`，直接以 `sqlite3*` connection 作为 transaction identity；准确 C signature、input/output ownership 与 `lithograph_v1_free` contract 已由 Design 冻结，不在实现阶段重新设计；
- begin 只允许 SQLite autocommit mode，取得 Engine/SQLite single-writer ownership，pin target Branch head；可选 `expectedHead=commit/<id>` 在 writer ownership 下原子校验，mismatch 返回 `BRANCH_HEAD_MOVED`；
- 多个 `tx_execute` 复用现有 parser/planner/executor，并共享 transaction-local staged graph/Schema/Index state；后续 execution 能读取前序 staged writes，但 commit 前没有 public intermediate Commit；
- 每个 `tx_execute` 的 query-local Graph View 都针对当时 staged state 重新计算，允许不同 execution 使用不同 selector，不缓存 `tx_begin` 时的 element membership；
- `tx_execute` 保持普通 immediate Cypher Schema/Constraint/Graph View/error semantics，不引入 deferred-constraint mode；transaction-owned `branch/at/author/message` options、Version Procedure 与 transaction-owning subquery 被拒绝；
- `LOAD CSV` / external-I/O query 在 explicit transaction 内于 I/O 前返回 `TRANSACTION_BOUNDARY_REQUIRED` 并触发 fail-closed abort；
- 新建 Node/Relationship identity 可以在同一 transaction 后续 execution 中引用，abort 时不形成 durable element；
- 任一 execute/callback/cancel/validation failure fail-closed rollback 全部 staged state；commit failure 同样不留下 Layer/Commit/ref move；
- `tx_commit` 基于 base -> final staged state canonicalize 一个 transaction-level net delta；只要存在 mutating execution，就恰好创建一个 Commit，包括 final net-zero 的 empty-delta write intent；纯 read transaction 不创建 Commit；
- final Commit metadata 来自 begin 的 author/message，`committed_at` 在 finalize 取得；transaction-level counters 来自最终 canonical net delta；
- explicit transaction temporal clock 在 begin 固定，每个 `tx_execute` 有独立 statement clock；
- `tx_abort` 显式 rollback 并清除 connection-local transaction state；connection teardown 自动 rollback 未 terminal 的 explicit transaction，reopen 后不得残留 staged history；
- active explicit transaction 保持短生命周期；不得等待用户输入或无界外部交互，避免长期占用 SQLite single-writer ownership。

该 Feature 不改变 Phase 05 的普通 `lithograph_v1_execute` auto-commit，也不改变 Phase 08 `IN TRANSACTIONS` 的 per-batch Commit semantics；caller-owned SQLite transaction 仍只提供 durability atomicity。

### Feature 09.4 Structural Diff

- canonical patch operation set；
- ancestor fast path；
- arbitrary snapshot diff；
- typed before/after values；
- Schema/Index diff；
- deterministic operation ordering。
- patch `databaseId` provenance 与 same-database validation。

### Feature 09.5 Patch

- before-condition verification；
- all-or-nothing apply；
- one patch -> one Commit；
- inverse patch foundation for revert。

### Feature 09.6 Three-way Merge

- best-common-ancestor merge-base selection；
- criss-cross 多 merge-base 的 deterministic virtual-base 合成；
- logical-slot three-way comparison；
- identical/disjoint auto merge；
- property/label/entity/schema/index conflicts；
- delete-vs-modify；
- endpoint dependency conflict；
- post-merge constraint validation；
- two-parent Merge Commit。
- up-to-date / fast-forward / diverged merge 三种状态。

### Feature 09.7 Conflict resolution

返回：

```text
conflict_id
slot
base
ours
theirs
```

再次 merge 接受 per-conflict resolution：`ours`、`theirs`、explicit value。任何 unresolved conflict 时不写 partial result。

### Feature 09.8 Rebase

- active Branch -> `onto` merge-base；
- first-parent commit sequence replay；
- 每个旧 Commit 通过 structural three-way application 重放；
- Merge Commit replay flatten second-parent topology；
- conflict shape 复用 merge，并增加 `sourceCommit`；
- 全 rebase 单 SQLite transaction staged，任一 conflict/error 全部 rollback；
- success 返回 old/new Commit mapping。

### Feature 09.9 Squash

- explicit ancestor `since`；
- structural `diff(since, HEAD)` -> one new Commit；
- new Commit parent=`since`；
- branch atomic move；
- old history immutable/unreachable until GC；
- resulting Snapshot 与 old HEAD byte/semantic equivalent。

### Feature 09.10 Reset / Revert

- reset 原子移动 active Branch；
- revert 应用 inverse patch 产生新 Commit；
- Merge Commit revert 强制指定 `mainline: 1|2`；
- reset/revert 后旧 Commit 仍 time-travel 可见。

### Feature 09.11 GC and cache maintenance

- automatic derived cache cleanup；
- explicit canonical `lithograph.gc()`；
- 只删除所有 Branch 与 Tag 都不可达的 canonical objects；
- Commit 被 GC 时同时删除对应 Commit Data；Tag 自身不由 GC 自动删除；
- reachable Commit hash/integrity 不变。

## 5. Acceptance

- [ ] cheap branch creation 不复制 checkpoint/graph rows；
- [ ] format `1 -> 2` migration 只新增 sidecar/ref storage，migration 前后既有 Commit ID、Snapshot hash 与 history semantics 完全一致，失败原子 rollback；
- [ ] invalid Branch name 被拒绝，`main` 不能删除；
- [ ] 当前 active Branch 不能删除，transaction 内 checkout 被拒绝；
- [ ] 两 Branch 独立写入并可分别查询；
- [ ] Tag create/list/move/delete 正确；Branch head 前进时 Tag 不自动移动；`branch/foo` 与 `tag/foo` 可以并存；
- [ ] `tag/<name>` 可以作为 `options.at`、branch creation source 与其它接受通用 version descriptor 的 read/version operation 输入，并在 operation 开始时 pin 到当时的 immutable Commit；
- [ ] Tag 指向的 Commit 即使不再被任何 Branch 引用也不会被 GC；删除/移动最后一个保护该 history 的 Tag 后才允许后续 explicit GC 回收；
- [ ] `commit.data.set` 支持 object/array/string/number/boolean/null，set/replace/clear 不改变 Commit ID、parents、Snapshot 或 Branch/Tag ref；非法 JSON adapter input 被拒绝；
- [ ] Commit Data 修改不出现在 graph/schema/index diff/patch 中，merge 不尝试合并 Data；
- [ ] `commit.create` 在没有 graph/schema/index delta 时仍创建一个新的 single-parent Commit 并移动目标 Branch；可选初始 Data 与 Commit 创建原子完成；
- [ ] `commit.create` 不引入 working tree/staging，也不改变普通 graph mutation 自动 Commit 的 Phase 05 contract；
- [ ] Native `tx_begin` 在 autocommit mode 成功 pin target Branch；caller-owned SQLite transaction、nested explicit transaction 返回 `TRANSACTION_BOUNDARY_REQUIRED`；
- [ ] explicit transaction active 时，同一 `sqlite3*` 上普通 Native execute/validate、SQL Bridge scalar/rows、init/integrity 与其它 graph/version operation 返回 `TRANSACTION_BOUNDARY_REQUIRED`；纯信息 `lithograph_version()` 仍可调用。任何 surface 都不能产生独立 Commit、旁路 staged state 或对 staged/committed state 给出歧义结果；
- [ ] public header / exported symbols 与 Design 冻结的四个 connection-scoped C signatures 完全一致；`result_json` / `error_json` ownership 由 `lithograph_v1_free` 正确释放，NULL optional out-pointer 不改变行为；
- [ ] `tx_begin` 返回实际 resolved `baseCommit`；`tx_commit` 返回 `{commit,counters}`，纯 read transaction 返回原 `baseCommit` + zero counters，mutating transaction 返回唯一新 Commit；没有 active transaction 的 execute/commit/abort 返回 `INVALID_ARGUMENT` + `SQLITE_MISUSE`；
- [ ] `expectedHead` 与实际 Branch head 相等时 transaction 正常开始，不一致时在任何 staged write/identity/Commit/ref side effect 前返回 `BRANCH_HEAD_MOVED`；
- [ ] 两个以上独立 `tx_execute` 可以共享 staged state，后一个 execution 能读取前一个 execution 创建/修改的 Node、Relationship 与 Phase 07 Schema/Constraint/Index state；commit 前其它 connection 不可见 staged state；
- [ ] 相邻 `tx_execute` 使用不同 `graphView` selector 时，每次 visibility 都基于当前 staged state 重新计算：后一个 execution 可以按自己的 selector 看到前一个 execution 新增/改 Label 后变得可见的 element，且不能看到后序 writes；
- [ ] transaction 内 graph + Schema/Constraint/Index mutation 成功后只生成一个 Layer、一个 Commit、一次 Branch move；history 中不存在 per-execution intermediate Commit；
- [ ] transaction 中新建 element 的 identity 可以被后续 execution 引用；abort 或失败 transaction 不产生 durable element/Commit/ref move；
- [ ] 任一 `tx_execute` parse/semantic/type/schema/constraint/Graph View/callback/cancel failure 自动终止 transaction 并完整 rollback；失败后不能继续 execute/commit；cleanup fault 遵守 fail-closed `INTERNAL_ERROR` contract；
- [ ] explicit transaction 中 `LOAD CSV`、Version Procedure 与 transaction-owning subquery 在对应 external/ref/inner-transaction side effect 前返回 `TRANSACTION_BOUNDARY_REQUIRED`，并完整 rollback 之前 staged writes；
- [ ] explicit transaction 不把 constraint 自动延迟到 commit；statement 在当前 staged state 下非法时立即失败；final commit 仍执行完整 candidate validation；
- [ ] transaction 至少包含一个 mutating execution 时 `tx_commit` 恰好创建一个 Commit；多个 execution 最终 net delta 为零时仍创建一个 empty-delta Commit；纯 read transaction 不创建 Commit；
- [ ] final Commit 的 parent、author/message、committed_at、counters 与 transaction contract 一致；`tx_execute` statement summary 不伪造 staged Commit identity，最终 Commit 只由 `tx_commit` 返回；
- [ ] explicit transaction 的 `datetime.transaction()` 在多个 `tx_execute` 间稳定，`.statement()` 每次 execution 独立；
- [ ] explicit transaction 内 Version Procedure、checkout/GC 与 `CALL ... IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 返回 `TRANSACTION_BOUNDARY_REQUIRED`；per-execution `branch/at/author/message` 返回 `INVALID_ARGUMENT`；
- [ ] 显式 `tx_abort` 成功后 Branch/history/Snapshot 与 begin 前完全一致；abort cleanup fault 返回 `INTERNAL_ERROR` 并使 connection 不可继续依赖；未 commit/abort 就 teardown connection 时 SQLite rollback + connection-state cleanup 后 reopen 不存在 staged Commit/Layer/ref move；
- [ ] caller-owned SQLite `BEGIN ... COMMIT` 继续保留多个普通 mutating execution 的独立 Commit chain，证明 durability atomicity 与 Native explicit transaction version atomicity 没有被混淆；
- [ ] `log` 默认 bounded，opaque cursor 可以稳定遍历大型 DAG；首次解析后的 Branch/Tag 即使在分页过程中移动，后续 page 仍固定在原 start Commit；
- [ ] arbitrary commit/branch/tag descriptor diff deterministic；
- [ ] patch databaseId 不匹配当前 database 时原子拒绝；
- [ ] patch round-trip：`apply(diff(A,B), A) == B`；
- [ ] three-way disjoint changes auto merge；
- [ ] source 已包含于 target 返回 `up_to_date`；target 是 source ancestor 时只 fast-forward ref，不创建多余 Merge Commit；
- [ ] criss-cross history 使用 deterministic virtual base，并且 ambiguous base slot 不发生错误 auto-merge；
- [ ] same-value changes auto merge；
- [ ] property conflict 返回完整 base/ours/theirs 且不写结果；
- [ ] delete-vs-modify、relationship endpoint dependency、schema conflict、constraint conflict fixtures 通过；
- [ ] resolved merge 创建 parent1=target、parent2=source 的 Commit；
- [ ] merge snapshot 只用 first-parent + merge layer 可重建；
- [ ] rebase 对 multi-commit linear history 保留每个 Commit 的结构化 intent 和 author/message，并生成全新 IDs；
- [ ] rebase 不自动复制 Commit Data 到 rewritten IDs，返回的 old/new mapping 足以让上层显式处理；Tag 不自动移动；
- [ ] rebase 中间 Commit conflict 时整个 operation rollback，Branch/Commit history 不留下部分 rewrite；
- [ ] rebase old Merge Commit 时按 first-parent diff flatten second-parent topology，最终 Snapshot 正确；
- [ ] squash(since) 后 Snapshot 与原 HEAD 完全一致，只由一个新 Commit 连接 since；
- [ ] squash/merge/revert 新 Commit 默认没有 Commit Data，不聚合旧 annotation；
- [ ] squash/rebase 产生的旧 unreachable history 在 explicit GC 前仍可按 Commit ID 查询；
- [ ] reset/revert/history/time-travel 通过；
- [ ] historical graph-data query 的 `at + graphView` 保持目标 Commit visibility；任何 Version Procedure 与 `graphView` 同时提交都返回 `INVALID_ARGUMENT` 且无 ref/history side effect；
- [ ] `lithograph_rows` 对 checkout、GC 及其它 version/connection-state mutation 返回 `READ_ONLY_ADAPTER`，不产生任何 ref/cache/history side effect；
- [ ] Merge Commit 未指定 mainline 时 revert 返回 `INVALID_ARGUMENT`，指定 1/2 时 inverse patch 正确；
- [ ] explicit GC 不删除 Branch/Tag reachable history，并随真正被删除的 Commit 清理其 Commit Data；
- [ ] reset 后 unreachable history 默认保留直到 explicit GC；
- [ ] restart 后 Commit DAG、Branch/Tag refs 与 Commit Data 完整。

## 6. Review

重点检查 explicit transaction 是否错误生成 per-execution Commit、是否允许 staged state 被其它 connection 观察、不同 execution 的 Graph View 是否错误复用旧 membership、execute/abort/connection-teardown failure 是否残留 identity/Layer/ref、`expectedHead` 是否存在 check-then-lock race、`LOAD CSV` 是否在 writer ownership 内做外部 I/O、长事务是否无界持有 writer、statement/transaction clock 是否混淆；同时检查 Commit Data 是否错误进入 immutable hash/Snapshot/Diff/Merge、Tag 是否被普通 write 或 merge/rebase/squash 隐式移动、format `1 -> 2` migration 是否改写既有 Commit ID、cursor 是否因 Branch/Tag move 产生漂移，以及 merge/rebase 是否退化成 blind patch replay、rebase 是否产生 partial history、conflict unit 是否用整 Node 导致不必要冲突、schema/index 是否漏出 merge/rebase/squash、GC reachability 是否遗漏 Tag root。

## 7. 完成条件

Versioned State 用户能力全部 acceptance 通过；Phase 10 转 `ready`。
