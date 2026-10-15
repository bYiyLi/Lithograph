# 版本管理 Procedure

[产品手册](../../guide/README.md) · [Procedure 参考](../procedures.md) · [版本对象格式](../versioning.md)

适用版本：**Lithograph v0.3.1**，Cypher Profile `CY25-2026.08`，storage format `3`。本页给出 Branch、Tag、Commit、History、Diff/Patch 与历史改写的完整调用合同；[Merge Session](merge.md) 和 [维护](maintenance.md) 分别说明合并与 GC。

下面 `[...]` 表示可以省略的位置参数；调用时不要输入方括号。参数为非空 `STRING`，除非条目明确给出其它类型。`null` 不代表“使用默认值”。返回的 `commit`、`previousCommit`、`from`、`to`、`parents` 元素都是 `commit/<64-hex-id>` Descriptor。

<a id="execution-context"></a>

## 执行上下文与共同失败条件

版本操作处理整个 database 的版本状态，不能用 Graph View 缩小其作用范围。它们的 `mode` 为 `WRITE` 时，即使只修改 ref 或 Data，也属于受限只读执行所拒绝的写操作。

| 上下文 | 规则 |
| --- | --- |
| 普通 `lithograph()` / `lithograph_rows()` | 均可调用；写入必须完成整个 execution，rows 使用者必须读到成功 summary |
| active Lithograph explicit transaction | 所有 Version Procedure 都返回 `TRANSACTION_BOUNDARY_REQUIRED`；先 commit/rollback 该事务 |
| caller-owned SQLite transaction | 除 checkout 外，本页操作可以参与外层事务，外层 rollback 撤销持久改动；checkout 要求 SQLite autocommit |
| `options.at` | 不用于 Version Procedure；READ operation 返回 `INVALID_ARGUMENT`，WRITE operation 返回 `READ_ONLY_SNAPSHOT`。读取历史 Commit 应把 Descriptor 传给 `commit.get`、`log`、`diff` |
| `options.graphView` | 受限 Graph View 返回 `INVALID_ARGUMENT`，包括只读版本操作 |
| `options.mergeSession` | candidate context 不允许 Version Procedure，返回 `TRANSACTION_BOUNDARY_REQUIRED` |
| graph mutation clause | 不能在同一 query 中混合 `CREATE` / `SET` 等 graph mutation 和 Version Procedure，返回 `INVALID_ARGUMENT`；分成独立 execution |
| `options.branch` | 本页只有 `commit.create`、`patch.apply`、`rebase`、`squash`、`reset`、`revert` 接受；选择目标 Branch，不改变 connection checkout |
| `options.author` / `message` | 本页只有 `commit.create`、`patch.apply`、`squash`、`revert` 接受，用于新 Commit；其它操作提供该 key 即返回 `INVALID_ARGUMENT` |

输出 columns 按本页表格定义。不要依据旧版 `SHOW PROCEDURES.returnDescription[*].type` 把所有结果转成字符串；`BOOLEAN`、`INTEGER`、`LIST`、`MAP` 和 `null` 必须保留实际类型，宿主编码见 [Values](../values.md)。

语法错误、错误参数数量、`null` 或不合法的参数类型返回 `INVALID_ARGUMENT` 或查询编译错误。Descriptor 格式错误为 `INVALID_ARGUMENT`；合法 Descriptor 的目标不存在时分别为 `BRANCH_NOT_FOUND`、`TAG_NOT_FOUND`、`VERSION_NOT_FOUND`。目标 Branch 并发变动可能为 `BRANCH_HEAD_MOVED`；SQLite writer 竞争可能为 `BUSY`。失败不会发布半个 Commit 或部分 Branch head 更新。

<a id="branch-create"></a>

<a id="proc-lithograph-branch-create"></a>

## `lithograph.branch.create`

```text
CALL lithograph.branch.create(name [, from])
```

Mode：`WRITE`。`name` 是 Branch name，不是 Descriptor；格式见 [引用名称](../versioning.md#reference-names)。`from` 是可选 Version Descriptor，省略时取 operation 开始时的 active Branch head。创建 ref 后不 checkout、不复制图数据、不创建 Commit。`options.branch` 和 author/message 不适用。

返回一行：`name :: STRING, commit :: STRING`。同名 Branch 已存在、非法 name 为 `INVALID_ARGUMENT`。Branch namespace 与 Tag 分开。

<!-- example: vp-branch-create -->
```cypher
CALL lithograph.branch.create('feature') YIELD name, commit
RETURN name, commit STARTS WITH 'commit/' AS valid
```

空 database 初始化后返回 `['feature', true]`。指定来源时可写 `CALL lithograph.branch.create('review', 'tag/baseline')`；该 Tag 必须已经存在。

<a id="branch-list"></a>

<a id="proc-lithograph-branch-list"></a>

## `lithograph.branch.list`

```text
CALL lithograph.branch.list()
```

Mode：`READ`。无参数。每个 Branch 返回 `name :: STRING, commit :: STRING, active :: BOOLEAN`，按 name 的 UTF-8 binary 升序。`active` 只表示当前 connection 的 checkout，不由 `options.branch` 选择。

在上一例创建 `feature` 后：

<!-- example: vp-branch-list -->
```cypher
CALL lithograph.branch.list() YIELD name, active
RETURN name, active ORDER BY name
```

结果为 `[['feature', false], ['main', true]]`。需要稳定 Snapshot 身份时保存 `commit`，不要把可移动 Branch name 当成不可变版本。

<a id="branch-checkout"></a>

<a id="proc-lithograph-branch-checkout"></a>

## `lithograph.branch.checkout`

```text
CALL lithograph.branch.checkout(name)
```

Mode：`WRITE`，但只改变当前 SQLite connection 的 active Branch，不创建 Commit、不移动 ref。新 connection 默认 `main`。`name` 必须是已存在 Branch 的裸名称；返回一行 `name :: STRING, commit :: STRING`。

调用前 connection 必须处于 SQLite autocommit，外层 `BEGIN` / savepoint 中调用为 `TRANSACTION_BOUNDARY_REQUIRED`。不存在为 `BRANCH_NOT_FOUND`。连接池归还前应自行恢复预期 checkout，详见 [宿主事务生命周期](../transactions.md)。

在创建 `feature` 后：

<!-- example: vp-branch-checkout -->
```cypher
CALL lithograph.branch.checkout('feature') YIELD name
RETURN name
```

结果为 `[['feature']]`；之后省略 branch 的图写入以 `feature` 为目标。

<a id="branch-delete"></a>

<a id="proc-lithograph-branch-delete"></a>

## `lithograph.branch.delete`

```text
CALL lithograph.branch.delete(name)
```

Mode：`WRITE`。删除一个 Branch ref，返回 `name :: STRING, previousCommit :: STRING` 一行。既不删除目标 Commit，也不创建 Commit。禁止删除 `main` 或当前 connection 的 active Branch，均为 `INVALID_ARGUMENT`；不存在为 `BRANCH_NOT_FOUND`。

其它 connection 仍 checkout 被删除的 Branch 时，下一次依赖该 active Branch 的 query 返回 `BRANCH_NOT_FOUND`，直到 checkout 到存在的 Branch。

在创建但尚未 checkout `feature` 后：

<!-- example: vp-branch-delete -->
```cypher
CALL lithograph.branch.delete('feature') YIELD name
RETURN name
```

结果为 `[['feature']]`。以下调用总是失败：

<!-- example: vp-branch-delete-main-error -->
```cypher
CALL lithograph.branch.delete('main')
```

预期错误：`INVALID_ARGUMENT`。

<a id="tag-create"></a>

<a id="proc-lithograph-tag-create"></a>

## `lithograph.tag.create`

```text
CALL lithograph.tag.create(name, target)
```

Mode：`WRITE`。`name` 是 Tag name；`target` 必须是 Descriptor，解析一次后保存 resolved Commit。返回 `name :: STRING, commit :: STRING` 一行。Tag 不参与 checkout，不随 graph write 移动，不创建 Commit；同名已存在为 `INVALID_ARGUMENT`。

<!-- example: vp-tag-create -->
```cypher
CALL lithograph.tag.create('baseline', 'branch/main') YIELD name
RETURN name
```

结果为 `[['baseline']]`。Tag 会保护其 target 及祖先免于 GC；Tag 是可显式移动的命名引用，不是 immutable label。

<a id="tag-list"></a>

<a id="proc-lithograph-tag-list"></a>

## `lithograph.tag.list`

```text
CALL lithograph.tag.list()
```

Mode：`READ`。无参数。每个 Tag 返回 `name :: STRING, commit :: STRING`，按 name binary 升序，没有分页参数。不存在 Tag 时返回零行。

在创建 `baseline` 后：

<!-- example: vp-tag-list -->
```cypher
CALL lithograph.tag.list() YIELD name
RETURN name ORDER BY name
```

结果为 `[['baseline']]`。

<a id="tag-move"></a>

<a id="proc-lithograph-tag-move"></a>

## `lithograph.tag.move`

```text
CALL lithograph.tag.move(name, target)
```

Mode：`WRITE`。显式移动一个现有 Tag，`target` 为 Descriptor。返回 `name :: STRING, previousCommit :: STRING, commit :: STRING` 一行。不存在 Tag 为 `TAG_NOT_FOUND`；目标不存在为相应 Descriptor 错误。不创建 Commit，也不改变任何 Branch。

在创建 `baseline` 后执行 [创建显式 Commit 的示例](#commit-create)，使当前 `main` 增加一个 Commit，再移动 Tag：

<!-- example: vp-tag-move -->
```cypher
CALL lithograph.tag.move('baseline', 'branch/main')
YIELD name, previousCommit, commit
RETURN name, previousCommit <> commit AS moved
```

结果为 `[['baseline', true]]`。需要防止应用层误移动 release 标签时，应在宿主权限层限制此调用。

<a id="tag-delete"></a>

<a id="proc-lithograph-tag-delete"></a>

## `lithograph.tag.delete`

```text
CALL lithograph.tag.delete(name)
```

Mode：`WRITE`。返回 `name :: STRING, previousCommit :: STRING` 一行。仅删除 Tag ref；不存在为 `TAG_NOT_FOUND`。目标 Commit 未被直接删除，但若它随后失去所有 Branch / Tag / open Merge Session 保护，GC 可以回收它。

在创建 `baseline` 后：

<!-- example: vp-tag-delete -->
```cypher
CALL lithograph.tag.delete('baseline') YIELD name
RETURN name
```

结果为 `[['baseline']]`。

<a id="commit-get"></a>

<a id="proc-lithograph-commit-get"></a>

## `lithograph.commit.get`

```text
CALL lithograph.commit.get(version)
```

Mode：`READ`。`version` 接受三种 Descriptor。读取 Commit immutable metadata 与 operation 时刻的 mutable Data；不修改任何 ref。

| 列 | 实际类型 / 含义 |
| --- | --- |
| `commit` | STRING，resolved Commit Descriptor |
| `parents` | LIST&lt;STRING&gt;，依次 first parent、second parent；Root 为 `[]` |
| `author`, `message` | STRING 或 null，Commit 创建后不可变 |
| `committedAt` | INTEGER，UNIX epoch **微秒** |
| `hasData` | BOOLEAN，是否存在 Data sidecar |
| `data` | JSON-compatible Cypher value；没有 sidecar 时为 null |

<!-- example: vp-commit-get -->
```cypher
CALL lithograph.commit.get('branch/main') YIELD parents, hasData, data
RETURN size(parents) AS parentCount, hasData, data
```

刚初始化的 database 返回 `[[0, false, null]]`。同一个 `commit` 的 Data 后续可以变化；immutable 审计事实应写入 versioned graph。

<a id="commit-create"></a>

<a id="proc-lithograph-commit-create"></a>

## `lithograph.commit.create`

```text
CALL lithograph.commit.create([data])
```

Mode：`WRITE`。在 `options.branch` 或 active Branch 上创建 **empty-delta single-parent Commit**；parent 为目标当前 head，Graph / Schema / Index Snapshot 不变，但 Commit identity 改变。返回一行 `commit :: STRING`。支持 execution-level author/message；省略时为 null。

`data` 省略则无 sidecar；显式 `null` 则创建一个存在且值为 null 的 sidecar。其余合法值为 JSON-compatible Boolean、Integer、有限 Float、String、List、Map，可任意嵌套；Node、Relationship、Path、Temporal、Point、Vector 等原生值不能直接作为 Data，非法值为 `INVALID_ARGUMENT`。新 Commit 与 Data 原子写入。

<!-- example: vp-commit-create -->
```cypher
CALL lithograph.commit.create({title: 'checkpoint', stage: 1}) YIELD commit
RETURN commit STARTS WITH 'commit/' AS valid
```

返回 `[[true]]`。这是建立显式状态节点的 API，不存在 working tree / staging。

<a id="commit-data-set"></a>

<a id="proc-lithograph-commit-data-set"></a>

## `lithograph.commit.data.set`

```text
CALL lithograph.commit.data.set(version, data)
```

Mode：`WRITE`。`version` 是 Descriptor；Data 合法值同 `commit.create`。设置或**替换整个** JSON 值，不是 JSON merge。返回 `commit :: STRING, data :: ANY` 一行。不创建 Commit、不改 Snapshot、不移动 Branch / Tag；不接受 author/message。

<!-- example: vp-data-set -->
```cypher
CALL lithograph.commit.data.set('branch/main', {stage: 2, approved: true})
YIELD data
RETURN data.stage AS stage, data.approved AS approved
```

结果为 `[[2, true]]`。显式 JSON null 是合法 Data：

<!-- example: vp-data-null -->
```cypher
CALL lithograph.commit.data.set('branch/main', null) YIELD data
RETURN data
```

返回 `[[null]]`。执行后读取可区分“存在且值为 null”与“不存在”：

<!-- example: vp-data-null-get -->
```cypher
CALL lithograph.commit.get('branch/main') YIELD hasData, data
RETURN hasData, data
```

结果为 `[[true, null]]`。

<a id="commit-data-clear"></a>

<a id="proc-lithograph-commit-data-clear"></a>

## `lithograph.commit.data.clear`

```text
CALL lithograph.commit.data.clear(version)
```

Mode：`WRITE`。删除 resolved Commit 的 Data sidecar，返回 `commit :: STRING`。已经没有 Data 时仍成功；不存在的 Commit 仍报版本错误。不创建 Commit或移动 ref。删除后 `commit.get` 返回 `hasData=false, data=null`。

<!-- example: vp-data-clear -->
```cypher
CALL lithograph.commit.data.clear('branch/main') YIELD commit
RETURN commit STARTS WITH 'commit/' AS valid
```

返回 `[[true]]`。

<a id="log"></a>

<a id="proc-lithograph-log"></a>

## `lithograph.log`

```text
CALL lithograph.log([version [, limit [, cursor]]])
```

Mode：`READ`。`version` 省略时为 active Branch；`limit` 默认 `100`，必须为正 Integer。cursor 必须省略或传引擎返回的非空 opaque String，不接受 null、offset 或手工构造的 token。位置参数决定了要设置 limit 时须同时传 version。

每行返回 `commit :: STRING, parents :: LIST<STRING>, author :: STRING?, message :: STRING?, committedAt :: INTEGER, cursor :: STRING?`。从 pinned start 遍历整个可达 Commit DAG，保证 child 在 parent 之前；同时可输出的 Commit 按 committedAt 降序、Commit ID 升序。合并历史不等同于 first-parent 列表。每行 cursor 表示“从该行之后继续”，遍历结束为 null。不展开 Commit Data。

<!-- example: vp-log -->
```cypher
CALL lithograph.log('branch/main', 1)
YIELD parents, cursor
RETURN size(parents) AS parentCount, cursor
```

空 database 的 Root 返回 `[[0, null]]`。多页时保存第一页 resolved 起点和最后一行非空 cursor，下一页传同一起点、limit、cursor。cursor 自身 pin 住 traversal；Branch / Tag 后续移动不会改变该 traversal，但 cursor 不是 GC root，需要长期分页时用 Tag 保护起点。

在执行 [创建显式 Commit 的示例](#commit-create) 后有两个 Commit。以下把第一页的 resolved 起点和 cursor 直接交给第二次调用，取到 Root：

<!-- example: vp-log-continuation -->
```cypher
CALL lithograph.log('branch/main', 1) YIELD commit, cursor
CALL lithograph.log(commit, 1, cursor) YIELD parents, cursor AS nextCursor
RETURN size(parents) AS parentCount, nextCursor
```

结果为 `[[0, null]]`。此例只有两页；一般分页循环须在 cursor=null 时停止，而不是继续把 null 传给 log。

非法 cursor / limit 为 `INVALID_ARGUMENT`，已被 GC 回收的 pinned Commit 为 `VERSION_NOT_FOUND`。cursor 结构不属于可解析接口，参见 [History 对象](../versioning.md#history-cursor)。

<!-- example: vp-log-limit-error -->
```cypher
CALL lithograph.log('branch/main', 0)
```

预期错误：`INVALID_ARGUMENT`。

<a id="diff"></a>

<a id="proc-lithograph-diff"></a>

## `lithograph.diff`

```text
CALL lithograph.diff(before, after)
```

Mode：`READ`。两个参数均为同一 database 中的 Descriptor，可以是任意祖先/分叉 Snapshot。返回一行 `patch :: MAP`，结构见 [Patch](../versioning.md#patch)。Patch 只描述 graph / Schema / Index 变化，不包含 Branch、Tag、Commit Data 或 Commit metadata。

<!-- example: vp-diff -->
```cypher
CALL lithograph.diff('branch/main', 'branch/main') YIELD patch
RETURN patch.format AS format, size(patch.operations) AS operations,
       patch.from = patch.to AS sameIdentity
```

结果为 `[[1, 0, true]]`。不同 Commit 也可有空 diff，例如 empty-delta Commit 和 parent；空 patch 不表示 identity 相同。

<a id="patch-apply"></a>

<a id="proc-lithograph-patch-apply"></a>

## `lithograph.patch.apply`

```text
CALL lithograph.patch.apply(patch)
```

Mode：`WRITE`。参数为 `diff` 返回的完整 Map。目标为 `options.branch` 或 active Branch；支持 execution author/message。先验证 format、databaseId、slot 唯一性、typed before/after 和所有 before conditions，再验证最终 graph / Schema / Constraint / Provider 状态；全部成立后原子创建 single-parent Commit 并移动目标 Branch。返回一行 `commit :: STRING`。

`from` / `to` 是 provenance；不要求当前 head 等于 `from`，也不会把 Branch 直接移到 `to`。cross-database Patch、重复 slot、未知 operation、伪造或未分配 identity、before 不匹配均为 `INVALID_ARGUMENT`；约束与 Provider validation 使用各自错误。任一失败整体 rollback。

<!-- example: vp-patch-empty -->
```cypher
CALL lithograph.diff('branch/main', 'branch/main') YIELD patch
CALL lithograph.patch.apply(patch) YIELD commit
RETURN commit STARTS WITH 'commit/' AS valid
```

返回 `[[true]]`，即使空 Patch 也建立新的 Commit。跨宿主传输时使用参数绑定，并原样保留 [tagged value 编码](../values.md)；不要用普通 JSON stringify/parse 改写 Temporal、Vector 或大 Integer。

下面是包含实际 graph change 的 round-trip。先执行 [创建 baseline Tag 的示例](#tag-create) 保存空图基准，再执行本文的 [写入教学记录的示例](#version-example-write) 建立教学节点；创建另一个 Branch 并 checkout 到基准：

<!-- example: vp-patch-branch -->
```cypher
CALL lithograph.branch.create('replay', 'tag/baseline') YIELD name
RETURN name
```

<!-- example: vp-patch-checkout -->
```cypher
CALL lithograph.branch.checkout('replay') YIELD name
RETURN name
```

两次都返回 `[['replay']]`。此时 active Branch 为 replay，而 main 仍含教学节点；计算并应用差异：

<!-- example: vp-patch-graph -->
```cypher
CALL lithograph.diff('tag/baseline', 'branch/main') YIELD patch
CALL lithograph.patch.apply(patch) YIELD commit
RETURN size(patch.operations) AS operations,
       commit STARTS WITH 'commit/' AS valid
```

返回 `[[3, true]]`，三个 operation 分别新增 Node、Label 和 Property。验证 replay 的 graph：

<!-- example: vp-patch-graph-verify -->
```cypher
MATCH (n:VersionDoc) RETURN n.name AS name
```

返回 `[['Alice']]`。这个 replay 在同一 database identity space 中完成，main 与 baseline Tag 均保持原 target。

<a id="rebase"></a>

<a id="proc-lithograph-rebase"></a>

## `lithograph.rebase`

```text
CALL lithograph.rebase(onto [, options])
```

Mode：`WRITE`。`onto` 为 Descriptor，目标为 `options.branch` 或 active Branch。这里 procedure 参数 `options` 与 SQL execution options 是不同对象：procedure options 只支持 `resolutions :: LIST<MAP>`。可以省略整个参数；提供时必须包含 `resolutions`，所以 `{}`、null、未知 key 都为 `INVALID_ARGUMENT`。最小显式值为 `{resolutions: []}`。不接受 execution author/message。

沿目标 HEAD 的 first-parent chain 选择距 HEAD 最近、且为 onto ancestor 的 replay boundary，将 boundary exclusive 到旧 HEAD 的 Commit 按旧到新逐个重放。每个新 Commit 保留其旧 author/message、采用新 committedAt；Merge Commit 重放相对 first parent 的变化，second parent topology 被 flatten。Tag 和旧 Data 不随之移动/复制。

| 返回列 | 类型 / 含义 |
| --- | --- |
| `status` | STRING：`up_to_date`、`rebased`、`conflicted` |
| `commit` | 成功时 STRING resolved 目标 head；冲突时 null |
| `rewritten` | LIST&lt;MAP&gt;，按 replay 顺序的 `{from: <old commit>, to: <new commit>}`；非 rebased 时为 `[]` |
| `conflicts` | LIST&lt;MAP&gt;，冲突时含 conflictId/slot/base/ours/theirs/sourceCommit；成功为 `[]` |

无须 replay 时 `status=up_to_date`，但如果旧 HEAD 是 onto 的 ancestor，**仍会将目标 Branch fast-forward 到 onto**。不要用 status 推断 ref 没有变化。

所有 replay 处于同一原子边界；冲突返回普通成功 rows 中的 `status=conflicted, commit=null, rewritten=[]`，graph/Branch/已暂建新 Commit 全部 rollback。失败与返回冲突都不产生“半完成 rebase”。resolutions 使用 [共享 resolution shape](../versioning.md#resolution)；ours 是当前 replay state，theirs 是待 replay sourceCommit。unknown/duplicate conflictId、非法 choice/value 返回 `INVALID_ARGUMENT`。同一 onto、source sequence 和之前 resolution 选择下，冲突 ID 跨重试稳定。

<!-- example: vp-rebase -->
```cypher
CALL lithograph.rebase('branch/main', {resolutions: []})
YIELD status, rewritten, conflicts
RETURN status, size(rewritten) AS rewritten, size(conflicts) AS conflicts
```

结果为 `[['up_to_date', 0, 0]]`。以下显式空 options 不合法：

<!-- example: vp-rebase-options-error -->
```cypher
CALL lithograph.rebase('branch/main', {})
```

预期错误：`INVALID_ARGUMENT`。

<a id="squash"></a>

<a id="proc-lithograph-squash"></a>

## `lithograph.squash`

```text
CALL lithograph.squash(since)
```

Mode：`WRITE`。`since` 必须是目标 HEAD 的 ancestor 且不等于 HEAD；相等或非祖先为 `INVALID_ARGUMENT`。目标为 `options.branch` 或 active Branch。创建 parent=`since` 的新 single-parent Commit，其 Snapshot 与旧 HEAD 完全相同，并原子移动目标 Branch。返回一行 `from :: STRING, previousHead :: STRING, commit :: STRING`，其中 from 是 resolved since。

新 metadata 取 execution author/message，省略为 null。旧 Commit immutable、Data 仍绑旧 Commit，Tag 不移动，新 Commit 默认无 Data。操作前保存原 head 的 Tag，可在 GC 前用 reset 恢复。

以下示例先创建基准 Tag，再写入一条教学记录，最后 squash：

<a id="version-example-write"></a>

<!-- example: vp-write -->
```cypher
CREATE (n:VersionDoc {name: 'Alice'}) RETURN n.name AS name
```

在 [创建 baseline Tag 的示例](#tag-create) 与上述写入完成后：

<!-- example: vp-squash -->
```cypher
CALL lithograph.squash('tag/baseline') YIELD from, previousHead, commit
RETURN from <> previousHead AS hadHistory,
       commit <> previousHead AS replaced
```

结果为 `[[true, true]]`；查询仍可读取 `VersionDoc {name:'Alice'}`。

<a id="reset"></a>

<a id="proc-lithograph-reset"></a>

## `lithograph.reset`

```text
CALL lithograph.reset(target)
```

Mode：`WRITE`。`target` 为任意已有 Descriptor。原子将目标 Branch ref 移到 resolved Commit，返回一行 `from :: STRING, to :: STRING`。不创建 Commit、不删除历史、不修改 Data/Tag；不接受 author/message。可回退，也可前移或移到另一条历史线。

在基准 Tag 后写入教学记录，再 reset：

<!-- example: vp-reset -->
```cypher
CALL lithograph.reset('tag/baseline') YIELD from, to
RETURN from <> to AS moved
```

结果为 `[[true]]`；基准为 Root 时，当前图恢复为空。原 head 若没有其它保护引用，后续 GC 可删除它；恢复需要事先保存其 Tag 或其它 Branch。

<a id="revert"></a>

<a id="proc-lithograph-revert"></a>

## `lithograph.revert`

```text
CALL lithograph.revert(commit [, options])
```

Mode：`WRITE`。第一个参数必须是 `commit/<id>`，不接受 Branch/Tag Descriptor。普通 Commit 固定相对 first parent 取 inverse patch，且**不能提供第二个参数**，包括 `{}`。Merge Commit 必须传 `{mainline: 1}` 或 `{mainline: 2}` 指定相对哪个 parent 撤销；未知 key、错误值、Root Commit 均为 `INVALID_ARGUMENT`。

把 inverse patch 应用到 `options.branch` 或 active Branch 的当前 head，通过 before conditions 与最终约束/Provider 校验后，创建新 single-parent Commit 并移动该 Branch，返回一行 `commit :: STRING`。支持 execution author/message。它不会删除被撤销 Commit、不改写现有 Data/Tag，新 Commit 默认无 Data；当前状态不再满足 inverse patch 的 before conditions 时整体失败。

先执行上面的 [写入教学记录的示例](#version-example-write)，再读取当前 immutable identity 并撤销：

<!-- example: vp-revert -->
```cypher
CALL lithograph.commit.get('branch/main') YIELD commit
CALL lithograph.revert(commit) YIELD commit AS reverted
RETURN reverted STARTS WITH 'commit/' AS valid
```

结果为 `[[true]]`；原写入 Commit 与新的撤销 Commit 都保留在历史中。以下传入可移动引用的形式不合法：

<!-- example: vp-revert-descriptor-error -->
```cypher
CALL lithograph.revert('branch/main')
```

预期错误：`INVALID_ARGUMENT`。

相关主题：[版本工作流](../../guide/versioning.md) · [Merge 工作流](../../guide/merge.md) · [执行 options](../execution-options.md) · [错误参考](../errors.md)。
