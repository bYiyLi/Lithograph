# Merge Session Procedure

适用版本：v0.3.3 · Cypher Profile：`CY25-2026.08`。

[产品手册](../../guide/README.md) · [接口参考](../README.md) · [Procedure 索引](../procedures.md) · [Merge 操作指南](../../guide/merge.md)

Merge Session 用于固定两侧 Commit、分批解决冲突、检查候选图，最后一次性发布合并结果。本页完整定义七个 `lithograph.merge.*` Procedure 的调用行为；Version Descriptor、冲突值和共享对象结构见 [版本对象参考](../versioning.md)。

<a id="lifecycle"></a>

## 生命周期与公共执行边界

```text
start → get / conflicts → resolve → 检查 candidate → finalize
          ↑                 │
          └─────────────────┘
                    └────────────────────────────→ abort
```

`ours` 是 target Branch 在 start 时固定的 head，`theirs` 是 source Descriptor 当时解析出的 Commit。source 后续移动不会改变已有 Session。Session 是持久的工作状态，可以跨 connection 和进程恢复；它不长期持有 writer，没有自动 TTL，调用方必须显式 finalize 或 abort。open Session 保护其固定历史免受 GC。

| Procedure | mode | 修改内容 | 新建 Commit / 移动 Branch |
| --- | --- | --- | --- |
| `merge.start` | `WRITE` | 创建 Session，初始 `revision=1` | 均不发生，包括 fast-forward |
| `merge.get` | `READ` | 无 | 均不发生 |
| `merge.list` | `READ` | 无 | 均不发生 |
| `merge.conflicts` | `READ` | 无 | 均不发生 |
| `merge.resolve` | `WRITE` | 原子更新 resolution set 和 revision | 均不发生 |
| `merge.finalize` | `WRITE` | 发布结果并原子删除 Session/resolution | 依 status 决定 |
| `merge.abort` | `WRITE` | 原子删除 Session/resolution | 均不发生 |

这些 Procedure 经 `lithograph()` 或 `lithograph_rows()` 调用，结果是普通 Cypher rows，可使用 `YIELD`、`WITH`、`RETURN` 组合。Version Procedure 与 graph/schema mutation 不能放在同一个 query 中，返回 `INVALID_ARGUMENT`；读写图的准备步骤应分别执行。

所有七项共享以下边界：

- active Lithograph explicit transaction 内拒绝执行，返回 `TRANSACTION_BOUNDARY_REQUIRED`。Merge candidate execution 的 `options.mergeSession` context 也拒绝所有 Version Procedure，返回同一错误。
- 可以在 caller-owned SQLite transaction 中执行；此时 Session/ref/Commit 变化与外层事务一起提交或回滚，成功返回还不表示外层已经 durable。事务期间的 writer 持有时间由外层事务决定。
- 不接受非空 `options.graphView`，返回 `INVALID_ARGUMENT`。Session 操作针对整个 version repository；候选图的普通只读查询另可使用 Graph View。
- 不接受 `options.at`。对 `get/list/conflicts` 返回 `INVALID_ARGUMENT`；对 `start/resolve/finalize/abort` 返回 `READ_ONLY_SNAPSHOT`。
- `options.branch` 仅由 `start` 接受；其余操作从 Session 取得 target，不能再次选择 Branch。`options.author/message` 仅由 `finalize` 接受；其余操作不保存未来 Commit metadata，传入返回 `INVALID_ARGUMENT`。
- `readOnlyNoExternalIO:true` 允许 `get/list/conflicts`，拒绝四个 `WRITE` 操作，返回 `INVALID_ARGUMENT`。它仍受本页其它执行边界约束。

SQL 返回、stream 完整消费与提前关闭、错误编码见 [SQL API](../sql-api.md)、[事务与 cursor 生命周期](../transactions.md)、[执行选项](../execution-options.md) 和 [错误参考](../errors.md)。

<a id="status"></a>

## 状态与结果类型

`start/get/resolve` 的 `status` 使用以下四个值：

| status | 条件 | candidate 内容 |
| --- | --- | --- |
| `up_to_date` | source 已在 target 的历史中，或两侧相同 | pinned `ours` |
| `fast_forward` | `ours` 是 `theirs` 的祖先 | pinned `theirs` |
| `conflicted` | 当前 resolution set 下仍有未解决冲突 | 无完整 candidate，检查时返回 `MERGE_CONFLICT` |
| `ready` | 两侧分叉，当前合并候选图无未解决冲突 | 合并后的候选图 |

`status` 不表示 Branch 已经移动。只有 `finalize` 发布结果，其成功 status 为 `up_to_date`、`fast_forward` 或 `merged`。

Session 状态行由以下列组成，`start/get` 均恰好返回一行：

| 列 | Cypher 类型 | 含义 |
| --- | --- | --- |
| `session` | `STRING` | 引擎生成的 opaque `merge-session/...` identity，不是 Version Descriptor |
| `targetBranch` | `STRING` | target Branch 的名称，没有 `branch/` 前缀 |
| `ours` | `STRING` | pinned target head，完整 `commit/<id>` Descriptor |
| `theirs` | `STRING` | pinned source Commit，完整 `commit/<id>` Descriptor |
| `revision` | `INTEGER` | 从 `1` 开始的正整数；有效 resolution 修改时单调递增 |
| `status` | `STRING` | 上表四个值之一 |
| `unresolved` | `INTEGER` | 当前仍未解决的冲突数量，非负整数 |

<a id="proc-lithograph-merge-start"></a>
<a id="merge-start"></a>

## `lithograph.merge.start`

```text
CALL lithograph.merge.start(source :: STRING [, expectedHead :: STRING])
YIELD session, targetBranch, ours, theirs, revision, status, unresolved
```

| 参数 | 要求与默认值 |
| --- | --- |
| `source` | 必填 Version Descriptor：`branch/<name>`、`tag/<name>` 或 `commit/<id>` |
| `expectedHead` | 可省略；提供时必须为已解析的 `commit/<id>`，不能传 Branch/Tag Descriptor 或 `null` |

target 由 `options.branch` 指定，省略则取 connection 的 active Branch。start 不改变 active checkout。它固定 ours/theirs，计算初始状态，然后持久化 Session；即使返回 `fast_forward` 也不会移动 target。

提供 `expectedHead` 时，target 必须在持久化 Session 的 writer boundary 内仍指向该 Commit，否则返回 `BRANCH_HEAD_MOVED`，不创建 Session。省略时固定本次计算开始所见的 target head；此后 target 的并发变化由 finalize 的 head CAS 检查。已固定 Commit 在持久化前被 GC 时返回 `VERSION_NOT_FOUND`，不留下 partial Session。

结果：一行[完整 Session 状态](#status)。常见错误还包括 Descriptor/参数错误 `INVALID_ARGUMENT`、source 不存在的 `VERSION_NOT_FOUND`、target 不存在的 `BRANCH_NOT_FOUND`；缺少的 Branch/Tag source 按对应 ref 错误报告。公共 options/context 限制见[执行边界](#lifecycle)。

下面先取得当前 head，再用它限制 Session 的输入。新初始化数据库中返回 `main,1,up_to_date,0,true`；Session 保持 open。

<!-- example: mp-start -->
```cypher
CALL lithograph.commit.get('branch/main') YIELD commit AS expected
CALL lithograph.merge.start('branch/main', expected)
YIELD targetBranch, ours, theirs, revision, status, unresolved
RETURN targetBranch, revision, status, unresolved, ours = theirs AS sameCommit
```

同一 start query 使用 SQL options `{"readOnlyNoExternalIO":true}` 会在执行前返回 `INVALID_ARGUMENT`：

<!-- example: mp-start-safe-rejected -->
```cypher
CALL lithograph.merge.start('branch/main') YIELD session RETURN session
```

<a id="proc-lithograph-merge-get"></a>
<a id="merge-get"></a>

## `lithograph.merge.get`

```text
CALL lithograph.merge.get(session :: STRING)
YIELD session, targetBranch, ours, theirs, revision, status, unresolved
```

`session` 必填，使用 start/list 返回的 identity。没有可选参数。恰好返回一行[完整 Session 状态](#status)，状态与 revision 从同一个 SQLite read snapshot 取得。它重新计算当前 conflict 状态，不修改 Session、图或 ref。

Session 已完成、已 abort 或不存在时返回 `MERGE_SESSION_NOT_FOUND`。读到的 revision 用于后续 resolve/finalize/abort 的 CAS；它不会锁住未来的 resolution 修改。

以下示例创建一个自合并 Session，再核对 get 的结果，预期一行 `1,up_to_date,0,true`。

<!-- example: mp-get -->
```cypher
CALL lithograph.merge.start('branch/main') YIELD session
CALL lithograph.merge.get(session)
YIELD session AS fetched, revision, status, unresolved
RETURN revision, status, unresolved, session = fetched AS sameSession
```

<a id="proc-lithograph-merge-list"></a>
<a id="merge-list"></a>

## `lithograph.merge.list`

```text
CALL lithograph.merge.list([limit :: INTEGER [, cursor :: STRING]])
YIELD session, targetBranch, ours, theirs, revision, createdAt, cursor
```

`limit` 默认 `100`，必须为正 `INTEGER`。`cursor` 省略表示第一页；下一页原样传回上一页最后一行的非空 cursor。显式 `null` 不等价于省略。无参数时写作 `merge.list()`。

每个当前存在的 open Session 返回一行，按 session id 的 binary ascending 排序。`session,targetBranch,ours,theirs,revision` 类型同[状态行](#status)；`createdAt` 是 `INTEGER` UNIX epoch 微秒；`cursor` 是 `STRING | null`。只有当还有下一页时，本页最后一行返回非空 continuation；其它行返回 `null`。没有 Session 时返回零行。

list 只列持久化 metadata，不计算 candidate，不返回 `status/unresolved`；需要这些字段时对选中的 Session 调用 get。列表不固定跨页集合，并发 start/finalize/abort 可能改变后续页。需要最新完整清单时从第一页重新枚举。

非法 limit、损坏或非本类 cursor 返回 `INVALID_ARGUMENT`。以下示例在空数据库创建唯一一个 Session，返回一行 `true,main,1,true,null`。

<!-- example: mp-list -->
```cypher
CALL lithograph.merge.start('branch/main') YIELD session AS created
CALL lithograph.merge.list(1)
YIELD session, targetBranch, revision, createdAt, cursor
RETURN session = created AS sameSession, targetBranch, revision,
       createdAt > 0 AS hasCreationTime, cursor
```

<!-- example: mp-list-invalid-limit -->
```cypher
CALL lithograph.merge.list(0) YIELD session RETURN session
```

上例返回 `INVALID_ARGUMENT`。安全只读模式可以枚举 Session；以下在没有 Session 的临时数据库执行，SQL options 为 `{"readOnlyNoExternalIO":true}`，返回零行。

<!-- example: mp-list-safe -->
```cypher
CALL lithograph.merge.list() YIELD targetBranch, revision
RETURN targetBranch, revision
```

<a id="conflict-fixture"></a>

## 冲突示例的前置数据

本页后续冲突示例在独立临时数据库按以下顺序准备。`main` 和 `review` 从相同节点分叉，对同一 `name` Property 写入不同值。

<!-- example: mp-fixture-base -->
```cypher
CREATE (:Item {key:'document', name:'base'}) FINISH
```

<!-- example: mp-fixture-branch -->
```cypher
CALL lithograph.branch.create('review') YIELD name RETURN name
```

在 `main` 上执行：

<!-- example: mp-fixture-ours -->
```cypher
MATCH (n:Item {key:'document'}) SET n.name='ours' RETURN n.name AS name
```

接着对同一 database 使用 SQL options `{"branch":"review"}` 执行：

<!-- example: mp-fixture-theirs -->
```cypher
MATCH (n:Item {key:'document'}) SET n.name='theirs' RETURN n.name AS name
```

除非另有说明，每个后续冲突例子都从这四步准备后的状态独立开始，不接续前一个示例留下的 Session。

<a id="proc-lithograph-merge-conflicts"></a>
<a id="merge-conflicts"></a>

## `lithograph.merge.conflicts`

```text
CALL lithograph.merge.conflicts(session :: STRING
  [, limit :: INTEGER [, cursor :: STRING]])
YIELD session, revision, conflictId, slot, base, ours, theirs, resolution, cursor
```

`session` 必填；`limit` 默认 `100`，要求正整数；`cursor` 可省略，下一页原样传回上一页最后一个非空 continuation。传 `null` 不能用于跳过可选位置。

| 返回列 | Cypher 类型 | 含义 |
| --- | --- | --- |
| `session` | `STRING` | 本页所属 Session |
| `revision` | `INTEGER` | 本页固定的 Session revision |
| `conflictId` | `STRING` | 由固定 merge inputs 和冲突内容决定的 identity，供 resolve 使用 |
| `slot` | `STRING` | 冲突的 logical slot，供显示和选择处理方式 |
| `base`、`ours`、`theirs` | 对应 slot 的值或 `null` | 三方值；无该项时为 `null` |
| `resolution` | `MAP | null` | 已保存的 `{choice[,value]}`；尚未解决时为 `null` |
| `cursor` | `STRING | null` | 从该行之后继续；到遍历末尾为 `null` |

返回包含已解决和未解决的当前 conflict inventory，按 `slot` UTF-8 bytes、再按 `conflictId` bytes 升序。没有 conflict 时返回零行。Node、Relationship、Label、Property、Schema 的 slot/value 结构见[版本对象参考](../versioning.md)。删除与修改、Relationship 端点依赖、合并后 Constraint 违反也可能产生冲突，不能只检查 Property 值。

cursor 绑定 Session 和 revision；resolution 有效变化后旧 cursor 返回 `MERGE_SESSION_CHANGED`，必须从新 revision 的第一页重新开始。另一个 Session 的合法 conflict cursor 也返回此错误；格式或校验无效返回 `INVALID_ARGUMENT`。Session 不存在返回 `MERGE_SESSION_NOT_FOUND`。

准备[冲突前置数据](#conflict-fixture)后执行，预期一行 `conflicted,1,base,ours,theirs,null,null`。

<!-- example: mp-conflicts -->
```cypher
CALL lithograph.merge.start('branch/review') YIELD session, status, unresolved
CALL lithograph.merge.conflicts(session, 10)
YIELD base, ours, theirs, resolution, cursor
RETURN status, unresolved, base, ours, theirs, resolution, cursor
```

<a id="proc-lithograph-merge-resolve"></a>
<a id="merge-resolve"></a>

## `lithograph.merge.resolve`

```text
CALL lithograph.merge.resolve(session :: STRING,
  expectedRevision :: INTEGER, resolutions :: LIST<MAP>)
YIELD session, revision, status, unresolved
```

三个参数均必填。`expectedRevision` 为正整数，必须精确等于当前 revision；`resolutions` 可以为空列表。每个 item 使用刚从 conflicts 读取的 `conflictId`：

```text
{conflictId: <id>, choice: 'ours'}
{conflictId: <id>, choice: 'theirs'}
{conflictId: <id>, choice: 'value', value: <typed Cypher value>}
```

`ours/theirs` 不能带 `value`。`value` 方式必须显式提供该字段；`null` 表示删除该 slot，并不是把 Property 存成 null。Node existence 和 Label membership 的非空值只能为 `true`；Property 必须是合法可存储值；Relationship 和 Schema 值必须满足对应结构。完整形状见[版本对象参考](../versioning.md)。通常选择 ours/theirs；需要替换具体 Property 或删除一项时使用 value。

同一调用原子 set/replace 一批选择。整批与当前保存值完全相同，或空列表时，revision 不变；任何有效变化使 revision 恰好加一。每次以**返回的** revision 发起下一步。旧 revision 即使提交相同 payload，也返回 `MERGE_SESSION_CHANGED`，不能把它当成请求幂等重放。

成功恰好返回一行 `session :: STRING, revision :: INTEGER, status :: STRING, unresolved :: INTEGER`。status 使用[状态表](#status)。它不生成 Commit、不移动任何 Branch。解决冲突可能使某些旧 conflict 消失或暴露新的 Constraint conflict，应读取当前 status/unresolved 决定是否继续。暂时消失的已保存 resolution 保留为 dormant；相同 conflictId 重新出现时可重新生效。

Session 不存在返回 `MERGE_SESSION_NOT_FOUND`；revision 过期返回 `MERGE_SESSION_CHANGED`；unknown/duplicate conflictId、非法 choice、缺少 value、slot/value 类型不匹配返回 `INVALID_ARGUMENT`。失败时整批不写入。合法选择仍留下其它冲突时是成功，`unresolved>0`，不以异常替代可继续的 Session 状态。

以下例子选择 source 一侧，预期 `2,ready,0`，Session 保持 open。使用[冲突前置数据](#conflict-fixture)。

<!-- example: mp-resolve -->
```cypher
CALL lithograph.merge.start('branch/review') YIELD session, revision
CALL lithograph.merge.conflicts(session, 10) YIELD conflictId
CALL lithograph.merge.resolve(session, revision,
  [{conflictId:conflictId, choice:'theirs'}])
YIELD revision AS nextRevision, status, unresolved
RETURN nextRevision, status, unresolved
```

在当前 revision 上保存同一选择不再递增。下面返回 `2,2`：

<!-- example: mp-resolve-noop -->
```cypher
CALL lithograph.merge.start('branch/review') YIELD session, revision
CALL lithograph.merge.conflicts(session, 10) YIELD conflictId
CALL lithograph.merge.resolve(session, revision,
  [{conflictId:conflictId, choice:'theirs'}]) YIELD revision AS firstRevision
CALL lithograph.merge.resolve(session, firstRevision,
  [{conflictId:conflictId, choice:'theirs'}]) YIELD revision AS repeatedRevision
RETURN firstRevision, repeatedRevision
```

显式替换值的例子也返回 `2,ready,0`：

<!-- example: mp-resolve-value -->
```cypher
CALL lithograph.merge.start('branch/review') YIELD session, revision
CALL lithograph.merge.conflicts(session, 10) YIELD conflictId
CALL lithograph.merge.resolve(session, revision,
  [{conflictId:conflictId, choice:'value', value:'reviewed'}])
YIELD revision AS nextRevision, status, unresolved
RETURN nextRevision, status, unresolved
```

以下重复 conflictId 的整批调用返回 `INVALID_ARGUMENT`：

<!-- example: mp-resolve-duplicate -->
```cypher
CALL lithograph.merge.start('branch/review') YIELD session, revision
CALL lithograph.merge.conflicts(session, 10) YIELD conflictId
CALL lithograph.merge.resolve(session, revision,
  [{conflictId:conflictId, choice:'ours'},
   {conflictId:conflictId, choice:'theirs'}])
YIELD revision AS nextRevision RETURN nextRevision
```

<a id="candidate-inspection"></a>

## 在已确认 revision 上检查 candidate

candidate inspection 使用普通只读查询，SQL options 传入实际返回值：

```json
{"mergeSession":{"id":"merge-session/<actual-id>","revision":2}}
```

在上面的 `mp-resolve-value` 后，以这些 options 执行 `MATCH (n:Item) RETURN n.name`，结果为 `reviewed`。JSON 中的 identity 是占位说明；实际代码应绑定返回的 Session identity，不按示意字符串执行。

每次 execution 原子读取 Session、验证 revision 并固定 resolution set。`summary.commit=null`，`summary.mergeSession` 标明实际 id/revision。可以运行多个只读验证，然后用同一 revision finalize；其间 resolution 变化会使旧 finalize 返回 `MERGE_SESSION_CHANGED`。

candidate 允许普通只读 Cypher、Search 和 Schema introspection，也可以叠加 query-local Graph View；不允许 Version Procedure、LOAD CSV 或 transaction-owning subquery，返回 `TRANSACTION_BOUNDARY_REQUIRED`。graph/schema/index 写入返回 `READ_ONLY_SNAPSHOT`。`mergeSession` 与 `branch/at/author/message` 互斥。完整 options 见[执行选项](../execution-options.md)。

<a id="proc-lithograph-merge-finalize"></a>
<a id="merge-finalize"></a>

## `lithograph.merge.finalize`

```text
CALL lithograph.merge.finalize(session :: STRING, expectedRevision :: INTEGER)
YIELD status, commit
```

两个参数必填；`expectedRevision` 为正整数。target 固定在 Session 中，不能再传 `options.branch`。execution-level `options.author/message` 仅在创建新的 Merge Commit 时生效；没有 Procedure options Map。

finalize 同时要求 Session 存在、revision 匹配、`unresolved=0`，以及 target head 仍等于 pinned `ours`。后两个 CAS 检查在 writer boundary 内再次确认；发生失败时 Session 和已保存 resolution 保留。

恰好返回一行 `status :: STRING, commit :: STRING`，commit 始终是非空 `commit/<id>`：

| status | 返回的 commit | canonical/ref 效果 |
| --- | --- | --- |
| `up_to_date` | pinned `ours` | 无新 Commit，Branch 不动 |
| `fast_forward` | pinned `theirs` | 无新 Commit，target Branch 移到 theirs |
| `merged` | 新 Merge Commit | parent1 为 ours，parent2 为 theirs，Layer 相对 ours；target 移到新 Commit |

所有成功分支都原子删除 Session 及 resolution。Commit/ref 变化与删除在同一个 SQLite transaction 中完成。它不移动 Tag，不合并 Commit Data；新 Merge Commit 默认没有 Commit Data，若需要在成功后另行设置。fast-forward 指向的已有 Commit 保留其原有 Data。

关键错误：`MERGE_SESSION_NOT_FOUND`、`MERGE_SESSION_CHANGED`、`MERGE_CONFLICT`、`BRANCH_NOT_FOUND`、`BRANCH_HEAD_MOVED`。target 已移动时不能给旧 Session 换 target 或 ours；重新 start 并审查新输入，不直接重放旧 resolution。Schema/Constraint、Provider definition 或 storage 验证失败也不会发布部分 Commit/ref 变化。

用[冲突前置数据](#conflict-fixture)运行完整解决与提交例子，预期 `ready,0,merged,2`。这里把步骤合在一次 query 便于演示；实际需要独立 candidate 检查时，分开调用并保存 Session/revision。

<!-- example: mp-finalize -->
```cypher
CALL lithograph.merge.start('branch/review') YIELD session, revision
CALL lithograph.merge.conflicts(session, 10) YIELD conflictId
CALL lithograph.merge.resolve(session, revision,
  [{conflictId:conflictId, choice:'theirs'}])
YIELD revision AS acceptedRevision, status AS prepared, unresolved
CALL lithograph.merge.finalize(session, acceptedRevision) YIELD status, commit
CALL lithograph.commit.get(commit) YIELD parents
RETURN prepared, unresolved, status, size(parents) AS parentCount
```

以下自合并无需前置数据，预期 `up_to_date,true`：

<!-- example: mp-finalize-up-to-date -->
```cypher
CALL lithograph.merge.start('branch/main') YIELD session, revision
CALL lithograph.merge.finalize(session, revision) YIELD status, commit
RETURN status, commit STARTS WITH 'commit/' AS resolvedCommit
```

fast-forward 的前置数据只执行[前置数据](#conflict-fixture)的 base、branch 两步，然后用 SQL options `{"branch":"review"}` 写入 source；不执行 main 的 `ours` 修改：

<!-- example: mp-fixture-fast-forward -->
```cypher
MATCH (n:Item {key:'document'}) SET n.name='source-only' RETURN n.name AS name
```

此后运行下例，start 仅建立 Session，finalize 才移动 Branch。返回 `fast_forward,fast_forward,true`，没有新 Merge Commit：

<!-- example: mp-finalize-fast-forward -->
```cypher
CALL lithograph.merge.start('branch/review')
YIELD session, revision, status AS prepared, theirs
CALL lithograph.merge.finalize(session, revision) YIELD status, commit
RETURN prepared, status, commit = theirs AS sourceCommit
```

以下有未解决冲突的调用返回 `MERGE_CONFLICT`，不会生成中间合并 Commit：

<!-- example: mp-finalize-unresolved -->
```cypher
CALL lithograph.merge.start('branch/review') YIELD session, revision
CALL lithograph.merge.finalize(session, revision) YIELD status RETURN status
```

<a id="proc-lithograph-merge-abort"></a>
<a id="merge-abort"></a>

## `lithograph.merge.abort`

```text
CALL lithograph.merge.abort(session :: STRING, expectedRevision :: INTEGER)
YIELD session
```

两个参数必填；revision 为正整数。短 writer boundary 内按 revision CAS 删除 Session 和全部 resolution。成功恰好返回一行 `session :: STRING`，不产生图 Commit、不移动 Branch/Tag。删除后 Session 不再保护历史免受后续显式 GC。

不存在返回 `MERGE_SESSION_NOT_FOUND`；revision 已变化返回 `MERGE_SESSION_CHANGED`，保留 Session。不要以 stale revision 强制清理其它调用方刚更新的工作。finalize/abort 请求结果未知时，先 get/list 并检查 target 状态，不把 not-found 当成自动再次 start 的理由。

无前置数据的例子返回 `true`：

<!-- example: mp-abort -->
```cypher
CALL lithograph.merge.start('branch/main') YIELD session, revision
CALL lithograph.merge.abort(session, revision) YIELD session AS removed
RETURN session = removed AS removed
```

以下在[冲突前置数据](#conflict-fixture)上先完成一次有效 resolve，再故意用旧 revision abort，返回 `MERGE_SESSION_CHANGED`：

<!-- example: mp-abort-stale -->
```cypher
CALL lithograph.merge.start('branch/review') YIELD session, revision
CALL lithograph.merge.conflicts(session, 10) YIELD conflictId
CALL lithograph.merge.resolve(session, revision,
  [{conflictId:conflictId, choice:'ours'}]) YIELD revision AS nextRevision
CALL lithograph.merge.abort(session, revision) YIELD session AS removed
RETURN removed
```

<a id="errors-and-recovery"></a>

## 错误优先级与恢复

在参数和 execution context 合法的前提下，依次检查 Session 不存在、revision 不匹配、未解决冲突，再检查 finalize 的 target Branch 是否存在及 head 是否匹配。依次对应 `MERGE_SESSION_NOT_FOUND`、`MERGE_SESSION_CHANGED`、`MERGE_CONFLICT`、`BRANCH_NOT_FOUND` / `BRANCH_HEAD_MOVED`。不能用旧 revision 的业务验证结果继续提交。

- `MERGE_SESSION_CHANGED`：重新 get/conflicts，审查新的 revision 后再决定选择和提交。
- `MERGE_CONFLICT`：分页查看当前冲突并 resolve，不通过 reset target 退出 Session。
- `BRANCH_HEAD_MOVED`：旧 Session 保留供审查；建立新的 Session 处理变化后的输入，确认后清理旧 Session。
- `MERGE_SESSION_NOT_FOUND`：检查是否已 finalize/abort；对结果未知的调用先核对持久状态。

以下在新初始化数据库读取不存在的 Session，返回 `MERGE_SESSION_NOT_FOUND`：

<!-- example: mp-get-missing -->
```cypher
CALL lithograph.merge.get('merge-session/00000000-0000-4000-8000-000000000000')
YIELD session RETURN session
```

恢复操作的完整流程见 [Merge 指南](../../guide/merge.md)；字段与 logical slot 见[版本对象参考](../versioning.md)，跨连接资源管理见[事务参考](../transactions.md)。
