# 版本管理

适用 v0.1.0。本页先用独立演示库解释日常操作，再介绍会改写 Branch 历史的操作。完整可执行流程见 [version_workflow.py](examples/version_workflow.py)；合并冲突独立见 [Merge Session](merge.md)。

## Commit、Branch、Tag 与 Descriptor

Commit 保存 immutable 图、Schema 和 Index definition；Branch 是随写入前进的引用。Tag 是由调用方显式移动的命名引用，不随普通写入前进。它们全部属于**同一个 SQLite database**，不是网络 Git repository。

| 输入位置 | 写法 |
| --- | --- |
| Query `options.branch` | `main`、`feature/review`，只有 Branch name |
| 接受版本的 procedure 参数、`options.at` | `branch/main`、`tag/baseline`、`commit/<64-hex-id>` |
| `expectedHead` | 只接受已解析的 `commit/<64-hex-id>` |

Branch / Tag 会移动，Commit Descriptor 不会。一次操作开始时解析并固定版本；要让多次读取使用同一 Snapshot，先获取 Commit，再让后续读取全部使用它。

## 创建分支并读写

```sql
SELECT lithograph_init();
SELECT lithograph('CREATE (:Setting {name:''theme'', value:''light''}) FINISH');
SELECT lithograph('CALL lithograph.tag.create(''baseline'', ''branch/main'')');
SELECT lithograph('CALL lithograph.branch.create(''feature'', ''tag/baseline'')');
SELECT lithograph(
  'MATCH (s:Setting) SET s.value = ''dark'' RETURN s.value AS value',
  '{}', '{"branch":"feature","message":"Try dark theme"}'
);
SELECT lithograph('MATCH (s:Setting) RETURN s.value AS value');
SELECT lithograph('MATCH (s:Setting) RETURN s.value AS value', '{}', '{"branch":"feature"}');
SELECT lithograph('CALL lithograph.branch.list()');
```

`main` 是 `light`，`feature` 是 `dark`。Query-level Branch 不改变 connection 默认的 `main`。同一数据库不同连接可以执行不同分支，但写入仍共享 SQLite 单文件 writer。

**v0.1.0–v0.2.0 已知限制：**公开 inventory 包含 `lithograph.branch.checkout(name)`，但 SQL Bridge 与普通 Native execute 实际执行都会返回 `TRANSACTION_BOUNDARY_REQUIRED`。使用已验证的 `options.branch`；不要先 BEGIN、不要改内部表，也不要把这个错误解释成分支数据丢失。跟踪细节见 [已知问题](../reference/known-issues.md)。

## 查看历史和时间旅行

```sql
SELECT lithograph('CALL lithograph.commit.get(''branch/feature'')');
SELECT lithograph('CALL lithograph.log(''branch/feature'', 10)');
SELECT lithograph(
  'MATCH (s:Setting) RETURN s.value AS value', '{}', '{"at":"tag/baseline"}'
);
```

`commit.get` 返回 immutable metadata 与**当前** Commit Data；Commit Data 不会因为查看历史 Snapshot 而回到它以前的注释值。`at` 查询始终只读，包括 `at: "branch/main"`。从历史继续写入应先创建 Branch。

`log` 默认每页 100，limit 必须为正整数。结果遍历可达 Commit DAG，不只是 first-parent 链；每行 cursor 都可从该行之后继续。保存固定起点和最后一行的非空 cursor：

```python
start = one(db, "CALL lithograph.commit.get('branch/main')")["commit"]
page = query(db, "CALL lithograph.log($start,100)", {"start": start})
# 按 columns 的位置取最后一行 cursor；若没有行或 cursor 为 null 则结束。
cursor = page["rows"][-1][page["columns"].index("cursor")] if page["rows"] else None
if cursor is not None:
    page = query(db, "CALL lithograph.log($start,100,$cursor)",
                 {"start": start, "cursor": cursor})
```

上面使用 [示例 helper](examples/version_workflow.py) 的 `one` / `query` 和已经打开的 `db`。不要解析 cursor，也不要换一个起点后复用它。完整分页循环在同一示例文件中。

## Commit Data 与显式标记

```sql
SELECT lithograph(
  'CALL lithograph.commit.data.set(''branch/feature'', $data)',
  '{"data":{"review":"accepted","ticket":42}}'
);
SELECT lithograph('CALL lithograph.commit.get(''branch/feature'')');
SELECT lithograph('CALL lithograph.commit.data.clear(''branch/feature'')');
SELECT lithograph(
  'CALL lithograph.commit.create($data)',
  '{"data":"Manual milestone"}',
  '{"branch":"feature","author":"example-app","message":"Review milestone"}'
);
```

Commit Data 可为任意 JSON，包括 `null`；`hasData` 区分“没有 Data”和“Data 显式为 null”。set / clear 不创建 Commit，不移动引用，也不进入 Diff、Merge、Rebase 或 Squash。需要不可变、可版本化的业务事实时，把它保存为图数据。

`commit.create` 主动创建 empty-delta Commit，不是提交未保存数据。author/message 在 Commit 创建后不可修改；不要用它们替代需要后续修改的 Data。

## Diff 与 Patch

```sql
SELECT lithograph('CALL lithograph.diff(''tag/baseline'', ''branch/feature'')');
```

结果 `patch` 描述图、Schema、Index 的变化，包含 `format`、`databaseId`、`from`、`to`、`operations`。它不包含 Branch/Tag 引用变化和 Commit Data。

应用取得返回的 `patch` Map 后，原样作为参数传给另一个 Branch：

```python
patch = one(db, "CALL lithograph.diff($before,$after)",
            {"before": "tag/baseline", "after": "branch/feature"})["patch"]
query(db, "CALL lithograph.branch.create('replay','tag/baseline')")
applied = one(db, "CALL lithograph.patch.apply($patch)",
              {"patch": patch}, {"branch": "replay"})
```

只有同一 `databaseId` 的 Patch 可应用。每个 before condition 都必须满足，整体成功产生一个 Commit，否则不产生部分更改。`from/to` 是 provenance，不代表直接把 Branch ref 移到 `to`，也不表示要求当前 head 必须等于 `from`。保留 tagged values，不做有损 JSON 类型转换。

## Rebase、Squash、Reset、Revert 的选择

这些操作会改变目标 Branch 观察到的历史。先为旧 head 建立保留 Tag，并对持久库做一致性备份；在独立 Branch 演练，不盲目重试。

| 目标 | Procedure | 历史效果 |
| --- | --- | --- |
| 把本分支提交重新应用到新基线 | `lithograph.rebase(onto[,options])` | 创建新 Commit，返回 `rewritten` 的 old→new 映射；按 first-parent replay，merge 的第二 parent 拓扑被压平 |
| 把一段历史压成一个 Commit | `lithograph.squash(since)` | 新 Commit parent 为 since，最终 Snapshot 与之前 HEAD 一致；since 必须为祖先且不能等于 HEAD |
| 让 Branch 回到已有状态 | `lithograph.reset(target)` | 只移动 Branch，不创建 Commit，不删除旧 Commit |
| 撤销某个 Commit 的影响并保留新历史记录 | `lithograph.revert(commit[,options])` | 应用 inverse patch，产生新 Commit；可能因当前状态不再适用而失败 |

调用形式都是正常 Cypher procedure，不是新的 query grammar：

```cypher
CALL lithograph.rebase($onto, {resolutions:$resolutions})
CALL lithograph.squash($since)
CALL lithograph.reset($target)
CALL lithograph.revert($commit)
```

以上四行是四次**独立调用**的语法示例，不是可一次执行的脚本。通过 execution `options.branch` 明确目标分支。

Rebase 全部成功才移动 Branch。冲突返回 `status: conflicted`、`commit: null` 和冲突内容；为相同输入提供 `options.resolutions` 后重试。它不创建 Merge Session。不要复用不同输入下的 conflictId。成功的新 Commit 默认保留原 author/message，但不自动复制 Commit Data，不移动 Tag。

Revert 普通 Commit 不传第二个参数；Root 不能 revert。Revert Merge Commit 必须指定 `{mainline:1}` 或 `{mainline:2}`，明确以哪个 parent 为原始基线。

Reset / Squash / Rebase 后旧 Commit 不被即时删除；只要还被 Branch、Tag 或 open Merge Session 保护，就不会被 GC。删除最后一个引用后再 GC 可能永久删除它们，不能依赖“我还保存着 Commit 字符串”阻止回收。

## 引用维护与范围

`tag.move` 显式更改 Tag；`tag.delete` / `branch.delete` 删除引用但不立刻删除 Commit。`main` 不可删除；不能删除 connection 当前 active Branch。名称按大小写区分，具体限制见 [Limits](../reference/limits.md)。

没有远程 clone / fetch / push / pull API。完整数据库备份可以复制整个 repository，但不能把两个独立数据库的 identity space 用版本 API 隐式合并。Procedure 的参数、结果列和限制见 [Reference](../reference/procedures.md)。
