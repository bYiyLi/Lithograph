# 版本控制

[设计入口](../design.md) · [开发状态与验收](../development/README.md)

本文件拥有 Commit/Branch/Tag/Commit Data、Version Procedure、Diff/Patch、Merge Session、Rebase/Squash/Reset/Revert/GC 的行为合同。持久布局与事务原子性见 [Storage](storage.md)，query/transaction options 和结果编码见 [接口合同](interfaces.md)。


<a id="commit-branch-tag-data"></a>

## Root、Commit、Branch、Tag 与 Commit Data

`lithograph_init()` 创建 Root Commit，`main` 指向 Root。

Commit 是 immutable database state：其 parents、graph Layer、Schema reference、author/message/committedAt 与 Commit ID 创建后不允许原地修改。普通 graph / Schema / Index mutating query 按[Transaction 与 Concurrency Model](storage.md#transactions-and-concurrency)自动创建 Commit；此外 Version Procedure 可以显式创建一个 single-parent empty-delta Commit，用于调用方主动建立新的状态节点，而不引入 Git working tree 或 staging model。

Branch 与 Tag 都是可变的命名引用，但语义不同：

```text
main -> C3
feature -> F2
tag/release-baseline -> C2
```

创建 Branch 只新增一个 ref，初始指向指定 version descriptor 解析出的 Commit Snapshot，不复制图数据。

每个 connection 有一个 active Branch，默认 `main`。`checkout` 只改变 connection-local execution context，不改 graph data。

Branch name 使用 case-sensitive UTF-8 bytes，长度为 1..255 bytes；禁止 NUL、ASCII control characters、开头或结尾 `/`、空 path segment，以及 segment `.` / `..`。`/` 可以用于层级命名。`main` 是 init 创建的保留 Branch，不能删除或重命名。删除其它 Branch 不删除 Commit；如果另一 connection 仍 checkout 已删除 Branch，它的下一次依赖 active Branch 的 query 返回 `BRANCH_NOT_FOUND`，直到 checkout 一个存在的 Branch。

Tag 使用与 Branch name 相同的字节与 path validation，但位于独立 namespace，因此 `branch/foo` 与 `tag/foo` 可以同时存在。Tag 不参与 connection checkout，也不会因 graph write 自动移动；只有显式 `tag.move` 才能改变其 target。Tag create / move / delete 都不创建 Commit。Tag 是 GC reachability root：只要任意 Tag 仍指向某 Commit，该 Commit 及其 canonical ancestors 不能因 Branch 不可达而被 GC。

每个 Commit 最多有一份可选 **Commit Data**。它是调用方提供的 mutable JSON annotation，可以是 object、array、string、number、boolean 或 `null`；Lithograph 不预定义 `title`、`time`、`stage` 等业务字段。Commit Data 可以随时通过显式 sidecar mutation set / replace / clear，且这些操作不创建 Commit、不移动 Branch/Tag，也不改变目标 Snapshot。需要 immutable/versioned 的业务事实必须放入正常 graph / Schema state。

<a id="version-descriptor"></a>

## Version Descriptor

所有 version-aware API 使用无歧义 descriptor：

```text
branch/<name>
commit/<64-hex-id>
tag/<name>
```

`branch/<name>` 与 `tag/<name>` 在每次 operation 开始时解析为当时指向的 Commit；operation 后续使用 pinned Commit，不受并发 ref move 影响。历史 `commit/<id>` Snapshot 永远只读。要从任意历史 Commit / Tag 状态继续写入，先从解析出的 Commit 创建 Branch。

<a id="version-procedures"></a>

## Version Procedures

版本管理通过 Cypher procedure 提供，不增加自定义 grammar：

```text
CALL lithograph.branch.create(name [, from])
CALL lithograph.branch.checkout(name)
CALL lithograph.branch.list()
CALL lithograph.branch.delete(name)
CALL lithograph.commit.get(version)
CALL lithograph.commit.create([data])
CALL lithograph.commit.data.set(version, data)
CALL lithograph.commit.data.clear(version)
CALL lithograph.tag.create(name, target)
CALL lithograph.tag.list()
CALL lithograph.tag.move(name, target)
CALL lithograph.tag.delete(name)
CALL lithograph.log([version [, limit [, cursor]]])
CALL lithograph.diff(before, after)
CALL lithograph.patch.apply(patch)
CALL lithograph.merge.start(source [, expectedHead])
CALL lithograph.merge.get(session)
CALL lithograph.merge.list([limit [, cursor]])
CALL lithograph.merge.conflicts(session [, limit [, cursor]])
CALL lithograph.merge.resolve(session, expectedRevision, resolutions)
CALL lithograph.merge.finalize(session, expectedRevision)
CALL lithograph.merge.abort(session, expectedRevision)
CALL lithograph.rebase(onto [, options])
CALL lithograph.squash(since)
CALL lithograph.reset(target)
CALL lithograph.revert(commit [, options])
CALL lithograph.gc()
```

Procedure result 是普通 Cypher rows，因此可以与 `YIELD` / `RETURN` 组合。

参数规则：

- `branch.create(name, from)`：`from` 省略时使用 active Branch head；否则接受 version descriptor；
- `branch.checkout(name)`：只接受 Branch name；调用时 SQLite connection 必须处于 autocommit mode，否则返回 `TRANSACTION_BOUNDARY_REQUIRED`，避免 connection-local checkout 与 caller rollback 脱节；
- `branch.delete(name)`：不能删除 `main`，也不能删除当前 connection 的 active Branch；
- `commit.get(version)`：接受任意 version descriptor，返回解析后的 immutable Commit metadata 与当前 Commit Data；读取本身不修改任何 ref/data；
- `commit.create(data)`：在 query-level `branch` 或 active Branch 上创建一个 parent=当前 head、empty Layer、相同 Schema 的新 Commit；`author/message` 使用 execution-level query options。可选 `data` 与新 Commit Data 在同一 SQLite transaction 内原子写入；不使用 working tree / staging；
- `commit.data.set(version, data)`：解析 target Commit 后 set / replace 其 Commit Data；显式 JSON `null` 是合法 value；不创建 Commit；
- `commit.data.clear(version)`：删除目标 Commit 的 Data sidecar；目标 Commit 本身保持不变；
- `tag.create(name, target)`：创建新 Tag 并指向 target version descriptor 当前解析出的 Commit；同名 Tag 已存在时返回 `INVALID_ARGUMENT`；
- `tag.move(name, target)`：显式移动已存在 Tag；不存在时返回 `TAG_NOT_FOUND`；
- `tag.delete(name)`：删除 Tag ref，不删除其目标 Commit；
- `tag.list()`：枚举全部 Tag，按 name binary ascending 返回；
- `log(version, limit, cursor)`：`version` 省略时使用 active Branch；第一次调用把 version 解析并 pin 成 immutable start Commit。`limit` 省略时默认 `100`，必须为正整数；`cursor` 是 opaque continuation，包含 start Commit 与 DAG traversal frontier，只能用于同一 pinned traversal，Branch / Tag 后续移动不改变已开始的分页；
- `diff(before, after)`：两个参数都必须是 version descriptor；
- `patch.apply(patch)`：Commit author/message 使用 execution-level query options；
- `merge.start(source, expectedHead)`：`source` 接受任意 version descriptor；target 使用 query-level `branch` 或 active Branch。可选 `expectedHead` 只接受 resolved `commit/<id>`，用于要求 Session 的 pinned `ours` 必须精确等于调用方已验证的 target head；省略时使用调用开始实际 pin 到的 target head。成功创建 durable Merge Session 并计算初始 candidate/conflict 状态；**不创建 Commit、不移动 Branch，包括 fast-forward 情况**；
- `merge.get(session)`：在一个 SQLite read snapshot 内读取 Session 当前 pinned inputs/revision，并基于同一 revision 的 resolution set 计算 status 与 unresolved conflict count，不能返回 revision/status 来自不同瞬间的混合结果；
- `merge.list(limit, cursor)`：分页枚举**调用期间当前存在**的 open Merge Session；`limit` 默认 `100`，按 session id binary ascending，cursor opaque。它只读取持久化 session metadata，不为了 listing 重算 candidate/conflict；需要 `status/unresolved` 时对具体 Session 调用 `merge.get`。list 是 operational inventory，不 pin 一个跨多页不可变的 Session 集合；并发 start/finalize/abort 可以改变后续 page，调用方需要最新完整 inventory 时从首屏重新枚举；
- `merge.conflicts(session, limit, cursor)`：分页返回该 Session 当前 revision 的 conflict；cursor 绑定 session + revision，resolution 改变后旧 cursor 返回 `MERGE_SESSION_CHANGED`；
- `merge.resolve(session, expectedRevision, resolutions)`：`expectedRevision` 必须等于当前 revision；同一调用原子 set/replace 一组 conflict resolution，unknown conflictId / duplicate conflictId / 非法 choice/value 返回 `INVALID_ARGUMENT`。在**当前 revision** 上，如果整批 resolution 与已保存值完全相同则是 no-op、revision 不变；只要 resolution set 有有效变化，revision 就只递增一次。使用 stale `expectedRevision` 的重试仍返回 `MERGE_SESSION_CHANGED`，即使 payload 恰好与当前值相同，v1 不另外维护 request-id 幂等日志；
- `merge.finalize(session, expectedRevision)`：只在 expected revision 精确匹配且 unresolved conflict 为 `0` 时运行；Commit author/message 使用 finalize execution-level query options。取得 target Branch writer ownership 后必须再次确认 head 仍等于 Session 的 pinned `ours`；不一致返回 `BRANCH_HEAD_MOVED` 且 Session 保留；
- `merge.abort(session, expectedRevision)`：进入短 writer boundary 后要求 Session revision 仍等于 `expectedRevision`，匹配时原子删除 Session/resolution，不创建 Commit、不移动 Branch；stale caller 返回 `MERGE_SESSION_CHANGED`，避免用旧状态误删别人刚更新的 conflict resolution；
- `rebase(onto, options)`：把 active Branch 在 merge-base 之后的 first-parent commit sequence 逐个 replay 到 `onto`；procedure options 只支持 `resolutions`；replayed Commit 默认保留各自旧 author/message，不使用 execution-level author/message 覆盖历史 intent；
- `squash(since)`：`since` 必须是 active Branch head 的 ancestor descriptor，把 `since..HEAD` 的最终结构化变化压成一个新 Commit；新 Commit author/message 使用 execution-level query options；
- `reset(target)`：把 active Branch ref 移到 target descriptor 当前解析出的 Commit；
- `revert(commit, options)`：commit 必须是 `commit/<id>`；procedure options 只支持 `mainline`；Commit author/message 使用 execution-level query options；
- `gc()`：没有参数。

`merge.resolve(..., resolutions)` 与 `rebase.options.resolutions` 共用同一 resolution item shape：

```text
[
  {conflictId: "...", choice: "ours"},
  {conflictId: "...", choice: "theirs"},
  {conflictId: "...", choice: "value", value: <typed Cypher value>}
]
```

未知 conflictId、重复 conflictId、非法 choice 或与 slot 类型不匹配的 explicit value 都返回 `INVALID_ARGUMENT`，且不写任何 ref/Commit。

公开 procedure 结果合同：

| Procedure | 结果 |
| --- | --- |
| `branch.create` | `name, commit` 一行 |
| `branch.checkout` | `name, commit` 一行 |
| `branch.list` | 每个 Branch 一行 `name, commit, active`，按 name binary ascending |
| `branch.delete` | `name, previousCommit` 一行 |
| `commit.get` | `commit, parents, author, message, committedAt, hasData, data` 一行 |
| `commit.create` | `commit` 一行 |
| `commit.data.set` | `commit, data` 一行 |
| `commit.data.clear` | `commit` 一行 |
| `tag.create` | `name, commit` 一行 |
| `tag.list` | 每个 Tag 一行 `name, commit` |
| `tag.move` | `name, previousCommit, commit` 一行 |
| `tag.delete` | `name, previousCommit` 一行 |
| `log` | 每个 Commit 一行 `commit, parents, author, message, committedAt, cursor`；`cursor` 可从该 row 之后继续，遍历结束时为 `null` |
| `diff` | 一行 `patch` map |
| `patch.apply` | 一行 `commit` |
| `merge.start` | 一行 `session, targetBranch, ours, theirs, revision, status, unresolved` |
| `merge.get` | 一行 `session, targetBranch, ours, theirs, revision, status, unresolved` |
| `merge.list` | 每个 open Session 一行 `session, targetBranch, ours, theirs, revision, createdAt, cursor` |
| `merge.conflicts` | 每个 conflict 一行 `session, revision, conflictId, slot, base, ours, theirs, resolution, cursor` |
| `merge.resolve` | 一行 `session, revision, status, unresolved` |
| `merge.finalize` | 一行 `status, commit`；`up_to_date | fast_forward | merged` |
| `merge.abort` | 一行 `session` |
| `rebase` | 一行 `status, commit, rewritten, conflicts`；冲突时 `commit = null` |
| `squash` | 一行 `from, previousHead, commit` |
| `reset` | 一行 `from, to` |
| `revert` | 一行 `commit` |
| `gc` | 一行 deleted Commit/Layer/cache counters |

`log` 遍历 pinned start Commit 可达的 DAG，按 reverse-topological order 返回；同一 topology level 先按 `committed_at` descending，再按 Commit ID ascending，保证结果确定。Continuation cursor 只编码 traversal state，不是新的 version identity，也不能被调用方解析或修改。`log` 默认不展开 Commit Data；需要某个状态的业务 annotation 时使用 `commit.get`，避免大型 history 把任意 JSON 一次性塞入结果。

<a id="diff-and-patch"></a>

## Diff 与 Patch

Diff 是结构化 graph patch，不是 SQL row diff 或文本 diff。Patch operation 的封闭集合：

```text
AddNode
DeleteNode
AddLabel
RemoveLabel
AddRelationship
DeleteRelationship
SetProperty
RemoveProperty
SetSchema
CreateIndex
DropIndex
SetIndex
```

Patch 是一个 map：

```text
{
  format: 1,
  databaseId: "<uuid>",
  from: "commit/...",
  to: "commit/...",
  operations: [ ... ]
}
```

每个 operation 都包含 `op`、stable logical slot，以及该 operation 所需的 typed `before` / `after`。一个 canonical Patch 对同一 logical slot **最多包含一个 operation**；duplicate slot 属于非法 Patch 并在应用任何 operation 前返回 `INVALID_ARGUMENT`。这样全部 `before` conditions 都解释为对输入 Snapshot 的并列前置条件，而不是依赖 Patch 内 operation 顺序形成第二套 imperative mutation language。Index 从一个定义替换为同名的另一个定义时使用单一 `SetIndex`，不能编码成同一 `index/<name>` slot 上的 `DropIndex` + `CreateIndex` 顺序对。`patch.apply` 只接受 `databaseId` 与当前 database 相同的 patch；跨 database patch/import 不属于当前合同。`from/to` 用于 provenance，不要求 active Branch 当前 head 等于 `from`，真正 applicability 由所有 `before` conditions 决定。

每个 operation 使用稳定 `elementId`、label/type/property name 和 before/after value 表示。`DETACH DELETE` 产生显式 Relationship deletions 与 Node deletion，因此 patch 可独立验证和重放。

`lithograph.diff(A, B)` 对任意 version descriptor 产生从 A 变为 B 的 canonical ordered patch。若 A 是 B 的 ancestor，可以直接 compose Layers；否则使用 Snapshot / merge-base 优化，但输出语义相同。

`lithograph.patch.apply` 在 active Branch 上验证 patch 的 `before` condition，全部成立后以一个新 Commit 原子应用；任一 condition 不成立则整体失败。

Diff / Patch 只描述 canonical graph / Schema / Index Snapshot change。Commit Data、Tag、Branch ref 不进入 patch。显式 empty-delta Commit 与其 parent 的 `diff` 可以合法返回空 `operations`；这不表示两个 Commit identity 相同。

<a id="merge-session"></a>

## Three-way Merge 与 Merge Session

Merge 使用 Git 风格 three-way model：

```text
merge-base
   /   \
 ours  theirs
   \   /
  merge commit
```

目标 Branch 当前 head 是 `ours` / first parent；source 是 `theirs` / second parent。Merge result layer 相对 `ours` 保存。

Merge 使用**可恢复的 Merge Session**，把“计算/解决冲突/检查 candidate”与“最终 Commit + Branch move”分开。`merge.start` 同时解析并 pin `ours` 与 `source` Commit，并持久化 Session；Source Branch 后续移动不改变本次 merge 已 pin 的 `theirs`。Session 创建后不长期持有 SQLite writer ownership，用户或上层系统可以跨多个调用、connection reopen 甚至 process restart 分页查看和逐步解决大量冲突。

`merge.start` 的 merge-base / candidate/conflict 计算可以在 read path 上完成，但 Session row 真正建立前 `ours/theirs` 还不是 GC root。开始持久化时必须进入一个短 SQLite write transaction，在该边界内重新确认两个 pinned Commit 仍存在，再原子写入 Session；如果其间某个 Commit 已被 GC，返回 `VERSION_NOT_FOUND` 且不创建 partial Session。调用方提供 `expectedHead` 时，还必须在**同一个 writer boundary** 内确认 target Branch 当前 head 仍精确等于 `expectedHead`，否则返回 `BRANCH_HEAD_MOVED` 且不创建 Session；成功 Session 的 `ours=expectedHead`。省略 `expectedHead` 时，target Branch 在 read-path 计算后移动不要求 start 失败，因为 Session 明确保存计算时 pin 的原 `ours`，最终是否还能提交由 finalize 的 target-head CAS 决定。

Merge Session 是 operational workspace，不是 Commit、Branch、Tag 或 Version Descriptor。Session 的 authoritative state 只有 pinned `ours/theirs`、resolution set 与单调 `revision`；candidate/conflict 都从这些 immutable inputs 确定性计算。多个 Session 可以并存，也可以针对同一 target Branch 并行准备；只有 finalize 时的 target-head CAS 决定谁能够提交。

`merge.start` 计算出的初始 status 使用：

- `theirs == ours` 或 `theirs` 是 `ours` ancestor -> `status = up_to_date`；
- `ours` 是 `theirs` ancestor -> `status = fast_forward`；
- 其它 divergence 且存在 unresolved conflict -> `status = conflicted`；
- 其它 divergence 且 conflict 已全部解决/不存在 -> `status = ready`。

这些 status 在 `merge.start/get/resolve` 阶段都**不会**修改 canonical history。即使是 fast-forward，也要等 `merge.finalize` 才能移动 target Branch，使调用方可以在 ref move 前对 pinned candidate 做额外只读验证。

用于 candidate inspection 的逻辑 Snapshot 固定为：`up_to_date` 读取 pinned `ours`；`fast_forward` 读取 pinned `theirs`；`ready` 读取基于 pinned `ours/theirs` + 当前 resolution set 计算出的 merged candidate。`conflicted` 没有完整 candidate，不能进入 candidate query context。

Merge-base 使用 Git-style “best common ancestors”：先找所有同时可达且不是另一 common ancestor 祖先的 best bases。只有一个时直接作为 base。存在多个 criss-cross best bases 时，按 Commit ID ascending 递归合成一个 **virtual base**；virtual-base merge 使用同一 logical-slot three-way rule，但冲突 slot 记录为内部 `unknown` sentinel。最终 merge 中，base 为 `unknown` 且 `ours != theirs` 时必须报告 conflict；`ours == theirs` 时可以自动接受该相同值。Virtual base 不写入 Commit DAG。

Conflict 的最小 logical slot：

- Node existence：`node/<id>`；
- Label membership：`node/<id>/label/<label>`；
- Relationship existence：`relationship/<id>`；
- Property：`node|relationship/<id>/property/<key>`；
- Schema / Constraint / Index object：对应 canonical schema identifier。

三方规则：

- 只有一侧相对 base 改变 -> 接受该侧；
- 两侧产生相同最终值 -> 自动合并；
- 两侧对同一 slot 产生不同最终值 -> conflict；
- delete-vs-modify -> conflict；
- 一侧删除 Node、另一侧新增或修改仍依赖该 Node 的 Relationship -> conflict；
- graph slot 虽无直接冲突，但 merged state 违反最终 Graph Type / Constraint -> constraint conflict。

存在 conflict 时 Session **不写任何 Commit/ref 部分结果**。`merge.conflicts` 以 bounded page 返回当前 revision 的 conflict inventory，包括已经有 resolution 与仍 unresolved 的项；结果按 canonical `slot` UTF-8 bytes、再按 `conflictId` bytes 升序，cursor 绑定 session + revision。大量 conflict 不要求一次 materialize 到 result 或 caller memory。Conflict ID 仍由 pinned merge inputs + logical slot/value 确定性生成。

`merge.resolve` 可以多次调用，每次只提交一批 `ours` / `theirs` / explicit replacement value，并允许后续用同一个 conflictId 替换之前选择。为避免在大量 conflict 校验时长期占用 writer，Engine 先在 `expectedRevision=R` 的 SQLite read snapshot 上 pin Session/resolution set，应用本次 proposed resolution 到 operation-local state，并在这个只读阶段完成 conflictId / explicit-value type validation、candidate/conflict 重算以及**本次成功 operation 应返回的 resulting revision/status/unresolved**：整批与已有 resolution 完全相同则 resulting revision 仍为 `R`，存在有效变化则为 `R+1`。随后进入短 write transaction / writer boundary，**重新**读取 Session 并要求 revision 仍为 `R`，否则返回 `MERGE_SESSION_CHANGED`。只有 CAS 成功才原子 set/replace resolution 并把 revision 至多增加一次；返回的 status/unresolved 使用前述 deterministic proposed state，因此不需要在 writer lock 内重新扫描大型 conflict set。一次 resolution 可能使旧 conflict 消失，也可能暴露新的 constraint conflict。任何 unresolved conflict 时都不能 finalize。

如果某个已经保存 resolution 的 conflict 因其它 resolution 改变而暂时不再出现在当前 conflict inventory，该 resolution 作为 **dormant resolution** 保留：它当前不作用于 candidate、不计入 unresolved，也不出现在 `merge.conflicts` 当前页；若同一 pinned merge inputs 下完全相同的 deterministic conflictId 后续重新出现，则自动重新应用原 resolution。这样逐步解决不会因为 conflict dependency 的出现/消失丢失已完成工作，同时 conflictId 绑定的 slot/value hash 又保证旧 resolution 不会被套到另一个不同 conflict。

当 unresolved conflict 为 `0` 时，调用方可以通过 `options.mergeSession={id,revision}` 使用普通只读 Cypher / Search / Schema introspection 检查**这一版精确 candidate**。每次 candidate execution 在自己的 SQLite read snapshot 内同时读取 Session、校验 requested revision 并 pin 对应 resolution set；如果 operation 开始时当前 revision 已不同则返回 `MERGE_SESSION_CHANGED`，operation 开始后的并发 resolution 不会改变该 execution 已 pin 的 candidate。这提供通用的上层 candidate-validation boundary，而 Lithograph 不需要知道调用方的业务规则。一个调用方可以在 revision `R` 上执行多次检查，随后调用 `merge.finalize(session,R)`；如果检查期间任何 resolution 被修改，revision 会变化，旧 finalize 以 `MERGE_SESSION_CHANGED` 失败。因此“被验证的 candidate”和“准备提交的 candidate”不会静默漂移。

`merge.finalize` 是唯一会影响 canonical history 的 Session operation。它分成 preparation 与短 writer finalize 两段，不能把大型 merge 重算放进 writer lock：

1. 在 `expectedRevision=R` 的 SQLite read snapshot 上 pin Session/resolution set，确认 unresolved=`0`，确定性构造 exact candidate、canonical net delta / schema result，并完成可在只读阶段证明的完整 graph/Schema/Constraint/canonical-integrity validation；这些 prepared result 只是本次 operation-local state，不创建新的持久 workspace；
2. 开启 Engine-owned SQLite write transaction / 取得 writer ownership，使并发 `merge.resolve/abort/finalize`、GC 与 Branch write 被 SQLite 串行化；
3. 在该 writer boundary **内部**重新读取 Session，验证其仍存在、revision 仍为 `R` 且 unresolved=`0`；随后读取 target Branch：Branch 已不存在返回 `BRANCH_NOT_FOUND`，存在但 head 不等于 pinned `ours` 返回 `BRANCH_HEAD_MOVED`。这些失败都保留 Session；不能先 check revision/head 再取得 writer；
4. 如果上述 CAS 成立，prepared candidate 仍由同一 immutable `ours/theirs` + revision `R` resolution set 唯一决定，不需要在 writer 内再次执行完整 merge/conflict scan；只执行依赖实际写入边界的 final storage/integrity recheck，并安装必要的 canonical/derived write result；
5. `up_to_date`：不写 Commit、不移动 Branch；`fast_forward`：只把 Branch 移到 pinned `theirs`；`ready`：创建 parent1=`ours`、parent2=`theirs` 的 Merge Commit，Layer 相对 `ours` 保存；
6. Branch move / Commit write（若有）与 Session/resolution 删除在**同一个 SQLite transaction**完成。

因此用户可以解决 1 个、100 个或 100,000 个冲突而不产生 intermediate Commit；Branch 也不会因为逐步 resolution 移动。只有最终 finalize 成功时历史才出现一次结果。

`merge.finalize` 成功时 `commit` 总是非空 resolved identity：`up_to_date` 返回 pinned `ours`，`fast_forward` 返回 pinned `theirs`，`merged` 返回新建 Merge Commit。这样调用方不需要根据 status 再执行一次 Branch read 才知道最终 Snapshot。

Merge Session operation 的稳定失败优先级固定为：Session 不存在先返回 `MERGE_SESSION_NOT_FOUND`；需要 expected revision 的 operation 在 Session 存在但 revision 不匹配时返回 `MERGE_SESSION_CHANGED`；candidate inspection / finalize 在当前 revision 仍有 unresolved conflict 时返回 `MERGE_CONFLICT`；finalize 再检查 target Branch 的 `BRANCH_NOT_FOUND` / `BRANCH_HEAD_MOVED`；通过这些 concurrency/boundary checks 后才报告 candidate 的 Schema/Constraint/storage validation error。这样 stale caller 不会因为后续 candidate 内容变化得到误导性的业务错误。

Merge 不解释或合并 Commit Data，也不移动 Tag。Diverged finalize 新建的 Merge Commit 默认没有 Commit Data；调用方需要时在 finalize 成功后显式 set。Fast-forward finalize 只移动目标 Branch 到已有 source Commit，因此该 Commit 原有 Data 保持可见。

Conflict ID 是对 `merge-base identity + ours commit + theirs commit + slot + base/ours/theirs canonical values` 的 BLAKE3 hash；同一 pinned merge inputs 得到相同 conflict ID。Session resolution 可以长期保存，但不会绕过 target Branch concurrency：target head 在 Session 生命周期内发生变化时，已有 conflict resolution 仍可查看，`merge.finalize` 必须返回 `BRANCH_HEAD_MOVED`，调用方重新 start 新 Session 后不能把旧 resolution 静默套到新 merge inputs。

Session 自身是持久 operational state：connection/process crash 后可以通过 `merge.get/list` 恢复进度；`merge.abort(session, expectedRevision)` 以 revision CAS 显式放弃。Lithograph v1 不自动按 TTL 删除 open Session，避免在长时间人工/AI conflict resolution 中丢失工作；调用方负责 finalize/abort，`merge.list` 提供可发现的清理入口。Open Session 同时保护 pinned history 免受 GC。

<a id="rebase"></a>

## Rebase

`rebase(onto)` 使用 Git-style commit replay，但以结构化 graph patch 为单位：

1. pin active Branch head 为 `oldHead`，pin `onto` Commit；
2. 沿 `oldHead` 的 **first-parent chain** 向历史方向查找，选择离 `oldHead` 最近且同时是 `onto` ancestor 的 Commit 作为 **replay boundary**；这一定义与本节 first-parent replay 模型绑定，不从多个 criss-cross best merge bases 中按 Commit ID 任取一个；
3. 取 replay boundary（exclusive）到 `oldHead` 的 first-parent Commit sequence，按旧到新顺序 replay；
4. 对每个旧 Commit `C`，使用 `diff(parent1(C), C)` 作为该 Commit 的 intent；在当前 replay head 上以 `parent1(C)` / current replay head / `C` 做 logical-slot three-way application；
5. 无冲突时创建一个新的 single-parent Commit，保留旧 Commit 的 `author` / `message`，使用新的 `committed_at`；旧 Commit 如果是 Merge Commit，其 second-parent topology 默认被 flatten，replay 的是它相对 first parent 的实际 graph/schema change；
6. 全部旧 Commit replay 成功后才把 active Branch 原子移动到最后一个新 Commit。

整个 rebase 在一个 SQLite transaction 内 staged。任何 replay conflict、constraint violation、resource failure 或目标 Branch stale-head 都 rollback **全部新 Commit** 并保持 Branch 不变，不产生半完成 rebase。

Rebase conflict 使用与 merge 相同的 logical slot 和 `base/ours/theirs` shape，并额外包含 `sourceCommit`。`options.resolutions` 使用相同 conflictId resolution format。没有需要 replay 的 Commit 时返回 `status = up_to_date`；成功重写时 `status = rebased`，`rewritten` 按旧到新顺序返回 `{from,to}` pairs。

Rebase conflictId 必须在 **相同 `onto`、相同待 replay source sequence、相同前序 resolution 选择** 下跨整个 operation retry 保持稳定。它不能依赖本次尝试中新建 rewritten Commit 的 ID 或 `committed_at`，因为 conflict rollback 会删除这些临时 Commit，而下一次调用按本节规则使用新的 `committed_at`。对每个 source Commit `C`，Rebase 因此使用 `parent1(C)` 的 stable base identity、当前 replay state 的 canonical logical-state identity、`C` 的 immutable Commit identity、slot 与 `base/ours/theirs` canonical values 生成 deterministic conflictId。当前 replay state identity 只描述 graph / Schema / Index logical slots，不包含 rewritten Commit metadata；前序 resolution 真正改变 replay state 时，后续 conflictId 可以相应改变。这样调用方可以跨多次 `rebase(..., {resolutions:[...]})` 逐步解决位于多个 source Commit 的 conflict，同时不会把旧 resolution 静默套到不同 candidate state。

[Three-way Merge 与 Merge Session](#merge-session) 的 best-common-ancestor / virtual-base 规则仍定义 **Merge** 的三方 base。Rebase 的 replay boundary 解决的是“active Branch 哪些 first-parent Commit 属于待重放序列”这一不同问题；在存在多个 best merge bases 的 criss-cross DAG 中，必须由 active first-parent chain 决定该边界，不能让 hash/Commit-ID 排序偶然改变 rebase 是否可执行。每个待重放 Commit 的冲突判定仍复用 [Three-way Merge 与 Merge Session](#merge-session) 的 logical-slot、dependency 与 post-merge Constraint validation 规则。

Rebase 不自动把旧 Commit Data 复制到 rewritten Commit，也不移动任何 Tag。旧 Commit Data 继续绑定旧 Commit；调用方可以根据 `rewritten` mapping 自行决定是否复制/重建 annotation。Lithograph 不猜测任意业务 JSON 在新 base 上是否仍然成立。

<a id="squash"></a>

## Squash

`squash(since)` 要求 `since` 是 active Branch head 的 ancestor。若 `since == HEAD`，返回 `INVALID_ARGUMENT`，因为没有 Commit 可 squash。

Squash 计算 `diff(since, HEAD)`，创建一个 parent=`since` 的新 single-parent Commit，Layer 表示该完整结构化变化，然后原子把 active Branch 移到新 Commit。原历史 Commit 保持 immutable；如果没有其它 Branch 引用，它们只是变成 unreachable，直到 explicit GC。

Squash 不把原 Commit author/message 列表嵌入新 Commit。新 Commit metadata 使用 execution-level query options 的 `author/message`；省略时为 `null`。Squash 后 graph/schema/index Snapshot 必须与原 HEAD 完全相同。

Squash 不聚合被压缩 Commit 的 Commit Data，也不移动 Tag。新 Commit 默认没有 Data；旧 annotation 仍绑定旧 Commit，直到这些 Commit 后续真正被 GC。

<a id="reset-revert-history"></a>

## Reset、Revert 与 History

- `reset(target)` 原子移动 active Branch ref 到已有 Commit，不删除 Commit；
- `revert(commit)` 计算该 Commit 相对 parent 的 inverse patch，并在 active Branch 创建一个新 Commit；普通 Commit 固定使用 parent1；Merge Commit 必须通过 options 指定 `mainline: 1|2`，否则返回 `INVALID_ARGUMENT`；Root Commit 不能 revert；
- `log` 沿 pinned Commit DAG 分页返回 id、parents、author、message、timestamp 与 opaque continuation；
- 使用 `options.at = commit/<id>|branch/<name>|tag/<name>` 可 time-travel / named-snapshot 查询，Branch / Tag 都在 query 开始时解析到 immutable Commit。

Reset / Revert 不修改既有 Commit Data 或 Tag；Revert 创建的新 Commit 默认没有 Data。

Canonical history 不自动 GC。`lithograph.gc()` 只删除从任何 Branch、Tag **或 open Merge Session** 都不可达的 Commit / Layer；derived checkpoint/index/cache 可以自动回收，因为可重建。

GC 对 canonical objects 按 reachability 删除：Commit 不可达后，其 Commit Data sidecar 一起删除；其 Layer 只有在没有其它 reachable Commit 引用时才删除；Schema object 同理。Tag 与 open Merge Session 都提供 root；Tag 不由 GC 自动删除，Merge Session 只由 finalize/abort 删除。Dictionary identity/name 是 database-global append-only metadata，即使当前没有 reachable Snapshot 使用也不回收，避免 ID 重用和历史/patch 解释变化。

Lithograph 的 versioned-state contract 是**单个 SQLite database 内的本地状态演进机制**；Git / TerminusDB 只提供 Commit DAG、Branch、Diff/Merge 等机制参考，不规定调用方把 Commit 解释成软件版本、时间点、场景还是其它业务状态。Commit/Branch/Tag/Diff/Merge/Rebase/Squash 等全部在同一个 `databaseId` 内工作。跨 SQLite database 或跨网络的 clone/fetch/push/pull 属于复制/传输层，不是 Lithograph Extension v1 的 Version Procedure contract；SQLite backup/file replication 可以复制整个 repository，但两个独立 `databaseId` 不通过 Version API 隐式合并 identity space。
