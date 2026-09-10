# Phase 09：Versioned State Operations

**状态：`planned`**

## 1. 目标

在 Phase 02/05 已存在的 immutable history 上交付完整用户级版本化状态能力：Commit Data、Tag、explicit Commit、可分页 History/DAG traversal、Branch、Time-travel、Diff、Patch、Merge、Rebase、Squash、Reset、Revert 与 GC。Git / TerminusDB 只提供机制参考，不把 Commit 限定为软件版本或时间点。

## 2. 依赖

- Phase 05 `done`；
- Phase 07–08 `done`，确保 Schema/Search definition 一起进入 merge/version semantics。

## 3. Design Inputs

- `docs/design.md` 第 4.1、4.4、8、10、14 节。

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

### Feature 09.3 Structural Diff

- canonical patch operation set；
- ancestor fast path；
- arbitrary snapshot diff；
- typed before/after values；
- Schema/Index diff；
- deterministic operation ordering。
- patch `databaseId` provenance 与 same-database validation。

### Feature 09.4 Patch

- before-condition verification；
- all-or-nothing apply；
- one patch -> one Commit；
- inverse patch foundation for revert。

### Feature 09.5 Three-way Merge

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

### Feature 09.6 Conflict resolution

返回：

```text
conflict_id
slot
base
ours
theirs
```

再次 merge 接受 per-conflict resolution：`ours`、`theirs`、explicit value。任何 unresolved conflict 时不写 partial result。

### Feature 09.7 Rebase

- active Branch -> `onto` merge-base；
- first-parent commit sequence replay；
- 每个旧 Commit 通过 structural three-way application 重放；
- Merge Commit replay flatten second-parent topology；
- conflict shape 复用 merge，并增加 `sourceCommit`；
- 全 rebase 单 SQLite transaction staged，任一 conflict/error 全部 rollback；
- success 返回 old/new Commit mapping。

### Feature 09.8 Squash

- explicit ancestor `since`；
- structural `diff(since, HEAD)` -> one new Commit；
- new Commit parent=`since`；
- branch atomic move；
- old history immutable/unreachable until GC；
- resulting Snapshot 与 old HEAD byte/semantic equivalent。

### Feature 09.9 Reset / Revert

- reset 原子移动 active Branch；
- revert 应用 inverse patch 产生新 Commit；
- Merge Commit revert 强制指定 `mainline: 1|2`；
- reset/revert 后旧 Commit 仍 time-travel 可见。

### Feature 09.10 GC and cache maintenance

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

重点检查 Commit Data 是否错误进入 immutable hash/Snapshot/Diff/Merge、Tag 是否被普通 write 或 merge/rebase/squash 隐式移动、format `1 -> 2` migration 是否改写既有 Commit ID、cursor 是否因 Branch/Tag move 产生漂移，以及 merge/rebase 是否退化成 blind patch replay、rebase 是否产生 partial history、conflict unit 是否用整 Node 导致不必要冲突、schema/index 是否漏出 merge/rebase/squash、GC reachability 是否遗漏 Tag root。

## 7. 完成条件

Versioned State 用户能力全部 acceptance 通过；Phase 10 转 `ready`。
