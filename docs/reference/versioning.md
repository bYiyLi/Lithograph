# 版本对象与数据格式

[产品手册](../guide/README.md) · [接口参考](README.md) · [版本 Procedure](procedures/versioning.md) · [Merge Procedure](procedures/merge.md)

适用版本：**Lithograph v0.3.3**，storage format `3`。本页定义公开 Version Descriptor、Commit metadata、Commit Data、History cursor、Patch、Merge Session/conflict/resolution 和 rebase mapping 的字段。调用参数、状态变化与错误分别由 [Version Procedure](procedures/versioning.md) 和 [Merge Procedure](procedures/merge.md) 说明。

代码框中的 `<...>` 是格式说明，不是可以直接传入的字面值。API 返回的是 typed Cypher value；通过 SQLite JSON surface 传输时继续使用 [Values 编码](values.md)，不能把所有字段转成字符串。

<a id="version-descriptor"></a>
<a id="descriptors"></a>

## Version Descriptor

| 形式 | 解释 |
| --- | --- |
| `branch/<name>` | operation 开始时解析命名 Branch 的当前 head，之后 pin 住该 Commit |
| `tag/<name>` | operation 开始时解析 Tag 当前 target，之后 pin 住该 Commit |
| `commit/<id>` | immutable Commit；id 为完整 64 个 hexadecimal characters |

Branch/Tag 在 operation 之间可以改变；同一个 `branch/main` 不保证两次调用读取同一 Snapshot。API 返回 resolved identity 时统一为 `commit/<id>`。普通 query 用 `options.at` 可以读取 pinned 历史 Snapshot，但不能修改；要从历史继续写入，先创建 Branch。

不能传裸 `main`、缩短的 Commit ID、Merge Session id 或 history cursor 作为 Descriptor。Descriptor 不带 databaseId，只在当前 SQLite database 内解释；Version API 不提供跨 database 的 clone/push/merge。

<a id="reference-names"></a>

## Branch 与 Tag 名称

两者位于独立 namespace，因此同名 Branch 和 Tag 可以共存。调用 `branch.create('work')` 的 name 是 `work`，Descriptor 才是 `branch/work`。name 区分大小写，按 UTF-8 bytes 校验和排序：

- 长度 `1..255` bytes；多字节 Unicode 字符按字节计数。
- 禁止 NUL 与 ASCII control characters。
- 禁止开头/结尾 `/`、空 path segment、`.` / `..` segment。
- 允许 `/` 构成层级名称，例如 `feature/import`；该层级没有独立目录对象。
- `main` 是初始化创建的保留 Branch，不能删除或重命名；Tag namespace 没有对应保留规则。

Branch list row 为 `{name: STRING, commit: STRING, active: BOOLEAN}`；Tag list row 为 `{name: STRING, commit: STRING}`。其它 Procedure 的 `previousCommit` 是 mutation 前 resolved target。裸名称与 Descriptor 均按原样保存/解释，不执行大小写折叠。

<a id="commit"></a>

## Commit metadata

`lithograph.commit.get` 返回下列七列；它不是一个另外定义的嵌套 Commit Map：

| 字段 | 类型 | 语义 |
| --- | --- | --- |
| `commit` | STRING | resolved `commit/<64-hex-id>` |
| `parents` | LIST&lt;STRING&gt; | Root 为 `[]`，普通 Commit 为 `[parent1]`，Merge Commit 为 `[parent1,parent2]` |
| `author` | STRING 或 null | 创建时的 execution author；immutable |
| `message` | STRING 或 null | 创建时的 execution message；immutable |
| `committedAt` | INTEGER | UNIX epoch **微秒**；immutable，不是秒或毫秒 |
| `hasData` | BOOLEAN | 是否存在 Data sidecar |
| `data` | JSON-compatible value | 当前 Data，或没有 sidecar 时的 null |

Graph、Schema、Index definition 与 Commit parents/metadata 共同属于 immutable history；Branch head 可变。Derived index/checkpoint 不是 Commit 的独立业务状态。Committed Node/Relationship identity 不复用；`elementId(n)` 为 `n:<positive id>`，relationship 为 `r:<positive id>`，它们用于同一 database 内 Patch/冲突定位，不是跨 database 全局对象 ID。

Merge Commit 的 first parent 是 target `ours`，second parent 是 source `theirs`，其 graph delta 相对 first parent 表达。若只需 Snapshot，相同 graph 内容并不要求相同 Commit identity。

<a id="commit-data"></a>

## Commit Data

Commit 最多有一份 mutable JSON annotation。允许 null、Boolean、Integer、有限 Float、String、List、Map，容器内部递归遵守同一规则。不直接接受 Node/Relationship/Path、Temporal/Duration、Point、Vector、UUID 原生 Cypher 值或 NaN/Infinity。需要这些业务内容时，显式转换为 JSON-compatible 应用格式，或写进 versioned graph。

| `hasData` | `data` | 含义 |
| --- | --- | --- |
| false | null | 未设置，或已经 clear |
| true | null | 明确设置 JSON null |
| true | 任意其它合法 JSON value | 存在该 annotation |

`data.set` 替换整个 JSON 值；`data.clear` 删除 sidecar。两者都不创建 Commit、不改 Snapshot、不移动 ref。Data 不进入 Diff/Patch/Merge，不自动复制到 Rebase/Squash/Revert 新 Commit，也不随 Tag 移动。被 GC 回收的 Commit，其 Data 一并删除。读取相同 immutable Commit 的 Data 可能观察到后续修改，因此不能把它当作 immutable audit 内容。

<a id="history-cursor"></a>

## History 与 cursor

`log` 每行返回 `{commit, parents, author, message, committedAt, cursor}`，前五项类型同上。`cursor` 为 opaque STRING 或 null，每行指向从该行之后继续的 traversal state；终点 null。分页默认 limit `100`，合法值为正 Integer。

第一次 operation pin 住起始 Commit，并遍历其可达 DAG：child 总在 parent 前，同时可返回者按 committedAt 降序、Commit ID 升序。不要把结果当成 first-parent chain 或按时间全局排序的事件表。

下一页继续传原 resolved 起点与最后一行 cursor。Branch/Tag 后续移动不改变已开始 traversal；可以调整 limit。不要解析、修改、跨 traversal 拼接 cursor，也不要以它作为 Descriptor。它包含 traversal 信息而不是永久数据库资源；保留 cursor 不阻止 GC。需要跨多次调用长期保留历史时，创建 Tag 保护起点。回收后读取失效 cursor 可以返回 `VERSION_NOT_FOUND`；非法 token 为 `INVALID_ARGUMENT`。

<a id="patch"></a>

## Structured Patch

`diff(before,after)` 返回一行 `patch :: MAP`：

```text
{
  format: 1,
  databaseId: "<database UUID>",
  from: "commit/<before id>",
  to: "commit/<after id>",
  operations: [<operation>, ...]
}
```

`format` 为 INTEGER `1`；databaseId、from、to 为 STRING；operations 为 LIST&lt;MAP&gt;。databaseId 必须等于当前 database。from/to 必须是完整 Commit Descriptor，记录 provenance；应用时不要求当前 Branch head 等于 from，真正前置条件为每个 operation 的 before。Diff 返回按 slot 确定性排序的 canonical operations，不包含 Commit Data、ref 或 Commit metadata。

每个 operation 都有 STRING `op`、STRING `slot`、typed `before`、typed `after`。同一个 Patch 每个 logical slot 最多一次，重复 slot 为 `INVALID_ARGUMENT`。不要用 DropIndex+CreateIndex 先后两步表达同名索引替换，应保留单个 SetIndex。

<a id="patch-operations"></a>

### Operation 封闭集合

以下 `<id>` 是正十进制 logical id，label/key/name 保留实际 Unicode 字符。slot 是引擎给出的稳定字符串，不是应自行按 `/` 切分的路径协议。

| op | slot | 其它必需字段 | before | after |
| --- | --- | --- | --- | --- |
| `AddNode` | `node/<id>` | `elementId:"n:<id>"` | null | true |
| `DeleteNode` | `node/<id>` | 同上 | true | null |
| `AddLabel` | `node/<id>/label/<label>` | `elementId:"n:<id>", label:STRING` | false | true |
| `RemoveLabel` | 同上 | 同上 | true | false |
| `AddRelationship` | `relationship/<id>` | `elementId:"r:<id>"` | null | relationship state Map |
| `DeleteRelationship` | 同上 | 同上 | relationship state Map | null |
| `SetProperty` | `node/<id>/property/<key>` 或 `relationship/<id>/property/<key>` | `owner:"node/<id>"` 或 `"relationship/<id>"`, `key:STRING` | property state Map | `{present:true,value:<property value>}` |
| `RemoveProperty` | 同上 | 同上 | property state Map | `{present:false}` |
| `SetSchema` | `schema` | 无 | schema core Map | schema core Map |
| `CreateIndex` | `index/<name>` | 无 | null | IndexDefinition Map |
| `DropIndex` | 同上 | 无 | IndexDefinition Map | null |
| `SetIndex` | 同上 | 无 | 旧 IndexDefinition Map | 新 IndexDefinition Map |

Relationship state 为 `{type: STRING, source:"n:<id>", target:"n:<id>"}`；不包含关系属性，属性由独立 operation 表达。Node/Relationship identity 必须已由本 database 分配，不能靠 Patch 任意造新 ID。`DETACH DELETE` 的 Diff 含显式 Relationship 和 Node 删除；最终完整性校验不允许悬空关系。

Property state 使用 presence wrapper，区别“不存在”与类型值；null 不作为已存储 Property 值：

```text
{present:false}
{present:true, value:42}
{present:true, value:<typed Temporal / Point / Vector / List ...>}
```

原生 typed value 经宿主 JSON surface 传输时保留 Values Reference 的 tagged encoding；property `before` / `after` 不得转成显示文本。`SetProperty` 可以表示新增或替换；`RemoveProperty` 的 after 必须 absent。

<a id="schema-core"></a>

### Schema core Map

`SetSchema` 用一个 aggregate `schema` slot 更新 Graph Type 和 Constraint；它不包含 indexes。其精确字段为 snake_case：

```text
{
  graph_nodes: {<label>: <GraphNodeType>, ...},
  graph_relationships: {<relationship type>: <GraphRelationshipType>, ...},
  constraints: {<constraint name>: <ConstraintDefinition>, ...}
}
```

| 对象 | 字段 |
| --- | --- |
| GraphNodeType | `label:STRING, implied_labels:LIST<STRING>, properties:MAP<property name,PropertyRule>` |
| GraphRelationshipType | `relationship_type:STRING, source_label:STRING?, source_identifying:BOOLEAN, target_label:STRING?, target_identifying:BOOLEAN, properties:MAP<property name,PropertyRule>` |
| PropertyRule | `property_type:PropertyType, required:BOOLEAN` |
| ConstraintDefinition | `name:STRING, target:SchemaTarget, properties:LIST<STRING>, kind:ConstraintKind, origin:STRING?` |

PropertyType 为带 `type` discriminator 的 Map：`{type:'any'|'boolean'|'integer'|'float'|'string'|'date'|'local_time'|'zoned_time'|'local_date_time'|'zoned_date_time'|'duration'|'point'|'uuid'}`；Vector 为 `{type:'vector',coordinate:STRING,dimension:INTEGER}`，List 为 `{type:'list',element:PropertyType}`，Union 为 `{type:'union',members:LIST<PropertyType>}`。coordinate 使用定义中的坐标类型名称。

SchemaTarget 为 `{target:'node',label:STRING}` 或 `{target:'relationship',relationship_type:STRING}`。ConstraintKind 为 `{kind:'key'|'unique'|'not_null'}` 或 `{kind:'type',rule:PropertyRule}`。`origin` 记录生成约束与 Graph Type 的关联；必须原样保留，不能改为业务描述。

<a id="index-definition"></a>

### IndexDefinition Map

Patch 和 Merge value 使用同一 versioned IndexDefinition 对象，而不是 `SHOW INDEXES` 的展示行：

| 字段 | 类型 / 编码 |
| --- | --- |
| `name` | STRING，必须与 `index/<name>` slot 一致 |
| `kind` | STRING：`lookup`, `range`, `text`, `point`, `full_text`, `vector`, `semantic` |
| `target` | 下面定义的 IndexTarget Map |
| `owning_constraint` | STRING 或 null，约束拥有的索引关联 |
| `labels_or_types` | 可选 LIST&lt;STRING&gt;，空值生成时省略 |
| `additional_properties` | 可选 LIST&lt;STRING&gt;，空值生成时省略 |
| `configuration` | 可选 IndexConfiguration Map，无配置时生成时省略 |

IndexTarget 形式：

```text
{target:'node_lookup'}
{target:'relationship_lookup'}
{target:'node_properties',label:STRING,properties:LIST<STRING>}
{target:'relationship_properties',relationship_type:STRING,properties:LIST<STRING>}
```

IndexConfiguration 形式：

| `kind` | 附加字段 |
| --- | --- |
| `full_text` | `analyzer:STRING, eventually_consistent:BOOLEAN` |
| `vector` | `dimensions:INTEGER?, similarity_function:STRING, quantization_type:STRING, default_search_expansion_factor:STRING, hnsw_m:INTEGER, hnsw_ef_construction:INTEGER` |
| `semantic` | `provider:STRING, provider_config:<JSON value>, dimensions:INTEGER, similarity_function:STRING` |

这些字段是 versioned definition 编码，包含实现规范化后的值，和 Cypher DDL `OPTIONS/indexConfig` 的外部 key 不完全相同。应通过 DDL/Procedure 创建定义，再保存 Diff 输出；构造/改动 definition 仍必须通过相应约束、tokenizer 或 Provider validation。

<a id="patch-atomicity"></a>

### Patch 的应用与失败

`patch.apply` 验证所有 before conditions 与最终图/Schema，整体生成一个新 Commit。字段和 slot 不一致、duplicate slot、before 不匹配、未分配 identity、cross-database、未知 op 或类型非法返回 `INVALID_ARGUMENT`；final graph / Constraint / Provider 校验保留相应错误。任何错误都不移动目标 Branch，也不留下部分 graph/Schema change。

Patch 中的 provenance、dictionary identity、constraint ownership 和 typed values 共同构成可重放结构。业务需要展示 diff 时可以派生自己的显示模型；重新应用时保留原完整 Map，不要把显示模型当作 Patch 发回。

<a id="merge-session"></a>

## Merge Session

Session id 形如 `merge-session/<engine-generated id>`，必须原样传回。它不是 Version Descriptor。Session 持久保存 targetBranch、pinned ours/theirs、resolution set 和单调 revision；跨 connection/process reopen 可恢复，并保护所 pin history 免受 GC。

`merge.start/get` 返回一行：

| 字段 | 类型 / 含义 |
| --- | --- |
| `session` | STRING，durable Session id |
| `targetBranch` | STRING，裸 Branch name |
| `ours` | STRING，pinned target head Commit Descriptor |
| `theirs` | STRING，pinned source Commit Descriptor |
| `revision` | 正 INTEGER，初始 `1`，resolution set 有效变化时递增 |
| `status` | STRING：`up_to_date`, `fast_forward`, `conflicted`, `ready` |
| `unresolved` | 非负 INTEGER，当前 revision 尚未解决的冲突数 |

`merge.list` 返回 `session,targetBranch,ours,theirs,revision,createdAt,cursor`；createdAt 是 UNIX epoch 微秒，cursor 为 opaque STRING 或 null。list 是调用期间当前存在的 operational inventory，按 Session id binary 升序，不 pin 跨多页集合；后续页可以受并发 start/abort/finalize 影响。

`merge.resolve` 返回 `session,revision,status,unresolved`；`merge.abort` 返回 `session`。`merge.finalize` 返回 `status,commit`：status 为 `up_to_date|fast_forward|merged`，commit 始终为非空 resolved identity。只有 finalize 可以发布 canonical history：up_to_date 返回 ours，fast_forward 移动 Branch 到 theirs，merged 创建 first-parent ours / second-parent theirs 的 Merge Commit。

Candidate 普通查询使用 execution option `mergeSession:{id:STRING,revision:INTEGER}`。它读取该精确 revision 的 logical Snapshot，不是 Commit，summary.commit 为 null；需持久引用时必须 finalize。revision stale、Session 消失、unresolved conflict 分别为 `MERGE_SESSION_CHANGED`、`MERGE_SESSION_NOT_FOUND`、`MERGE_CONFLICT`。

<a id="conflict"></a>

## Conflict row 与 logical slot

`merge.conflicts` 每行是 `session,revision,conflictId,slot,base,ours,theirs,resolution,cursor`。

- `conflictId`：64-hex STRING，由 pinned inputs、slot 与三方值确定；不是 sequential index。
- `slot`：STRING，最小冲突单位，见下表。
- `base/ours/theirs`：typed slot value 或 null；null 表示该侧 slot absent，不是字符串 `"null"`。
- `resolution`：null 表示当前尚未选择；否则为 normalized `{choice:'ours'|'theirs'}` 或 `{choice:'value',value:<typed value>}`，**不重复包含 conflictId**。
- `cursor`：STRING 或 null，绑定 Session + revision；resolution 改变后旧 conflict cursor 返回 `MERGE_SESSION_CHANGED`。

| logical slot | 非空 value |
| --- | --- |
| `node/<id>` | true |
| `node/<id>/label/<label>` | true；不存在为 null，与 Patch label false 编码不同 |
| `relationship/<id>` | `{type:STRING,source:'n:<id>',target:'n:<id>'}` |
| `node/<id>/property/<key>`、`relationship/<id>/property/<key>` | 直接 typed Property value；**没有 Patch 的 `{present,value}` wrapper** |
| `graph/node/<label>` | GraphNodeType Map |
| `graph/relationship/<type>` | GraphRelationshipType Map |
| `constraint/<name>` | ConstraintDefinition Map |
| `index/<name>` | IndexDefinition Map |

Schema/Index value 形式见上面的编码表。Merge 的 Schema 冲突采用各 canonical object slot，而 Patch 的 SetSchema 使用 aggregate `schema` slot；两者不可直接替换。

conflicts inventory 同时包含已解决和未解决项，按 slot UTF-8 bytes、conflictId bytes 升序。delete-vs-modify、关系端点依赖和最终 Constraint violation 都可能形成 conflict。修改 resolution 后旧 conflict 可能消失，新的依赖/约束 conflict 可能出现；已经保存且暂时不再出现的 dormant resolution 不计 unresolved，若相同 pinned inputs 的同一 conflictId 重新出现则会重新应用。

<a id="resolution"></a>

## Resolution 输入

`merge.resolve` 和 `rebase.options.resolutions` 接受 List，每项必须标明 conflictId：

```text
[
  {conflictId:'<64-hex>',choice:'ours'},
  {conflictId:'<64-hex>',choice:'theirs'},
  {conflictId:'<64-hex>',choice:'value',value:<typed slot value>}
]
```

ours/theirs 不得携带 value；value choice 必须显式携带 value，`value:null` 表示删除/absent slot。Node existence 和 Label membership 只能显式选择 true 或 null。Property replacement 须是可存储的 Property 类型；Relationship、Schema、Index replacement 须符合对应 Map。unknown/duplicate conflictId、错误 choice 或非法 value 为 `INVALID_ARGUMENT`，不发布部分 resolution/ref change。

`merge.resolve` 提供当前 expectedRevision 才能更新；完全相同的 resolution batch 是 no-op、revision 不变，真实变化只递增一次。stale expectedRevision 即使 payload 相同也失败，不能当作 request-id 幂等重放。解决后的 candidate 仍须满足全图/Schema 约束。

<a id="rebase-result"></a>

## Rebase mappings 与 conflicts

`rebase` 返回 `status,commit,rewritten,conflicts`。`rewritten` 为按旧到新 replay 顺序的 List：

```text
[{from:'commit/<old id>',to:'commit/<rewritten id>'}, ...]
```

每个 rebase conflict Map 为 `{conflictId,slot,base,ours,theirs,sourceCommit}`。前五项解释同上；sourceCommit 是正在 replay 的旧 Commit Descriptor，ours 是当前 replay state，theirs 是旧 source Commit 的 intent。没有 session/revision/resolution/cursor 字段；逐步解决需重新调用整个 rebase 并提供已选择 resolutions。

相同 onto、source first-parent sequence、前序 resolution 选择下，conflictId 跨 rollback/retry 稳定。它不绑定临时 rewritten Commit ID 或时间戳；改变 source/onto 或之前的 resolution 可能产生不同 conflictId。`conflicted` 是正常返回 status，commit=null、rewritten=[]，整次 rebase 未应用；成功的 rewritten mapping 可供调用方决定是否显式复制旧 Commit Data。

相关主题：[版本工作流](../guide/versioning.md) · [Merge 指南](../guide/merge.md) · [Values](values.md) · [事务生命周期](transactions.md)。
