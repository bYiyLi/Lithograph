# Procedure Reference

**版本：v0.1.0。** 所有 33 个公开签名、mode 和输出列见 [Inventory](procedure-inventory.md)。本页补充参数语义、默认值、Commit 效果及限制；名称不省略 `lithograph.` / `db.` 前缀。

Procedure 通过 Cypher `CALL name(...)` 调用，返回行可接 `YIELD` / `RETURN`。本页的方括号表示可选位置参数，不是实际调用字符。可选参数通常通过省略提供默认值，不应把 null 当作任意可选参数的默认值。

## 图元信息与全文检索

| Procedure | 参数和行为 |
| --- | --- |
| `db.labels()` | 当前 Snapshot / Graph View 中实际存在的 Label，返回 `label` |
| `db.propertyKeys()` | 当前图可见 Property key，返回 `propertyKey` |
| `db.relationshipTypes()` | 当前图可见 Relationship Type，返回 `relationshipType` |
| `db.index.fulltext.queryNodes(indexName,queryString[,options])` | 全文节点索引；返回 `node,score` |
| `db.index.fulltext.queryRelationships(indexName,queryString[,options])` | 全文关系索引；返回 `relationship,score` |

全文 options 默认 `{}`，支持 `skip`、`limit`、`analyzer`；skip 非负、limit 为合法非负整数，省略 limit 不额外施加该限制。`analyzer` 是当前 SQLite connection 可构造的完整 FTS5 tokenizer specification；省略时采用 IndexDefinition 的 specification。它只改变本次查询文本的分词，不改变 IndexDefinition 或已索引文档。名称必须对应当前选定版本的正确索引 target family。

默认 tokenizer 为 `unicode61`；Porter stemming 使用 `porter unicode61`。`standard-no-stop-words` / `english` 不再有 Lithograph 特殊映射。第三方 tokenizer 通过宿主 SQLite extension 在每个实际 connection 上注册，不是 Neo4j/Lucene analyzer plugin。score 由检索语义定义，非概率；业务分页与 top-k 应显式限制结果量。例子见 [Search](../guide/search.md)。

## Branch 与 Tag

| Procedure | 参数语义与结果 |
| --- | --- |
| `lithograph.branch.create(name[,from])` | from 为 Descriptor，省略从 active Branch head 创建；返回 name,commit；不切换 active Branch |
| `lithograph.branch.list()` | 返回 name,commit,active，按名称排序 |
| `lithograph.branch.delete(name)` | 返回 name,previousCommit；不能删除 main 或 connection active Branch |
| `lithograph.branch.checkout(name)` | 设计上改变 connection active Branch，要求 autocommit；**v0.1.0 SQL / Native adapter 均有已知执行缺陷，使用 query options.branch** |
| `lithograph.tag.create(name,target)` | target 为 Descriptor，返回 name,commit |
| `lithograph.tag.list()` | 返回 name,commit |
| `lithograph.tag.move(name,target)` | 显式移动已有 Tag，返回 name,previousCommit,commit |
| `lithograph.tag.delete(name)` | 返回 name,previousCommit |

这些操作不为引用变化创建 graph Commit。Branch 与 Tag 名称各有 namespace。name 不是 Descriptor：传 `release` 与传 `tag/release` 含义不同。参数已有目标的操作不再使用 query branch override。

## Commit 与可修改 Data

| Procedure | 参数语义与结果 |
| --- | --- |
| `lithograph.commit.get(version)` | 返回 commit,parents,author,message,committedAt,hasData,data；version 先解析成 Commit |
| `lithograph.commit.create([data])` | 在目标 Branch 创建 empty-delta Commit；返回 commit；可附任意 JSON Data |
| `lithograph.commit.data.set(version,data)` | 设置/替换整个 Data，而不是 merge JSON；返回 commit,data |
| `lithograph.commit.data.clear(version)` | 删除 Data，返回 commit；不改变 Commit identity |

Data 参数是任意 JSON 值，包含 null；`hasData:false` 与 `hasData:true,data:null` 不等价。Data 不进入 diff/merge/rewrite，不能作为 immutable audit 事实。author/message/committedAt 是 Commit 自身的 immutable metadata。`committedAt` 是 INTEGER，单位为 UNIX epoch 微秒，不是 seconds；Session list 的 `createdAt` 使用同一单位。

## History 与 Structural Patch

`lithograph.log([version[,limit[,cursor]]])`：version 默认 active Branch head，limit 默认 100。返回 commit,parents,author,message,committedAt,cursor。遍历 pinned 起点可达 DAG；把最后一行非空 cursor 用于下一页，保持原起点不变。cursor 为 opaque token，不接受自行拼接的 offset。

`lithograph.diff(before,after)`：两个参数均为 Descriptor，返回一个 `patch` Map：

```text
{format:1, databaseId:<uuid>, from:<commit descriptor>, to:<commit descriptor>, operations:[...]}
```

operations 使用稳定的逻辑槽位描述 Node / Label / Relationship / Property / Schema / Index 变化。Family 为 AddNode、DeleteNode、AddLabel、RemoveLabel、AddRelationship、DeleteRelationship、SetProperty、RemoveProperty、SetSchema、CreateIndex、DropIndex、SetIndex；以实际返回的字段和值类型为准，不把程序内存地址或物理 SQLite row 作为 Patch。

`lithograph.patch.apply(patch)`：接收 Diff 返回的完整 Map，经 `$patch` 绑定并保留 tagged value 编码。仅适用于同 databaseId；校验所有 before condition，原子写入一个新 Commit，返回 commit。from/to 记录 provenance，不直接决定目标 Branch 的移动方式；重复 slot、非法类型或条件不匹配不能当作成功忽略。

推荐从 Diff 得到可用 Patch，不手写内部对象 hash。完整 round-trip 见 [版本工作流](../guide/examples/version_workflow.py)。Patch 不是 Cypher mutation 的替代语法，也不含 Tag、Branch 或 Commit Data。

## Merge Session

| Procedure | 必需参数与默认 | 作用 |
| --- | --- | --- |
| `merge.start` | source Descriptor；可选 expectedHead Commit Descriptor | 用 query branch 选择 target，建立 Session；不移动 Branch |
| `merge.get` | session id | 读取 session,targetBranch,ours,theirs,revision,status,unresolved |
| `merge.list` | 可选正 limit 和 cursor；默认 100 | 分页列出未结束 Session，返回 createdAt / cursor 等列 |
| `merge.conflicts` | session；可选正 limit 和 cursor；默认 100 | 当前 revision 的 conflict inventory |
| `merge.resolve` | session,expectedRevision,resolutions | 原子应用一批选择，返回更新后的 Session 状态 |
| `merge.finalize` | session,expectedRevision | revision + target-head CAS；返回 status,commit，成功移除 Session |
| `merge.abort` | session,expectedRevision | 移除 Session，返回 session，无图 Commit |

表内均需 `lithograph.` 前缀。Session id 是引擎返回的 `merge-session/...` 字符串，revision 是正整数。resolutions 是 Map 列表，每项 `{conflictId,choice}`；choice 为 `ours` / `theirs` / `value`，最后一种还需要类型正确的 `value`。

start / get / list / conflicts / resolve / abort 不接受 author/message；finalize 可以使用 author/message 作为新合并 Commit metadata，但不能重新指定 branch。除 start 外的 Session 操作不能覆盖已经固定的目标。

start 的 status：up_to_date、fast_forward、conflicted、ready；finalize 成功：up_to_date、fast_forward、merged。冲突未解决不能 finalize；head 或 revision 变化时失败并保留 Session。candidate query 不是 Procedure，而是普通只读 query + `options.mergeSession`。详见 [Merge 指南](../guide/merge.md)。

## 历史改写与撤销

| Procedure | 行为和注意事项 |
| --- | --- |
| `lithograph.rebase(onto[,options])` | onto 为 Descriptor；procedure options 仅包含 resolutions。返回 status,commit,rewritten,conflicts；conflicted 时 commit 为 null，整个操作未应用。重写以 first-parent sequence 为准，新 Commit 保留各自原 author/message，不复制 Data/Tag |
| `lithograph.squash(since)` | since 为目标 HEAD 的祖先且不等于 HEAD；返回 from,previousHead,commit；最终 Snapshot 不变，新 parent=since |
| `lithograph.reset(target)` | target 为 Descriptor；返回 from,to，只移动目标 Branch，不创建 Commit |
| `lithograph.revert(commit[,options])` | commit 必须是 `commit/<id>`；普通 Commit 省略 options；Merge Commit 必须 `{mainline:1}` 或 `{mainline:2}`；Root 不允许 |

Rebase resolutions 使用 conflicts 返回的 conflictId，并根据槽位选择 ours/theirs/value。冲突返回还标识 replay 的 sourceCommit，不要把它当作 Merge Session 的 conflict inventory。普通成功、up-to-date、fast-forward 情况按返回 status 区分，不仅看有没有 SQL 异常。

操作前保留原 head 的 Tag，检查影响面。旧 Commit 只有在失去所有保护引用并执行 GC 后才可能被删除；Commit 字符串自身不是 GC root。

## 返回值类型与 introspection 注意事项

v0.1.0 的 `SHOW PROCEDURES.returnDescription[*].type` 把所有输出写成 STRING，`argumentDescription` 也未提供实际参数清单；见 [DOC-V010-03](known-issues.md)。不要把这些字段用于自动转换结果，或据空 argumentDescription 判断 procedure 没有参数。

| 字段 / 对象 | 实际值类型 |
| --- | --- |
| 普通 name、commit、previousCommit、descriptor、status、cursor、session | STRING；不存在的 cursor / nullable结果可能为 null |
| `branch.list.active`、`commit.get.hasData` | BOOLEAN |
| `commit.get.parents`、`log.parents` | LIST<STRING> |
| author、message | STRING 或 null |
| committedAt、createdAt | INTEGER，UNIX epoch 微秒 |
| revision、unresolved、indexedEntities、GC 删除计数 | INTEGER |
| `commit.get.data`、`commit.data.set.data` | 任意 JSON value，可能为 null |
| `diff.patch` | MAP，operations 为 LIST<MAP> |
| Full-text node / relationship、score | 分别为 NODE / RELATIONSHIP、FLOAT |
| conflict.slot / base / ours / theirs / resolution | 由具体冲突槽位决定的 typed value / Map / null；不要 stringify 后丢失结构 |
| `rebase.rewritten`、`rebase.conflicts` | LIST<MAP> |

说明中的 UNION/null 属于字段语义，不代表所有 procedure 的同名字段都可以随意换类型。JSON 跨语言编码继续遵守 [Values](values.md)，例如大 INTEGER 使用 tagged wrapper。

## 索引维护与 GC

`lithograph.index.rebuild(name,version)`：两个参数均为非空字符串，version 是明确 Descriptor。仅支持持久 Standard Index 的 RANGE/TEXT/POINT / Relationship LOOKUP family，不支持 Node LOOKUP、FULLTEXT、VECTOR。返回 `name,commit,indexedEntities`；commit 是实际 anchor，不创建新 Commit、不移动 Branch。

rebuild 在普通 scalar / Native 上独立执行，可接只读结果投影，但不与其他 mutation/maintenance 合并。rows adapter 禁止，Native explicit transaction / transaction-owning subquery / candidate 中禁止，不接受 at 或不适用 execution options。全量重建可能长时间持有 writer，应安排维护窗口。

`lithograph.gc()`：无参数，显式删除所有 Branch、Tag、open Merge Session 都不可达的历史和相关 derived data。返回 `commits,layers,schemas,checkpoints,commitData` 的删除数量。不会以“压缩数据库文件”为理由忽略可达历史；文件物理空间处理仍属于 SQLite 运维。

GC 是不可逆历史回收，不能通过 reset 恢复已删除 Commit。先建立保留策略与一致性备份，再执行。详情见 [维护](../guide/operations.md)。

依据：[公开注册表](../../crates/lithograph-core/src/query/registry.rs)、[版本执行](../../crates/lithograph-core/src/query/version/mod.rs)、[设计](../design.md)。
