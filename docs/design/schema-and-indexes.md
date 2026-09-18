# Schema、Constraint 与标准索引

[设计入口](../design.md) · [开发状态与验收](../development/README.md)

本文件拥有 versioned Schema、Constraint、公共 Index namespace 及 Standard Index 的物理 generation/base/delta。Full-text 与 Vector/Managed Semantic 的专用定义、配置和生命周期分别由 [Full-text](full-text.md) 与 [Vector](vector.md) 拥有；格式迁移见 [Storage](storage.md#storage-migration)。


<a id="graph-type"></a>

## Schema-free 与 Graph Type

Root Graph Type 是 empty/open，允许 schema-free data。Cypher 25 Graph Type 只约束其声明覆盖的数据，保持 Cypher 25 open graph type semantics。

Graph Type、standalone constraints 和 index definitions 共同构成 versioned Schema state。每个 Commit 直接引用完整 canonical Schema object hash，因此历史 Snapshot 自动看到当时的 Schema。

<a id="schema-change"></a>

## Schema Change

Schema command 在同一个 write transaction 中：

1. 构造新 Schema；
2. 验证当前 Snapshot 全部受影响数据；
3. 构建/更新所需 derived index；
4. 全部成功后创建 Commit 并移动 Branch。

已有数据不满足新 constraint 时整个 command 失败，旧 Schema 与 Branch head 保持不变。

Full-text 按[Schema 执行验证与原子失败](full-text.md#schema-validation)在此边界内验证 tokenizer 的构造能力；完整 FTS corpus 仍按需构建，不把一次配置验证扩大为全图全文索引构建。Canonical write 与 Branch move 的对外成功仍以整个 invocation/transaction 成功为准。

Managed Semantic Index 按[Vector](vector.md)只在此边界执行 connection-local Provider 存在性、ABI 与纯本地配置验证，不调用远程模型、不枚举 corpus、不生成 Embedding。任何 Provider 网络、本地模型推理或批量 materialization 都位于普通 graph/schema writer 之外；因此一次 `SET content = ...` 不会等待外部 Embedding 服务。

<a id="index-types"></a>

## Index 类型

Lithograph 实现 Cypher 25 current-graph index surface：

- lookup index；
- range index；
- text index；
- point index；
- full-text index；
- vector index。

Index definition 是 versioned Schema；physical index content 是 derived cache。历史 Snapshot 缺少对应 physical cache 时允许构建 cache 或使用正确但更慢的 fallback，不得返回错误结果。

除此之外，Lithograph 提供 **Managed Semantic Index** 作为明确的数据库扩展能力。它同样使用 versioned `IndexDefinition` 与全局 index-name namespace，但不是 Cypher 25 `VECTOR INDEX` 的别名：创建和查询只通过[Vector](vector.md)的 `db.index.semantic.*` procedures；`CREATE VECTOR INDEX ... ON (stringProperty)` 永远不会被重新解释成自动 Embedding。`SHOW ALL INDEXES` 可以返回 `type = 'SEMANTIC'`，`SHOW VECTOR INDEXES` 只返回标准 Vector Index；删除继续使用通用 `DROP INDEX <name>`，不新增 `DROP SEMANTIC INDEX` grammar。

<a id="range-text-point"></a>

## Range / Text / Point

Range / Text / Point index 使用 SQLite B-tree-backed derived tables，key 编码保持 Cypher ordering、comparison、collation 与 type semantics。Planner 根据 statistics 选择 seek / scan。

Derived index 不能通过“只缓存可索引类型”改变 Cypher 的 error / `null` / cross-type ordering semantics。Exact equality / `IN` 如果目标 property 可能同时存在与 probe value 不具 equality-comparability 的其它 value family，则直接 exact seek 会错误跳过本应产生的 `TYPE_ERROR`；这种情况下只有目标 Commit 的 versioned Property Type Constraint 能证明全部 present value 与 probe equality-compatible，Planner 才能使用 exact seek，否则必须 scan/filter。Ordered comparison 对不同 value family 按[Runtime Values 与 Persistent Properties](query-engine.md#runtime-values-and-properties)的 Cypher value hierarchy 比较，因此只覆盖单一 physical key family 的 range seek 也必须有足够 Property Type proof，或由访问路径显式覆盖 hierarchy 中全部相关 family，不能把其它 family 静默过滤。String predicate 对非-String value 返回 `null`；Text/Range seek 若会预先丢弃这些 value，只能用于与 filter semantics 等价的上下文，并继续遵守 Graph View / null semantics。Point spatial predicate 同样不得因 derived cache 的类型筛选改变 observable behavior。Property Type proof 与 index definition 一样按目标 Commit 解析，不能使用 current-head Schema 或 derived cache 内容替代。

<a id="persistent-standard-index"></a>

## Persistent Standard Index Base + Delta

本节覆盖 Node/Relationship 的 Range、Text、Point 和 Relationship Lookup 物理内容；Node Lookup 继续使用既有 Label access path。Full-text/HNSW 保持各自的存储架构，其测量与回归遵守[扩展压力场景与范围控制](runtime.md#stress-workloads)。目标是**已有物理索引跨 connection/reopen 可复用，少量图变化不触发整域重建**，不是承诺删除全部缓存后的首次 query 无构建成本。

<a id="generation-layout"></a>

### Generation identity 与持久布局

物理 generation 以 `(anchor_commit, definition_hash, encoding_version)` 唯一标识。`definition_hash` 使用带 domain separator 的 canonical IndexDefinition 编码，包含 name、kind、target、完整 property 序列及 options；不能只用 index name，也不能把整个 Schema hash 当作唯一 reuse key。新增无关 constraint/index 不应使未变化的 index 失效。Hash 和 physical encoding version 都是 derived identity，不参与 canonical Commit/Layer/Schema hash。

Anchor 是**已经完整构建索引的 immutable Commit**，不要求该 Commit 恰好有 graph checkpoint。例如图 checkpoint 在 B，index 在后续 S 创建，允许从 B + delta 构建 anchor S，不复制整张图建立一个新 graph checkpoint。查询只选择 target 的 first-parent ancestry 上具有匹配 definition 的 anchor；不能把相邻 Branch、second-parent lineage 或最新 Branch head 的 index 当作当前 target 的 base。Target Schema 决定 index 是否存在和 predicate 的 type proof。

Format 3 固定增加两张 `main` derived table，逻辑字段如下；实现的 exact DDL、约束、索引名与 inventory 必须由 storage schema 常量统一生成并验证，不能运行时按用户输入拼表名：

```text
_lithograph_index_generations
  generation_id INTEGER PRIMARY KEY AUTOINCREMENT  -- database-local, never-reused internal identity
  anchor_commit BLOB(32), definition_hash BLOB(32)
  definition_blob BLOB, encoding_version INTEGER
  complete INTEGER(0|1), indexed_entities INTEGER, entry_count INTEGER
  created_at INTEGER
  UNIQUE(anchor_commit, definition_hash, encoding_version)

_lithograph_index_entries
  generation_id INTEGER, owner_kind INTEGER(1|2)
  owner_id INTEGER, property_ordinal INTEGER, token_id INTEGER NULL
  value_blob BLOB NULL, equality_blob BLOB NULL, text_value TEXT NULL
  sort_family INTEGER NULL, sort_number NUMERIC NULL
  sort_a INTEGER NULL, sort_b INTEGER NULL, sort_c INTEGER NULL, sort_text TEXT NULL
  point_crs INTEGER NULL, point_x REAL NULL, point_y REAL NULL, point_z REAL NULL
  PRIMARY KEY(generation_id, owner_kind, owner_id, property_ordinal)
```

`generation_id` 只标识derived generation，不参与canonical Commit/Layer/Schema hash；但它在同一个SQLite database内一经分配即不得复用。长寿命read guard、query-local binding与分页continuation因此能区分“同一anchor/definition被maintenance替换前后的两个物理generation”。Format 3使用SQLite `AUTOINCREMENT` 保留已删除generation的high-water mark，不依赖普通`INTEGER PRIMARY KEY`在删除最大row后可能复用rowid的行为。

范围/编码合法性、manifest 与 entries 的对应关系由 storage primitives 验证，不依赖宿主 `foreign_keys` 开关。Key encoding 沿用[Runtime Values 与 Persistent Properties](query-engine.md#runtime-values-and-properties)、[Range / Text / Point](#range-text-point)的语义；`value_blob` 保留必要的精确 recheck 值。索引前缀固定是 generation + owner kind + property ordinal，后接 equality、typed range、text 或 spatial key，并以 owner identity 处理同值重复。Relationship Lookup 使用 generation + owner kind + token + owner identity。采用固定、按非空 key family 过滤的 partial secondary indexes，避免给不适用 family 填入大量全 NULL key；相关 SQL 必须包含匹配 predicate，并经真实 plan 验证。构建一个 generation 不得 DROP 或重建其它 generation 正在使用的全部 secondary indexes。

Persistent generation 只覆盖 committed canonical state，不缓存某个 Graph View，也不持久化 Native staged state 或 Merge candidate。数据库文件本身隔离 database identity；内存 handle 还必须绑定原 connection/database，不能仅凭一个 generation number 跨库复用。

<a id="base-and-delta"></a>

### Read path 与 delta overlay

```text
target Commit + target IndexDefinition
 -> compatible first-parent anchor generation
 -> collect relevant changed owners from anchor..target Layers
 -> indexed base candidates minus all changed owners
 -> union matching final-state values of changed owners
 -> Cypher predicate recheck / Graph View / semantic LIMIT or aggregation
```

相关 owner 包含 Node add/delete、Label membership change、被索引 property set/remove，以及 Relationship add/delete/type-domain/property change；即使 owner 的新值不再匹配，也必须屏蔽旧 base entry。Composite index 任一组成 property 或 membership 改变，都重新获取该 owner 的完整最终 tuple。Staged clause 与 Merge candidate 使用相同规则，但 cache key 额外绑定 state revision/candidate identity；不能把只有 committed target 身份的 cache 用于 staged state。

Base 与 delta 的候选合并必须 bounded/streamed，按实际 predicate key 分页；不能先把全部 matching owner 收集为无界 `BTreeSet`，也不能每一输出页重新计算同一 changed-owner set。结果需要重排时按[Streaming](query-engine.md#streaming) spill，不能靠全域 scan 或大 OFFSET 模拟 indexed pagination。删除/添加 Label、property remove、相同值、复合键缺项、跨类型 ordering、`null`、并行 Branch 与历史 query 都必须与 canonical scan oracle 一致。

小变化只产生与**相关 Layer delta / changed owners**有关的解析和 point read；不复制整个 base generation 为每个新 Commit 建一份 index。Empty-delta Commit 与无关 Label/property write 不应触发 full index build。跨越较长 lineage 的 anchor 查找允许 query/connection-local 有预算的 immutable metadata cache，不能为每次查询扫描完整 Commit DAG。Delta 超过内部内存预算时允许 TEMP spill 或正确的 bounded scan fallback，并在诊断中明确标记；不能因缓存预算不足截断结果或悄悄使用过期 base。

<a id="build-publish-maintenance"></a>

### Build、publish、read-only 与 maintenance

以下是不同状态，不能在 benchmark 中混为一个“cold”数字：

| 状态 | 行为 |
| --- | --- |
| generation 完整且兼容 | 直接从持久 B-tree seek；新 connection 不重新构建 |
| 小 delta | 复用 ancestor base + delta，生成 bounded query-local overlay |
| generation 不存在、未完成或 encoding 不兼容 | 正确的 canonical scan / TEMP materialization fallback；不得返回不完整结果 |
| 明确重建或新 Index DDL | 在拥有 write authority 的 boundary 构建、原子发布 generation |

普通 read、`lithograph_rows()`、只读 SQLite connection、`EXPLAIN`、validation 和 Merge candidate inspection **不得**为 cache miss 隐式写 `main`、升级 storage format 或打开辅助 write connection。可用的 TEMP 仍只是 query-local 可重建数据；宿主连 TEMP write 也禁止时使用流式 canonical fallback。`EXPLAIN`/validation 不执行 index rebuild。Cache miss 的慢路径必须被测量，而不是从延迟报告删除。

新 Index DDL 在既有[Schema Change](#schema-change) write boundary 内生成与最终 Schema/Commit 对应的物理 generation；Native transaction 中未提交的 index 可使用 staged/TEMP 内容，只能在最终 commit 时发布到 committed identity，abort 不得留下可见 generation。既有普通 graph mutation 不逐次重建所有 index；后续 read 使用 delta，maintenance 可以在当前目标 Commit re-anchor。

为使持久 cache 删除/失效后的恢复无需 DROP/CREATE 逻辑 index、无需伪造 Commit，提供一个独立维护 procedure（不是新 Cypher grammar）：

```text
CALL lithograph.index.rebuild(name, version)
YIELD name, commit, indexedEntities
```

两个参数均为非空 STRING；`version` 使用[Version Descriptor](versioning.md#version-descriptor)已有 `commit/`、`branch/`、`tag/` descriptor，取得 writer 后解析并 pin。目标 Schema 中必须存在该 name，且属于本节支持的 Standard Index family；缺失 name、不支持的 kind/Node Lookup 或非法参数返回 `INVALID_ARGUMENT`，version 解析沿用已有稳定错误。只重建该 target 的完整 canonical index，不解释为 view-local index，不创建 Commit、不移动 ref。结果固定一行 `name: STRING, commit: STRING, indexedEntities: INTEGER`；`summary.queryType = "version"`、`summary.commit = null`、graph/schema mutation counters为0。结果中的 `commit` 是实际 anchor，`indexedEntities` 为本 generation 索引的 owner 数，不是 property-entry 数。

该 procedure 只能独立调用并后接 `YIELD`/`RETURN` 等只读结果处理，不与 graph mutation 或其它维护 operation 混在一个 execution。通过 `lithograph()` 或普通 Native execution 调用；`lithograph_rows()` 返回 `READ_ONLY_ADAPTER`；Native explicit transaction、transaction-owning subquery 或 Merge candidate 中返回 `TRANSACTION_BOUNDARY_REQUIRED`。`at` 返回 `READ_ONLY_SNAPSHOT`，`branch`、`author`、`message`、`graphView` 等不适用 options 返回 `INVALID_ARGUMENT`。`SHOW PROCEDURES`/validate/EXPLAIN 必须认识该 procedure，但 validate/EXPLAIN 无写副作用。只读文件的真实执行返回既有 I/O/read-only 错误，不吞掉用户明确请求的 rebuild failure。

Rebuild 和 DDL 使用现有 invocation SAVEPOINT/transaction discipline：从 pin 到 publish 在同一 SQLite transaction，边扫描边分批编码/写 entries，最后置 `complete=1`，成功后才让其它 reader 看到。重建已存在 generation 时旧内容的移除与替换同样原子；failure、cancel、disk-full 或 crash 只留下旧完整 generation 或无 generation，不留下可被使用的半成品。不会引入异步 worker、server、持久 build job 或 request-id 系统。此最小方案的全量重建可能长时间持有 writer，必须单独报告 build、writer-wait/hold、临时空间与峰值内存，不能把“最后设置 complete 很快”描述为整个重建的 writer 很短；使用者应在维护窗口进行全量 rebuild。

完整构建Relationship Lookup本来就需要枚举全部Relationships。Relationship property generation有可复用的兼容Lookup/type访问路径时优先使用；不存在时，显式首次全量build允许一次有界canonical枚举并记录其全部成本，不能称为type seek。已经ready的property/Lookup读取不再重复这一过程。除非新的工作量证据证明必要，不为构建捷径追加一套覆盖所有Relationships的重复永久索引。

Generation 成为 complete 后视为不可原地改写的内容。Read guard 保护正在使用的 generation；maintenance 负责成组删除 manifest/entries，不能通过读路径清理数据库。默认每个 definition 保留至多两个 complete anchors（保留本次目标并逐出最旧的其它 anchor），这是可调整但不公开配置化的性能策略；被逐出版本继续使用 ancestor/fallback。显式 canonical GC 同时清理 anchor 已不可达的 generation；generation **不是** canonical GC root，不延长用户已删除历史的生命周期。Manifest/entry count、encoding 和被访问 payload 的可检测异常使整个 generation 失效并回退，不允许跳过坏 entry 返回少量“正常”结果。完整 cache-vs-canonical 检查属于显式 integrity gate，普通 query 不全量重算 index checksum。真正的 canonical corruption、内部 schema/trigger 篡改继续 fail closed，而不是伪装成 cache miss。

<a id="adoption-boundary"></a>

### Adoption boundary

Cache失效在首行输出前被发现时，可以切换canonical fallback重新执行。已经向调用方发出结果后才发现损坏，不得从头fallback造成重复行或返回成功SUMMARY；除非能证明continuation的等价性，否则使用既有 `STORAGE_ERROR` 终止该execution、清理read资源，要求显式完整性检查/重建后重试。常规maintenance删除generation必须同时使manifest失效并原子清理entries；离线任意修改单个entry而保留完整marker属于篡改，不能承诺每次point query在不全量核验的情况下都能检测。

可删除的 derived data 在这里指 generation manifest/entries，而不是任意修改内部 table definition。Format 3 的结构仍由[加载与初始化](interfaces.md#loading-and-initialization) exact inventory 校验；缺表、错列、额外 trigger/index 不能被静默当作可用缓存。完整重建入口解决 payload 层缺失；结构损坏沿用明确的恢复/迁移检查。`lithograph_init()` 的 format migration 只建立空 cache 结构，不在迁移期间扫描所有图数据建立每个索引。

本节不改变 Graph Type/Constraint 的校验域与[Range / Text / Point](#range-text-point) type proof。不能仅因为 persistent index 看起来完整，就把 derived entries 当成所有 canonical owner/value 的可信替身；任何约束验证加速必须另有针对相同 target state 的等价性证明及失败回归。
