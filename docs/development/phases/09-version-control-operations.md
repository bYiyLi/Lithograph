# Phase 09：Version Control Operations

**状态：`planned`**

## 1. 目标

在 Phase 02/05 已存在的 immutable history 上交付完整用户级 Git / TerminusDB 风格 Branch、History、Time-travel、Diff、Patch、Merge、Rebase、Squash、Reset、Revert 与 GC。

## 2. 依赖

- Phase 05 `done`；
- Phase 07–08 `done`，确保 Schema/Search definition 一起进入 merge/version semantics。

## 3. Design Inputs

- `docs/design.md` 第 10 节。

## 4. Features

### Feature 09.1 Branch lifecycle

实现：

```text
lithograph.branch.create
lithograph.branch.checkout
lithograph.branch.list
lithograph.branch.delete
```

- connection-local active branch；
- branch from branch/commit；
- no data copy；
- stable version descriptors。
- Branch name validation 与保留 `main` 规则；
- 删除 Branch 后其它 connection 的 stale checkout 行为。
- active Branch 不能由当前 connection 删除；checkout 在 caller-owned SQLite transaction 中返回 `TRANSACTION_BOUNDARY_REQUIRED`。

### Feature 09.2 Log and time-travel

- Commit DAG log；
- parent metadata；
- author/message/timestamp；
- per-query `options.at`；
- detached historical read-only enforcement。

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
- 只删除所有 Branch 都不可达的 canonical objects；
- reachable Commit hash/integrity 不变。

## 5. Acceptance

- [ ] cheap branch creation 不复制 checkpoint/graph rows；
- [ ] invalid Branch name 被拒绝，`main` 不能删除；
- [ ] 当前 active Branch 不能删除，transaction 内 checkout 被拒绝；
- [ ] 两 Branch 独立写入并可分别查询；
- [ ] arbitrary branch/commit diff deterministic；
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
- [ ] rebase 中间 Commit conflict 时整个 operation rollback，Branch/Commit history 不留下部分 rewrite；
- [ ] rebase old Merge Commit 时按 first-parent diff flatten second-parent topology，最终 Snapshot 正确；
- [ ] squash(since) 后 Snapshot 与原 HEAD 完全一致，只由一个新 Commit 连接 since；
- [ ] squash/rebase 产生的旧 unreachable history 在 explicit GC 前仍可按 Commit ID 查询；
- [ ] reset/revert/history/time-travel 通过；
- [ ] Merge Commit 未指定 mainline 时 revert 返回 `INVALID_ARGUMENT`，指定 1/2 时 inverse patch 正确；
- [ ] explicit GC 不删除 reachable history；
- [ ] reset 后 unreachable history 默认保留直到 explicit GC；
- [ ] restart 后 Branch/Commit DAG 完整。

## 6. Review

重点检查 merge/rebase 是否退化成 blind patch replay、rebase 是否产生 partial history、conflict unit 是否用整 Node 导致不必要冲突、schema/index 是否漏出 merge/rebase/squash、GC reachability 是否只看当前 main。

## 7. 完成条件

Version Control 用户能力全部 acceptance 通过；Phase 10 转 `ready`。
