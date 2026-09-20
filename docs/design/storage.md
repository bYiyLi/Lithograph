# 版本化存储、事务与恢复

[设计入口](../design.md) · [开发状态与验收](../development/README.md)

本文件拥有 canonical 存储/编码、邻接访问、Snapshot resolution、事务与并发、完整性、恢复及 storage-format 迁移。版本操作行为见 [Versioning](versioning.md)；Standard Index 专有物理布局见 [Schema / Index](schema-and-indexes.md#generation-layout)，Embedding cache 的 key、容量与生命周期见 [Vector](vector.md#embedding-result-cache)。

<a id="version-aware-storage"></a>

## Version-aware Storage Model

<a id="canonical-history"></a>

### Canonical History

Canonical source of truth 不是一组可覆盖的“当前 Node/Relationship 表”，而是：

```text
Immutable Commit DAG
        +
Immutable graph delta Layers
        +
Versioned Schema object
        +
Mutable Branch refs
```

每个 Commit 的 graph layer 表示相对 **first parent** 的标准化变化。Merge Commit 的 second parent 只记录第二条历史边；合并后的完整 delta 仍相对 first parent 保存，因此 Snapshot 重建只需沿 first-parent chain 应用 layer。

Commit Data、Tag 与 Merge Session 不加入上述 canonical graph Snapshot source of truth。Commit Data 只解释某个 Commit，Tag 只命名某个 Commit；Merge Session 是尚未 finalize 的 mutable operational workspace。修改这些 mutable state 不能改变既有 Commit、Layer、Schema hash 或 Snapshot resolution。Merge Session 只有在 `merge.finalize` 成功时才通过正常 Commit/ref transaction 影响 canonical history。

<a id="internal-tables"></a>

### Internal Tables

所有内部持久化对象使用 `_lithograph_` prefix。核心结构固定为：

```text
_lithograph_meta
  key TEXT PRIMARY KEY
  value BLOB

_lithograph_sequences
  kind INTEGER PRIMARY KEY
  next_id INTEGER NOT NULL

_lithograph_labels(id INTEGER PRIMARY KEY, name TEXT UNIQUE)
_lithograph_rel_types(id INTEGER PRIMARY KEY, name TEXT UNIQUE)
_lithograph_prop_keys(id INTEGER PRIMARY KEY, name TEXT UNIQUE)

_lithograph_commits
  id BLOB PRIMARY KEY
  format_version INTEGER
  parent1 BLOB NULL
  parent2 BLOB NULL
  layer_id INTEGER NOT NULL
  schema_hash BLOB NOT NULL
  author TEXT NULL
  message TEXT NULL
  committed_at INTEGER NOT NULL

_lithograph_branches
  name TEXT PRIMARY KEY
  commit_id BLOB NOT NULL

-- storage format 2 adds:
_lithograph_commit_data
  commit_id BLOB PRIMARY KEY
  data_json TEXT NOT NULL

_lithograph_tags
  name TEXT PRIMARY KEY
  commit_id BLOB NOT NULL

_lithograph_merge_sessions
  id TEXT PRIMARY KEY
  target_branch TEXT NOT NULL
  ours_commit BLOB NOT NULL
  theirs_commit BLOB NOT NULL
  revision INTEGER NOT NULL
  created_at INTEGER NOT NULL

_lithograph_merge_resolutions
  session_id TEXT NOT NULL
  conflict_id BLOB NOT NULL
  resolution_json TEXT NOT NULL
  PRIMARY KEY(session_id, conflict_id)

_lithograph_layers
  id INTEGER PRIMARY KEY
  hash BLOB UNIQUE NOT NULL

_lithograph_node_delta
  layer_id, node_id, op

_lithograph_label_delta
  layer_id, node_id, label_id, op

_lithograph_rel_delta
  layer_id, relationship_id, source_id, type_id, target_id, op

_lithograph_property_delta
  layer_id, owner_kind, owner_id, key_id, op,
  type_tag, int_value, real_value, text_value, blob_value, aux_value

_lithograph_schema_objects
  hash BLOB PRIMARY KEY
  canonical_blob BLOB NOT NULL

_lithograph_checkpoints
  commit_id BLOB PRIMARY KEY
  created_at INTEGER
  metadata BLOB

_lithograph_cp_nodes
_lithograph_cp_labels
_lithograph_cp_relationships
_lithograph_cp_properties
```

`op` 是封闭枚举：add/remove 或 set/remove，取决于 delta table。一个 Layer 内同一 logical slot 在 canonicalization 后只保留一个最终 operation。

Storage format 1 冻结以下 physical key 与 payload contract；后续若改变列语义、主键或 canonical access path，必须提升 storage format：

- `_lithograph_sequences.kind`：`1 = NodeId`、`2 = RelationshipId`、`3 = LabelId`、`4 = RelationshipTypeId`、`5 = PropertyKeyId`、`6 = LayerId`；`next_id` 始终是下一个可分配的正 `INTEGER64`。
- dictionary table 以 `id` 为主键并对 `name` 建唯一索引；名称按 SQLite `BINARY` bytes 比较，不做 case folding。
- `_lithograph_commits.id`、`parent1`、`parent2`、`schema_hash` 与 `_lithograph_layers.hash` 都保存原始 32-byte BLAKE3 digest；公开 Commit ID 才转成 64 位 lowercase hex。
- delta `op`：`1 = add/set`，`2 = remove`；`owner_kind`：`1 = Node`，`2 = Relationship`。
- `_lithograph_node_delta` 主键 `(layer_id, node_id)`；`_lithograph_label_delta` 主键 `(layer_id, node_id, label_id)`；`_lithograph_rel_delta` 主键 `(layer_id, relationship_id)`；`_lithograph_property_delta` 主键 `(layer_id, owner_kind, owner_id, key_id)`。
- `_lithograph_cp_nodes` 主键 `(commit_id, node_id)`；`_lithograph_cp_labels` 主键 `(commit_id, node_id, label_id)`；`_lithograph_cp_relationships` 主键 `(commit_id, relationship_id)`；`_lithograph_cp_properties` 主键 `(commit_id, owner_kind, owner_id, key_id)`。
- `Relationship` delta 的 remove row 仍保存创建时的 `source_id / type_id / target_id`，使 overlay 可以建立 forward/backward tombstone 而不扫描全部 Relationship。
- Property `type_tag`：`1 Boolean`、`2 Integer`、`3 Float`、`4 String`、`5 List`、`6 Date`、`7 LocalTime`、`8 Time`、`9 LocalDateTime`、`10 ZonedDateTime`、`11 Duration`、`12 Point`、`13 Vector`、`14 UUID`。remove row 的 `type_tag` 与所有 payload column 均为 `NULL`。
- Boolean / Integer / Date / LocalTime 使用 `int_value`；String 使用 `text_value`；Float 的 numeric value 使用 `real_value`，同时在 `aux_value` 保存 canonical binary64 bits，以保证 signed zero 与 canonical NaN 可重算；Time、LocalDateTime、ZonedDateTime 使用 `int_value` 保存主时间量并用 `aux_value` 保存其余 fixed-width/zone payload；List、Duration、Point、Vector、UUID 使用 `blob_value` 保存 LCE1 typed value bytes。
- checkpoint property row 使用与 set property delta 完全相同的 tagged payload contract，不保存 remove row。
- `_lithograph_checkpoints.metadata` 只保存可重建 derived metadata；当前统计 metadata 使用 versioned payload 记录该 checkpoint Snapshot 的 Node / Relationship 总量与 label / relationship-type cardinality。该 payload 不参与 Commit/Layer hash，缺失或不可解析时 Planner 必须保守降级，不能影响 Snapshot correctness。

Storage format `2` 保留 format `1` 的全部 canonical graph table / key / LCE1 contract，并增加 Commit Data / Tag sidecar 与 Merge Session operational state。`data_json` 必须是合法 JSON value 的 UTF-8 JSON 表达；普通说明文本使用 JSON string。Commit Data 不作为 Cypher Property，因此不受 PropertyValue 持久化类型限制，Engine 只验证 JSON 合法性而不解释 key 或业务 schema。没有 `_lithograph_commit_data` row 表示该 Commit 没有 Data；显式 JSON `null` 是一个已存在的 Data value，与无 row 不同。

Merge Session `id` 使用 `merge-session/<RFC-9562-uuid>` 的 lowercase text，但在所有 API 中视为 opaque token，不能当作 `branch/`、`tag/` 或 `commit/` version descriptor。`ours_commit` / `theirs_commit` 是 Session 创建时 pin 的 immutable Commit；`revision` 从 `1` 开始，只在 resolution set 发生有效变化时单调递增；`created_at` 使用 UTC Unix epoch microseconds，只用于 operational listing/diagnostics，不参与任何 Commit hash 或 merge correctness。`resolution_json` 使用[Three-way Merge 与 Merge Session](versioning.md#merge-session)的 `ours | theirs | value` shape，其中 explicit value 使用[Lithograph JSON](interfaces.md#lithograph-json) Lithograph JSON typed-value encoding。Conflict 集合、merge candidate 与分页 materialization 不作为持久化真源，可从 pinned Commit + resolution set 确定性重算；实现可以使用 query-local / TEMP spill，但不能要求把全部 conflict 或 candidate Snapshot 永久 materialize 到 main schema。

Merge Session encoding/semantics 是 storage format `2` contract 的一部分；未来若 merge algorithm/session encoding 的不兼容变化会让同一 pinned inputs + resolution set 得到不同 candidate，必须通过显式 storage-format migration 处理 open Session，不能在升级后静默用新语义重新解释旧 Session。

Open Merge Session 是 GC reachability root：其 `ours_commit` / `theirs_commit` 及所需 ancestors 在 Session finalize/abort 前不能被 canonical GC 删除。Session finalize/abort 会原子删除 session + resolution rows；derived candidate/conflict spill 随时可以丢弃重建。

Storage format 1 的 canonical explicit index inventory 除 table primary key 外固定包含：dictionary name unique indexes、Layer hash unique index，以及[Physical Access Indexes](#physical-access-indexes)列出的 relationship outgoing/incoming/global-identity、label reverse lookup。不得依赖 SQLite 自动生成且名称/布局不受 Lithograph 控制的 secondary index 作为 canonical access path。

`Property` 使用 tagged union storage：`type_tag` 决定 payload column 与 `aux_value` 的解释。复杂 property type 使用 canonical binary representation，不能因 JSON serialization 损失类型。

Lithograph Canonical Encoding v1（`LCE1`）固定所有进入 Layer/Schema/Commit hash 的字节表示：

- 所有整数使用 little-endian two's-complement fixed-width；ID 与 Cypher Integer 使用 64 bit；
- length/count 使用 unsigned LEB128；
- Boolean 使用单字节 `0` / `1`；
- Float 使用 IEEE-754 binary64 little-endian；所有 NaN payload canonicalize 为 quiet NaN `0x7ff8000000000000`，其它 bit pattern（包括 signed zero）原样保留；
- String/identifier 使用“UTF-8 byte length + UTF-8 bytes”，不做 Unicode normalization 或 case folding；
- UUID 使用 RFC 9562 16-byte network-order value；
- List 使用 element count + 逐项 tagged value；
- Date 使用 Unix epoch 起 signed day count；Local Time 使用 midnight 起 nanoseconds；offset Time 额外保存 signed offset seconds；
- Local DateTime 使用 day count + nanoseconds-of-day；Zoned DateTime 使用 epoch seconds + nanoseconds + exact zone-id string（或 fixed offset）；
- Duration 使用 months + days + seconds + nanoseconds 四元组；
- Point 使用 CRS identifier + coordinate count + binary64 coordinates；
- Vector 使用 coordinate-type tag + dimension + packed fixed-width coordinates；
- `null`、absent property 和 remove operation 使用不同 tag/op，不可互换；
- Schema canonical blob 按 object canonical identifier 排序；object 内 field/property/index/constraint 按 canonical name 排序；
- 所有 hash record 使用 domain tag + length-delimited fields，禁止依赖字符串拼接边界。

`LCE1` record framing 进一步冻结为：`"LCE1" + uleb128(domain_len) + domain_bytes + uleb128(field_count) + repeated(uleb128(field_len) + field_bytes)`。Domain 固定使用 ASCII bytes。format 1 使用 `VALUE`、`NODE`、`LABEL`、`REL`、`PROPERTY`、`LAYER`、`SCHEMA`、`COMMIT` domain；Layer logical slot 排序固定为 Node、Label、Relationship、Property 四个 family，再按各 family 主键中的 signed integer 数值升序。operation 的 numeric tag 使用上文 `op` 值。

`VALUE` 的第一个 field 是单字节 `type_tag`，后续 field 按对应 value contract 编码。List 使用 `uleb128(element_count) + repeated(uleb128(value_record_len) + VALUE_record)`；Point 保存 signed 64-bit CRS identifier、ULEB128 coordinate count 与 canonical binary64 coordinates；Vector 保存单字节 coordinate type（`1 i8`、`2 i16`、`3 i32`、`4 i64`、`5 f32`、`6 f64`）、ULEB128 dimension 与 packed little-endian coordinates，floating vector NaN 同样 canonicalize 为 quiet NaN；UUID field 固定为 RFC 9562 16-byte network-order value。

`COMMIT` fields 固定顺序为 `format_version, parent1, parent2, layer_hash, schema_hash, author, message, committed_at`。optional field 使用首字节 `0` 表示 absent、`1 + payload` 表示 present。Root Commit 使用 `parent1 = null`、`parent2 = null`、`author = null`、`message = null`、`committed_at = 0`；因此 empty Layer、empty Schema 与 Root Commit 都是 deterministic content-addressed object。普通 Commit 的 `committed_at` 仍遵循[Content Addressing](#content-addressing) commit timestamp contract。

`LCE1` 属于 storage-format contract。修改上述编码必须提升 storage format，并通过 migration 保持旧 Commit ID 可验证。

<a id="physical-access-indexes"></a>

### Physical Access Indexes

Canonical delta 与 checkpoint 至少建立以下 B-tree access paths；这是大规模遍历正确实现的一部分，不是可选优化：

```text
relationship delta:
  (layer_id, source_id, type_id, target_id, relationship_id)
  (layer_id, target_id, type_id, source_id, relationship_id)
  (layer_id, relationship_id)
  (relationship_id, layer_id)

checkpoint relationships:
  (commit_id, source_id, type_id, target_id, relationship_id)
  (commit_id, target_id, type_id, source_id, relationship_id)
  (commit_id, relationship_id)

labels:
  (layer_id|commit_id, node_id, label_id)
  (layer_id|commit_id, label_id, node_id)

properties:
  (layer_id|commit_id, owner_kind, owner_id, key_id)
```

Snapshot overlay 对增量 add/remove 建立同构的 query-local lookup structure。`ExpandAll/ExpandInto` 必须合并 checkpoint adjacency 与 overlay add/tombstone，不允许为了 overlay 便利回退到 Relationship 全扫描。

<a id="adjacency-keyset-cursor"></a>

#### Adjacency Keyset Cursor

邻接分页使用与上述 B-tree 一致的内部复合位置，不再对每页仅按 `relationship_id > after ORDER BY relationship_id` 访问。固定 endpoint / type 后，比较和排序键如下：

| 访问 | 固定前缀 | 剩余 keyset position |
| --- | --- | --- |
| outgoing、指定 type | checkpoint/Layer + source + type | `(target_id, relationship_id)` |
| incoming、指定 type | checkpoint/Layer + target + type | `(source_id, relationship_id)` |
| outgoing、任意 type | checkpoint/Layer + source | `(type_id, target_id, relationship_id)` |
| incoming、任意 type | checkpoint/Layer + target | `(type_id, source_id, relationship_id)` |

第一页无 continuation predicate；后续页使用严格 lexicographic `>` 和同序 `ORDER BY` / `LIMIT`。例如 typed outgoing 用 `(target_id, relationship_id) > (?, ?)`，不能把 range predicate 和 sort key 分别落在两个不一致的顺序上。优先使用现有覆盖索引；`INDEXED BY` 只在 SQL/key 已正确、固定内部索引存在性被验证后作为防 plan regression 的约束，不作为掩盖错误分页的 hint。不能每页重扫/排序整个邻接域，也不默认增加一套覆盖 100M Relationships 的重复索引。

Cursor 绑定 graph-state identity/revision、起点、方向和 type selector；换 Snapshot、起点、type 或 staged revision 时必须重新建立，不能把旧位置解释为新查询的位置。它只属于内部执行器，不是 public History/Merge cursor，也不改变持久 RelationshipId、Layer canonical sort 或 Commit hash。内部 Rust scan API 的所有调用者必须一起迁移，不能保留一个仍按全局 RelationshipId 扫描的隐蔽热路径。

Checkpoint 与 overlay 使用相同 tuple order 做有界归并：overlay tombstone 屏蔽 base，新增 relationship 按位置插入，同一 identity 只出现一次；同端点平行边由 RelationshipId 区分。Continuation 推进到**最后实际检查的位置**，即使整页均被 tombstone 或 view 过滤也必须继续，不漏行、不重复、不死循环。内存与输入页/overlay 相关，不与全部 degree 或最终输出行数相关。

Incident/undirected access 分开读取 outgoing 和 incoming 两个有序流，使用包含 half-stream 状态的 continuation，避免 `source_id = n OR target_id = n` 退化为全库扫描。Storage incident enumeration 只发一次 self-loop；Cypher pattern orientation、反向边、平行边和 path match-mode 的重复语义仍由 executor 按既有合同处理，不能把所有同端点边去重。无 `ORDER BY` 的内部行顺序不是公开保证，但不能借此改变显式排序、LIMIT/SKIP 的合法结果、path selection、null/error 或 aggregation 语义。

验证同时检查 Lithograph operator 与 SQLite 的实际 access plan/执行工作量；一个叫 `AdjacencySeek` 的 operator 或少量 logical `dbHits` 不足以证明没有扫描无关 Relationship，见[Performance Evidence Contract](runtime.md#performance-evidence)。

<a id="content-addressing"></a>

### Content Addressing

Layer、Schema object 与 Commit 使用 256-bit BLAKE3 content hash。

- Layer hash 基于按 logical key canonical sort 后的 delta bytes；
- Schema hash 基于 canonical schema representation；
- Commit hash 基于 `format_version + parent IDs + layer hash + schema hash + metadata`；
- Commit ID 对外为 64 个 lowercase hex characters。

Commit Data 与 Tag name/ref 不进入 Layer / Schema / Commit hash。更新或删除 Commit Data、创建/移动/删除 Tag 都不能改变既有 Commit ID；这些 sidecar mutation 由 SQLite transaction 提供原子 durability。

`format_version` 是 hash input 的一部分。后续 storage migration 不允许静默重算既有 Commit ID。

`committed_at` 使用 UTC Unix epoch microseconds，并在 **Commit finalization preparation** 时读取一次 Engine wall clock后冻结。它属于 Commit metadata 和 hash input，但不作为 DAG ancestry 或 merge correctness 的依据，也不复用[Temporal Clock Boundary](query-engine.md#temporal-clocks)的 query transaction/statement temporal clock。

Finalization 分成两个内部阶段，以同时满足 content-addressed Commit identity 与 SQL result preflight：

1. **preparation**：在最终 candidate graph/Schema 已确定且仍处于可 rollback boundary 内时，冻结 `committed_at`，得到最终 Layer/Schema hash，计算 prospective Commit ID，并构造 public summary/result 所需的 Commit metadata；此时不得把 Commit/ref 变成 durable/externally visible success；
2. **publication**：只有 owning adapter 对将要公开的 envelope/summary 完成 serialization、SQLite length/resource 等全部可检测 preflight 后，才持久化/确认 prepared Layer + Commit、执行 Branch compare-and-move，并 release/commit 对应 SQLite boundary；public success result 只能在 publication 成功后暴露。

preparation 后若发生 serialization/resource/cancel/constraint-finalization/SQLite publication failure，整个 ordinary execution rollback，prepared timestamp/Commit ID 从未成为 durable history；重试是新 execution，会重新取得 `committed_at` 并可能得到不同 Commit ID。该 two-phase finalization 不改变 Commit hash fields，也不把 timestamp 从 hash 中移除。`CALL ... IN TRANSACTIONS` 已完成 inner batch 的 Commit 已经经过各自 preparation + publication，不受之后 outer result failure 反向影响。

<a id="snapshot-resolution"></a>

### Snapshot Resolution

Snapshot Resolver：

1. 定位目标 Commit；
2. 找到 first-parent ancestry 中最近 checkpoint；
3. 使用 checkpoint 作为 immutable base；
4. 按祖先到目标顺序叠加后续 delta layer；
5. 形成 query-local overlay，并将目标 Commit 作为 Snapshot identity。

Root Commit 的 base 是空图。

Checkpoint 是可删除、可重建的 derived cache，不改变 Commit DAG。创建 checkpoint 不生成用户可见 Commit。

<a id="checkpoint-and-derived-cache"></a>

### Checkpoint 与 Derived Cache

Lithograph 自动根据 delta-chain 深度、累计 delta 体量和 query cost 创建 checkpoint；这些阈值属于 semantic-neutral performance policy，不是持久化兼容合同。

Statistics、range/text index materialization、FTS index、vector HNSW graph 与 Snapshot overlay cache 同样属于 derived data。删除它们不得改变 query result，只影响性能；缺失时必须能够从 canonical history 重建或回退到正确的 scan。

<a id="transactions-and-concurrency"></a>

## Transaction 与 Concurrency Model

<a id="sqlite-durability"></a>

### SQLite Transaction 是 durability boundary

所有 canonical graph change、Commit row、Layer、Schema object 与 Branch head move 必须处于同一个 SQLite transaction。任何一步失败，整个 write 不可见。

默认 auto-commit 模式下，一个成功的 top-level mutating Cypher query：

```text
pin active branch head
    -> execute clauses against pinned snapshot + staged writes
       under the fixed graphView selector
    -> validate constraints
    -> canonicalize delta
    -> write immutable layer/commit
    -> compare-and-move branch head
    -> SQLite COMMIT
```

上图的 `SQLite COMMIT` 对 SQL explicit transaction 表示 `lithograph_tx_commit()` 提交由 `lithograph_tx_begin()` 建立的 Engine-owned transaction；对普通 `lithograph()` / `lithograph_rows()` execution 表示 invocation/cursor 自己的 write boundary 完成后，由宿主 SQLite autocommit/outer transaction 决定最终 durability。普通 execution 不能提前提交 caller-owned transaction。

普通 auto-commit execution 中，每个成功的 **graph / Schema / Index mutating query** 都产生一个 Commit，即使 effective delta 为空；这样 graph history 与 write intent 一致。Explicit transaction 改变的是多个 execution 的 Commit boundary，而不是这些 query 的 Cypher mutation semantics，规则见 [Explicit Transaction](#native-explicit-transaction)。Version ref/control procedure 不一概产生 Commit：`branch.create/delete`、`reset` 与 `merge.finalize` 的 fast-forward 结果只原子修改 ref，`branch.checkout` 只修改 connection-local context，`gc` 只做 reachability cleanup，Merge Session 的 start/resolve/abort 只修改 operational workspace；`patch.apply`、`merge.finalize` 的 diverged `merged` 结果与 `revert` 会产生 Commit。

<a id="native-explicit-transaction"></a>

### Explicit Transaction（SQL lifecycle）

Explicit transaction 解决的是 **version atomicity**：调用方通过 SQL `lithograph_tx_begin()` 建立 connection-local staged state，随后继续使用正常的 `lithograph()` / `lithograph_rows()` execution surface 执行多个 Cypher，最后用 `lithograph_tx_commit()` 或 `lithograph_tx_abort()` 结束。整个成功写单元只产生一个 Layer、一个 Commit 和一次 Branch head move；不提供 `lithograph_tx_execute()`，也不建立第二套 staged storage、query engine 或 application-facing Native transaction adapter。

逻辑生命周期固定为：

```text
lithograph_tx_begin(branch?, expectedHead?, author?, message?)
    -> pin base Commit under writer ownership
    -> lithograph(...) / lithograph_rows(...) execution A
    -> lithograph(...) / lithograph_rows(...) execution B
    -> ...
    -> lithograph_tx_commit()
       -> canonicalize final net delta
       -> write at most one Layer / one Commit
       -> compare-and-move Branch once

or

lithograph_tx_abort()
    -> discard all staged state
    -> no Commit / no ref move
```

精确语义：

- `lithograph_tx_begin` 只能在目标 `sqlite3*` 处于 autocommit mode 且没有其它 active Lithograph explicit transaction 时成功；否则返回 `TRANSACTION_BOUNDARY_REQUIRED`。成功 begin 进入 Engine-owned SQLite write transaction / branch-commit coordinator，取得该 database 的 single-writer ownership，并 pin target Branch 当前 head 为 immutable base Commit；
- active explicit transaction 内，`lithograph()` 与 `lithograph_rows()` 自动使用同一 base + transaction-local staged graph/Schema/Index state。后续 execution 必须看到前序成功 execution 的 staged writes；这些 staged state 在 `tx_commit` 前没有 public Commit identity、不会移动 Branch，也不会被其它 connection 读取；
- 每个 execution 的 `graphView` 仍是 query-local selector，可以与前一个 execution 不同；visibility 必须针对当前 staged state 重新计算，不能在 begin 时预展开成固定 element-ID membership；
- 每个 execution 继续执行普通 Cypher immediate semantics：parse/semantic/type、Graph View、Schema/Constraint 与 statement failure behavior 都针对当前 staged state 生效。Explicit transaction 不把 constraint 自动变成 deferred constraint；
- active transaction 固定 target Branch / base / `expectedHead` / final Commit metadata；execution 不能通过 `branch` / `at` / `author` / `message` / `mergeSession` 切换 transaction/version context。Version Procedure、Branch/Tag/Commit Data mutation、checkout/GC 等拥有独立 version/ref lifecycle 的 operation返回 `TRANSACTION_BOUNDARY_REQUIRED`；
- `CALL { ... } IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 不能在 explicit transaction 内执行，因为它们要求独立 durable inner transaction boundary；不能用 SAVEPOINT 把这种语义改成可被外层整体 rollback 的 nested savepoint；
- External I/O 本身不是拒绝理由。普通 `LOAD CSV`、Managed Semantic query 等只要不另行拥有独立 transaction/version lifecycle，就可以在 active transaction 内运行；显式 rebuild/maintenance procedure 是否允许由其 committed-target lifecycle contract 决定。调用方承担网络/文件/model 等待期间延长 writer ownership 的成本；
- 任一 `lithograph()` execution failure、`lithograph_rows()` failure/interrupt/cancel、或 streaming cursor 在 success `summary` 前关闭，都使整个 explicit transaction fail closed：全部 staged state rollback，active state 清除，不能继续 execute/commit；cleanup 失败沿用 [SQL Bridge](interfaces.md#sql-bridge) 的 `INTERNAL_ERROR` + 丢弃 connection 语义；
- transaction 内新分配的 Node/Relationship identity 可以由后续 execution 通过正常 query result 引用，但在 `tx_commit` 成功前只属于 provisional staged state；abort 后不得当作 durable element；
- `tx_commit` 对最终 candidate state 再执行 canonical integrity / Schema / Constraint validation，按 base -> final staged state 计算 canonical net delta。只要 transaction 内至少成功执行过一个 graph/Schema/Index mutating execution，就创建恰好一个 Commit；即使最终 net delta 为空，也创建 empty-delta Commit 以保留 write intent。纯 read transaction 不创建 Commit并返回 base Commit；
- 最终 Commit 的 parent 是 begin pin 的 base Commit，`author/message` 来自 begin options。`lithograph_tx_commit()` 先按上面的 finalization preparation 冻结 `committed_at` 并计算唯一 prospective Commit identity/counters，完成 tx-commit result JSON/length/resource preflight；只有 preflight 通过后才 publication + Branch move + SQLite `COMMIT`。结果 preflight 失败必须 rollback 整个 explicit transaction，prepared ID 不进入 history；SQLite `COMMIT` 成功后发生的宿主 result-delivery failure则不能反向撤销 durable Commit。Branch 只在 Commit 与 Layer 已成功写入后移动一次，commit 前继续执行 compare-and-move / integrity guard；
- explicit transaction 占用 SQLite 单文件 writer ownership。Lithograph 不因性能偏好禁止 external I/O，但调用方应避免把无界用户交互或无界外部工作放进长事务。

active transaction 内每个成功 execution 的结果合同仍由 [Interfaces](interfaces.md#results-and-errors) 定义；`summary.commit = null`，statement counters 描述 provisional effect。只有 `lithograph_tx_commit()` 返回最终 durable Commit identity 与 transaction-level final-delta counters。

逻辑 lifecycle 结果固定为：`tx_begin -> {"baseCommit":"commit/<id>"}`；`tx_commit -> {"commit":"commit/<id>","counters":{...}}`，纯 read transaction 的 `commit == baseCommit` 且 counters 全为 `0`；`tx_abort -> {"aborted":true}`。没有 active transaction 时 commit/abort 返回 `INVALID_ARGUMENT` + `SQLITE_MISUSE`。

`tx_abort` rollback Engine-owned SQLite transaction并清除全部 staged/connection-local state；cleanup 失败返回 `INTERNAL_ERROR`，该 connection 必须关闭并丢弃。`tx_commit` 成功或任何 fail-closed abort 后 transaction 都进入 terminal 状态。connection 未 terminal 即 teardown 时，SQLite rollback 是最后 durability boundary；reopen 后不得出现该 transaction 的 Commit、Layer 或 Branch move。

<a id="caller-owned-transaction"></a>

### Caller-owned SQLite Transaction

如果调用方已经 `BEGIN` SQLite transaction，同一 connection 内每个 mutating Cypher query 仍产生独立逻辑 Commit 并连续移动 Branch head，但这些 Commit 与 ref move 只有在外层 SQLite `COMMIT` 后才对其它 connection 可见。外层 `ROLLBACK` 会移除这一 transaction 中创建的全部 graph Commits。

因此 caller-owned SQLite transaction 只提供 **durability atomicity**，不等价于 [Explicit Transaction](#native-explicit-transaction) 的 version atomicity。一个 outer SQLite transaction 可以整体回滚 Commit A/B/C，但只要最终 SQLite COMMIT 成功，历史中仍保留 A -> B -> C；需要一个逻辑版本节点时必须使用 Lithograph SQL explicit transaction，而不是事后隐式 squash。

<a id="stale-branch-head"></a>

### Stale Branch Head

Write 在开始时记录 base Commit，在写 Branch ref 前再次 compare current head。若同一 Branch 已被其它 writer 移动，当前 write 失败为 `BRANCH_HEAD_MOVED`；Lithograph 不自动把两个并发写隐式 merge。

普通 auto-commit write 使用 query-level base；SQL explicit transaction 使用 `tx_begin` pin 的 base / `expectedHead`，并由 [Explicit Transaction](#native-explicit-transaction) 的 writer ownership 把 compare-and-move boundary 扩展到整个 transaction。

<a id="readers-and-writers"></a>

### Readers and Writers

Read query pin immutable Commit，因此不会读取半完成 Layer。SQLite 的 concurrency mode 决定物理锁与 WAL 行为；Lithograph 不增加第二套 lock manager。

不同 connection 可以同时 checkout 不同 Branch。SQLite 仍是单文件 write serialization 的最终仲裁者。

<a id="transaction-subqueries"></a>

### Cypher Transaction Subqueries

普通 `lithograph()` / `lithograph_rows()` 在 SQLite autocommit mode 且没有 active Lithograph explicit transaction 时实现 `CALL { ... } IN TRANSACTIONS`：每个 batch 对应独立 SQLite transaction 与一个或多个按 query semantics 产生的 graph Commits。caller-owned SQLite transaction 或 active Lithograph explicit transaction 已占有外层 transaction boundary 时返回 `TRANSACTION_BOUNDARY_REQUIRED`。

精确规则：每个成功且发生 graph/schema/index mutation 的 batch 创建 **一个** graph Commit；read-only batch 不创建 Commit。某个 batch 失败时仅该 batch transaction rollback；后续 batch 是否继续、状态列和最终 query error 按冻结 Cypher Profile 的 `ON ERROR` / status semantics 执行，因此已经 durable commit 的成功 batch 不被后续独立 batch failure 回滚。

`lithograph_rows()` 执行 transaction subquery 时必须按 inner batch 增量推进，不能先跑完整个 transaction program 再保存最终 row set；但**一个 inner batch 自身是 result semantic barrier**。原因是该 batch 的 transaction 最终成功/失败、`ON ERROR FAIL | CONTINUE | BREAK | RETRY` 与 `REPORT STATUS` 会决定 outer query 实际应该看到 successful subquery rows，还是该 batch / remaining input 对应的 failure/status rows。实现因此不得在 batch outcome 未知时把 subquery success row作为 public event提前泄漏。

batch 内 operator 仍按正常 incremental execution 推进，其候选 public rows 使用 bounded memory + TEMP/disk spill保存，不得因为 batch 很大就把完整 batch row set 留在 RAM。batch transaction 成功且 durable commit 后，才从该 batch spill 增量输出 committed successful rows，并附加/投影 transaction status；batch 失败则丢弃其 provisional successful-row spill，并按冻结 Cypher Profile 生成 failure/status rows或终止 outer execution。`RETRY` 的失败 attempt 同样不得泄漏 provisional row，只有最终有效 attempt 的 outcome可以进入 outer stream。这样 transaction boundary 本身是允许的 semantic barrier，但 retained memory仍不与 batch/final result count线性增长。

Outer transaction-subquery input 同样按 upstream cursor 增量拉取并装入当前 bounded batch，不得先把全部 prefix/input rows materialize 后再开始第一个 inner transaction。若 prefix 自身包含 `ORDER BY`、aggregation、`DISTINCT` 等 operator barrier，则只按这些 operator 的既有 TEMP/spill contract物化其必要状态；transaction batching本身不增加第二个“全 input RowSet” barrier。`ON ERROR BREAK` 之后仍需返回 skipped/failed status 的 remaining input时，从 upstream继续增量消费但不再执行 inner subquery transaction，逐行生成 Profile定义的 status/result；不要求预知剩余 cardinality。`LOAD CSV` 或其它流式 producer进入 transaction batching时遵守同一 bounded pipeline。

early-close / interrupt / result-delivery failure按实际 batch lifecycle处理：尚在执行且未 commit 的 current batch rollback；已经 commit、正在从 spill 向 caller 排出的 batch保持 durable，即使其剩余 result rows未被消费或某个 public row 的 JSON/SQLite encoding/result handoff随后失败；已经完成的 earlier batch同样保留；outer execution终止后续 batch不再执行。Outer execution因此可以没有 success summary，但这不表示任何已 publication 的 inner transaction被撤销。已经 commit 的 batch spill只是结果传递资源，cursor close/error时可以直接丢弃未消费部分，不影响 durable graph state。这个规则与普通单 transaction mutation不同：后者在 finalize-success 前的 row/summary encoding failure仍会 rollback整个 ordinary write。

`IN CONCURRENT TRANSACTIONS` 可以并行执行不需要 SQLite write lock 的 parse/parameter/materialization preparation，但同一 active Branch 的真正 batch transaction 从“pin latest Branch head”开始进入 Lithograph branch commit coordinator，按获得 coordinator 的顺序执行并最终由 SQLite 串行 durable commit。内部 concurrent batches 因此不会互相触发 `BRANCH_HEAD_MOVED`；每个 batch 都从进入自身 transaction 时的最新 Branch head 开始。外部 writer 在某 batch pin head 后移动同一 Branch 时，该 batch 仍按 [Stale Branch Head](#stale-branch-head) 返回 `BRANCH_HEAD_MOVED`。`DISJOINT BY` 等 Cypher 25 semantics 由 executor 保证。

存在 `options.graphView` 时，outer execution 只解析/规范化一次 selector；每个 inner batch transaction 在 pin 自己的 base Commit 后，用同一个 selector 对该 batch 的 graph state 重新计算 visibility。顺序 batch 因而可以按 Cypher 语义观察前一成功 batch 已 durable 的 graph changes；concurrent batch 的 visibility 以其实际 pinned base 与 branch-commit coordinator 顺序为准，不允许复用 outer query 开始时预计算的 element membership。

<a id="integrity-recovery-migration"></a>

## Integrity、Recovery 与 Migration

<a id="integrity-invariants"></a>

### Integrity Invariants

`lithograph_integrity_check()` 至少验证：

- 当前 storage format 的 canonical internal-schema inventory 完整且无额外 reserved object、大小写变体 collision 或未声明的 internal-table child trigger/index；
- Branch ref 指向存在 Commit；
- Tag ref 指向存在 Commit；
- Commit Data row 指向存在 Commit 且 `data_json` 是合法 JSON；
- Merge Session 的 `ours_commit` / `theirs_commit` 存在，session id/revision 合法，resolution row 只引用存在 Session 且 `resolution_json` 符合 public resolution encoding；
- Commit parents、Layer、Schema object 均存在；
- Commit / Layer / Schema hash 可重算且匹配；
- Relationship endpoint 在对应 Snapshot 存在；
- dictionary ID 唯一且 name 唯一；
- checkpoint 与 derived index 声明的 Commit 可解析；
- internal storage format 与 Extension 兼容。

上述完整检查是显式 maintenance/integrity surface，不是每个普通 query、graph/version API、SQL explicit-transaction execution 或 validation call 的隐式前置全库扫描。普通 initialized gate 只执行[加载与初始化](interfaces.md#loading-and-initialization)定义、可在 bounded metadata/schema cost 内完成的结构性校验，包括 metadata marker、storage format、reserved internal-schema inventory、TEMP internal trigger 与 canonical table/index shape；Commit/Layer/Schema hash 重算、完整 DAG/ref/referential/checkpoint consistency 属于显式 `lithograph_integrity_check()`、初始化/迁移验证和 recovery/maintenance gate。需要证明完整 immutable history 未被离线篡改时，调用方必须显式运行 `lithograph_integrity_check()`。该边界不降低 corruption detection：显式 integrity surface 仍执行完整检查，运行时访问自身触及的 canonical object 也继续 fail closed；它只禁止普通 API 每次 invocation 重复扫描全部 canonical history，否则 read/write latency 会随全图/全历史线性放大并违反[Large-scale Invariants](runtime.md#large-scale-invariants) large-scale invariant。

<a id="crash-recovery"></a>

### Crash Recovery

Canonical write 与 Branch move 共用 SQLite transaction，因此 crash 后只允许出现 commit 前状态或 commit 后状态，不存在 Branch 指向半写 Layer 的合法状态。SQLite recovery 完成后 Lithograph 再执行自身 metadata/integrity checks。

SQL explicit transaction 在 `tx_commit` 前没有 public intermediate Commit；process crash 或实际 SQLite connection teardown 会由 SQLite rollback 未提交 transaction，恢复后只能看到 `tx_begin` 前的 base State。`tx_commit` finalize 期间仍服从同一 canonical write + Branch move crash boundary，只允许完整旧 State 或完整新 Commit。Explicit transaction 的 connection-local staged state 不进入 storage format，也不能在 reopen 后恢复为“悬挂 transaction”。

Commit Data set/clear 与 Tag create/move/delete 同样必须是单 SQLite transaction 的原子 sidecar/ref mutation；crash/reopen 后只允许看到操作前或操作后状态，不允许出现半写 JSON、Tag 指向不存在 Commit 或 ref/data 与返回成功状态不一致。

Merge Session start/resolve/abort 每次都是短 SQLite transaction，crash 后只能观察到该 operation 完整发生前或后的一版 session/revision/resolution state。`merge.finalize` 把最终 Commit/ref move（若有）与 Session 删除放在同一 SQLite transaction，因此 crash/reopen 不允许出现“Branch 已移动但 Session 仍可重复 finalize”或“Session 已删除但 Merge Commit/ref move 没有发生”的合法状态。Open Session 本身允许跨 restart 恢复，不需要保持原 connection。

<a id="storage-migration"></a>

### Storage Migration

Storage format version 记录在 `_lithograph_meta`。升级迁移必须：

- 在 SQLite transaction 中执行；
- 保持已存在 Commit ID 和 history semantics；
- 迁移失败整体 rollback；
- 新 Engine 继续读取历史 `format_version`；
- 旧 Engine 遇到更高 format version 直接拒绝写入和读取需要新格式语义的 graph。

首个正式 migration path 是 `1 -> 2`：增加 Commit Data / Tag sidecar 与 Merge Session operational storage，以及对应 integrity / GC semantics；不改写任何既有 Commit、Layer、Schema object 或 hash input。Migration 在一个 SQLite transaction 内创建新 internal objects、把 `storageFormat` 提升到 `2`，失败时整体 rollback。Format `1` database 不存在 Commit Data / Tag / Merge Session，因此迁移不需要为历史 Commit 合成 annotation、ref 或 workspace；升级后三者从空集合开始。

<a id="storage-format-3"></a>

#### Performance Storage Format 3

Format `3` 在 format `2` 基础上，仅为[Persistent Standard Index Base + Delta](schema-and-indexes.md#persistent-standard-index)持久 derived index 增加固定 table/index inventory，不重写 Node/Relationship/Layer 的 canonical 编码。

- Fresh database 的显式 `lithograph_init()` 创建 format `3`；format `2` 的显式 init 原子执行 `2 -> 3`，format `1` 的 init 在同一外层 migration transaction 完成 `1 -> 2 -> 3`。任何一步失败回到原格式及原 schema/metadata，而非留下半升级的 format `2`。
- 保留 `databaseId`、全部旧 Commit ID/各自 `format_version`、Layer hash、Schema hash、parents、Branch/Tag、Commit Data 与 open Merge Session/resolutions。新 Commit 使用新 engine 的 format `3` hash input；旧 Commit 仍按原1/2编码验证，不能全库 rehash。
- 新 Engine 对尚未 init 升级的 format `1/2` 保留既有 legacy read 能力及 TEMP/canonical fallback；任何会持久化 graph/schema/ref/session/cache 的 operation 都先返回 `STORAGE_ERROR` 并提示显式 `lithograph_init()`，不得在旧 schema 写 format `3` Commit。`lithograph_version()` 仍报告实际旧格式与支持范围，`EXPLAIN`/validation 不隐式迁移。所有 legacy 检查使用该版本自己的 exact inventory。
- 旧的 maximum-format-2 Engine 遇到 format `3`，按既有 `FORMAT_TOO_NEW` 合同拒绝 graph read/write，不尝试忽略新 reserved objects 继续工作。没有自动 downgrade；回退需要迁移前的完整数据库备份，或使用新 Engine，不能删 cache 表再修改版本号。
- Migration 只增加空 derived structures 和更新 metadata；已有必须执行的 integrity validation 不被省略，但不把全量 cache build 混入 migration。后续新 index DDL 或显式 rebuild 填充内容；普通只读查询可先走 fallback。
- 初始化/迁移、exact-schema collision/TEMP trigger、reopen、crash rollback、mixed-format Commit DAG、GC 与六平台 interoperability fixtures 全部需要扩展到 format `3`。不能因数据可重建就免除 storage-format 变更的测试。

<a id="storage-format-4"></a>

#### Managed Semantic 不增加 Lithograph Storage Format

Managed Semantic 的 text -> Vector result cache 不属于 Lithograph storage。当前目标设计不定义 format `4`、`_lithograph_embedding_cache`、`semantic.embedding_cache.*` metadata 或任何 Lithograph-owned embedding-cache migration；fresh/current Lithograph database 仍使用 format `3`。

具体 Embedding Provider 可以自行使用独立 SQLite database 或其它机制缓存 `text -> vector` 结果，但该 storage 不属于 Lithograph `main`、reserved namespace、integrity check、Commit/Layer/Schema/history 或 GC。删除/损坏 Provider cache 只能影响性能与外部 Provider 调用成本，不能改变 Lithograph graph correctness。当前优化不承担旧 format `4` database 的兼容、降级或 migration；实现切换到新设计时以最新 format `3` contract 为准。
