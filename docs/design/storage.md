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

`committed_at` 使用 UTC Unix epoch microseconds，在 Commit finalize 时读取 Engine wall clock。它属于 Commit metadata 和 hash input，但不作为 DAG ancestry 或 merge correctness 的依据，也不复用[Temporal Clock Boundary](query-engine.md#temporal-clocks)的 query transaction/statement temporal clock。

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

上图的 `SQLite COMMIT` 对 Engine-owned explicit transaction（Native `lithograph_v1_tx_*` 或 SQL `lithograph_tx_*`）表示 Engine 自己拥有的 transaction commit；对普通 SQL Bridge `lithograph()` 表示内部 SAVEPOINT 成功 release 后，由宿主 SQLite autocommit/outer transaction 决定最终 durability。普通 SQL Bridge 不能从 function callback 提前 commit caller-owned transaction；只有明确的 `lithograph_tx_commit()` 回调可以提交由对应 `lithograph_tx_begin()` 建立的 Engine-owned transaction。

普通 auto-commit execution 中，每个成功的 **graph / Schema / Index mutating query** 都产生一个 Commit，即使 effective delta 为空；这样 graph history 与 write intent 一致。Explicit transaction 改变的是多个 execution 的 Commit boundary，而不是这些 query 的 Cypher mutation semantics，规则见 [Explicit Transaction](#native-explicit-transaction)。Version ref/control procedure 不一概产生 Commit：`branch.create/delete`、`reset` 与 `merge.finalize` 的 fast-forward 结果只原子修改 ref，`branch.checkout` 只修改 connection-local context，`gc` 只做 reachability cleanup，Merge Session 的 start/resolve/abort 只修改 operational workspace；`patch.apply`、`merge.finalize` 的 diverged `merged` 结果与 `revert` 会产生 Commit。

<a id="native-explicit-transaction"></a>

### Explicit Transaction（Native / SQL lifecycle）

Explicit transaction 解决的是 **version atomicity**：调用方可以通过 Native `lithograph_v1_tx_*` 或 SQL `lithograph_tx_*` 把多个独立 Cypher execution 组织成一个逻辑写单元，整个单元成功时只产生一个 Layer、一个 Commit 和一次 Branch head move。两种 adapter 共享同一 connection-local state machine，不建立第二套 staged storage 或 commit coordinator。它不是 Git-style staging area，不暴露 raw Layer/Structural Patch，也不把 Cypher 25 换成 Lithograph-specific mutation language。

逻辑生命周期固定为：

```text
tx_begin(branch?, expectedHead?, author?, message?)
    -> pin base Commit under writer ownership
    -> tx_execute(query A)
    -> tx_execute(query B)
    -> ...
    -> tx_commit()
       -> canonicalize final net delta
       -> write at most one Layer / one Commit
       -> compare-and-move Branch once

or

tx_abort()
    -> discard all staged state
    -> no Commit / no ref move
```

精确语义：

- `tx_begin` 只能在目标 `sqlite3*` 处于 autocommit mode 且没有其它 active Lithograph explicit transaction 时成功；否则返回 `TRANSACTION_BOUNDARY_REQUIRED`。成功 begin 进入 Engine-owned SQLite write transaction / branch-commit coordinator，取得该 database 的 single-writer ownership，并 pin target Branch 当前 head 为 immutable base Commit；
- active explicit transaction 独占该 `sqlite3*` 上的 Lithograph graph/version execution lifecycle：除同一 connection 的 Native / SQL `tx_execute` / `tx_commit` / `tx_abort` 与纯信息 `lithograph_version()` 外，普通 `lithograph_v1_execute` / `lithograph_v1_validate`、SQL Bridge `lithograph()` / `lithograph_rows()`、`lithograph_init()`、`lithograph_integrity_check()` 以及其它会解析/读取/修改 graph/version state 的 operation 都返回 `TRANSACTION_BOUNDARY_REQUIRED`。这样既不能绕过 explicit transaction 形成独立 Commit，也不能通过另一个普通 execution surface 对 staged / committed state 得到含糊解释；
- `expectedHead` 提供时必须与取得 writer ownership 后观察到的 Branch head 相同，否则返回 `BRANCH_HEAD_MOVED` 且不创建 transaction。省略时以实际 pin 到的 head 为 base；
- 每个 `tx_execute` 使用同一 base + transaction-local staged graph/Schema/Index state。后续 execution 必须看到前序 execution 已成功完成的 staged writes；这些 staged state 在 `tx_commit` 前没有 public Commit identity、不会移动 Branch，也不会被其它 connection 读取；
- 每个 `tx_execute` 的 `graphView` 仍是 query-local selector，可以与前一个 execution 不同；visibility 必须针对**当前 transaction staged state**重新计算，因此会观察全部前序成功 `tx_execute` 的 staged writes，但不能看到后序 execution。不能在 `tx_begin` 时把 Graph View 预展开为固定 element-ID membership；
- `tx_execute` 继续执行普通 Cypher immediate semantics：parse/semantic/type、Graph View、Schema/Constraint 与 statement failure behavior 都针对当前 staged state 生效。Explicit transaction **不自动把全部 constraint 变成 deferred constraint**；如果一个 statement 按正常 Cypher/Lithograph semantics 已经非法，它立即失败；
- `tx_execute` 遇到 `LOAD CSV`、transaction-owning subquery、Version Procedure 或其它[Query Options](interfaces.md#query-options)禁止 surface 时在产生对应副作用前返回 `TRANSACTION_BOUNDARY_REQUIRED` 并使 explicit transaction fail-closed abort；
- 任一 `tx_execute` parse/semantic/type/schema/constraint/Graph View/I/O/callback/cancel failure 都使 transaction fail-closed：全部 staged state rollback，transaction 进入 terminal aborted state，不能继续 execute 或 commit；cleanup 自身失败沿用[SQL Bridge](interfaces.md#sql-bridge) `INTERNAL_ERROR` + 丢弃 connection 的故障语义；
- transaction 内新分配的 Node/Relationship identity 可以由该 transaction 后续 execution 通过正常 query result 引用，但在 `tx_commit` 成功前只属于 provisional staged state；abort 后调用方不得把这些 identity 当作 durable element。既有“不复用已提交 identity”合同不因此扩大为“失败 transaction 也永久消耗 identity”；
- `tx_commit` 对 transaction 最终 candidate state 再执行 canonical integrity / Schema / Constraint validation，按 base -> final staged state 计算一个 canonical net delta。只要 transaction 内至少成功执行过一个 graph/Schema/Index mutating query，就创建**恰好一个** Commit；即使最终 net delta 为空，也创建一个 empty-delta Commit 以保留本 transaction 的 write intent。只有 read execution 的 transaction 不创建 Commit 并返回 base Commit；
- 最终 Commit 的 parent 是 `tx_begin` pin 的 base Commit，`author/message` 来自 begin options，`committed_at` 在 Commit finalize 时取得。`tx_commit` 返回最终 Commit identity 与基于最终 canonical net delta 的 counters；Branch 只在 Commit 与 Layer 已成功写入后移动一次；
- begin 已持有 single-writer ownership，正常情况下其它 writer 不能在 transaction 生命周期内移动 Branch；Commit 前仍保留最终 compare-and-move / integrity guard，任何不一致都整体 rollback，不产生 partial Commit；
- explicit transaction 会占用 SQLite 单文件 writer ownership，因此调用方必须保持 transaction 短小，不在其中等待用户输入、长时间网络交互或其它无界外部工作。

`tx_execute` 的逻辑 event 序列仍为 `COLUMNS -> ROW* -> SUMMARY`：Native adapter 流式交给 callback，SQL adapter 把它组装为与 `lithograph()` 相同的完整 envelope。因为 staged state 尚没有 durable Commit，transaction 内 statement 的 `SUMMARY.commit` 固定为 `null`；statement counters 描述该 execution 的 provisional effect。只有 `tx_commit` 返回最终 durable Commit identity 和 transaction-level final-delta counters。

逻辑结果 shape 固定为：`tx_begin -> {"baseCommit":"commit/<id>"}`，其中 `baseCommit` 是实际 pin 的 resolved Commit；`tx_commit -> {"commit":"commit/<id>","counters":{...}}`，纯 read transaction 的 `commit == baseCommit` 且 counters 全为 `0`，存在 mutating execution 时 `commit` 是唯一新建 Commit。`tx_execute` / `tx_commit` / `tx_abort` 在当前 connection 没有 active explicit transaction 时返回 `INVALID_ARGUMENT` + `SQLITE_MISUSE`。

`tx_abort` 显式 rollback Engine-owned SQLite transaction、清除全部 staged state 与 connection-local transaction state，然后返回 `SQLITE_OK`；abort cleanup 失败返回 `INTERNAL_ERROR`，该 connection 必须关闭并丢弃。`tx_commit` 成功或任何 fail-closed abort 后 transaction 都进入 terminal 状态并从 connection 清除，后续必须重新 `tx_begin`。如果宿主没有调用 terminal operation 就真正 teardown SQLite connection，SQLite rollback 是最后的 durability boundary：所有未提交 staged canonical write 必须被撤销，Lithograph 的 connection-registration destructor 同时丢弃 transaction state；reopen 后不得出现该 transaction 的 Commit、Layer 或 Branch move。

<a id="caller-owned-transaction"></a>

### Caller-owned SQLite Transaction

如果调用方已经 `BEGIN` SQLite transaction，同一 connection 内每个 mutating Cypher query 仍产生独立逻辑 Commit 并连续移动 Branch head，但这些 Commit 与 ref move 只有在外层 SQLite `COMMIT` 后才对其它 connection 可见。外层 `ROLLBACK` 会移除这一 transaction 中创建的全部 graph Commits。

因此 caller-owned SQLite transaction 只提供 **durability atomicity**，不等价于 [Explicit Transaction](#native-explicit-transaction) 的 version atomicity。一个 outer SQLite transaction 可以整体回滚 Commit A/B/C，但只要最终 SQLite COMMIT 成功，历史中仍保留 A -> B -> C；需要一个逻辑版本节点时必须使用 SQL / Native explicit transaction，而不是事后隐式 squash。

<a id="stale-branch-head"></a>

### Stale Branch Head

Write 在开始时记录 base Commit，在写 Branch ref 前再次 compare current head。若同一 Branch 已被其它 writer 移动，当前 write 失败为 `BRANCH_HEAD_MOVED`；Lithograph 不自动把两个并发写隐式 merge。

普通 auto-commit write 使用 query-level base；SQL / Native explicit transaction 使用 `tx_begin` pin 的 base / `expectedHead`，并由 [Explicit Transaction](#native-explicit-transaction) 的 writer ownership 把 compare-and-move boundary 扩展到整个 transaction。

<a id="readers-and-writers"></a>

### Readers and Writers

Read query pin immutable Commit，因此不会读取半完成 Layer。SQLite 的 concurrency mode 决定物理锁与 WAL 行为；Lithograph 不增加第二套 lock manager。

不同 connection 可以同时 checkout 不同 Branch。SQLite 仍是单文件 write serialization 的最终仲裁者。

<a id="transaction-subqueries"></a>

### Cypher Transaction Subqueries

Native API 在 caller 没有 active transaction 时实现 `CALL { ... } IN TRANSACTIONS`：每个 batch 对应独立 SQLite transaction 与一个或多个按 query semantics 产生的 graph Commits。

精确规则：每个成功且发生 graph/schema/index mutation 的 batch 创建 **一个** graph Commit；read-only batch 不创建 Commit。某个 batch 失败时仅该 batch transaction rollback；后续 batch 是否继续、状态列和最终 query error 按冻结 Cypher Profile 的 `ON ERROR` / status semantics 执行，因此已经 durable commit 的成功 batch 不被后续独立 batch failure 回滚。

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
- format 4 的 Embedding cache table/index shape 与 operational metadata key 类型合法；cache payload 的 dimension/coordinate/vector encoding 可解析。单个 cache row payload 损坏属于可删除 derived corruption，不升级为 canonical history corruption；reserved table/index shape 被篡改仍是 `STORAGE_ERROR`；
- internal storage format 与 Extension 兼容。

上述完整检查是显式 maintenance/integrity surface，不是每个普通 query、graph/version API、Native `tx_begin` / `tx_execute` 或 validation call 的隐式前置全库扫描。普通 initialized gate 只执行[加载与初始化](interfaces.md#loading-and-initialization)定义、可在 bounded metadata/schema cost 内完成的结构性校验，包括 metadata marker、storage format、reserved internal-schema inventory、TEMP internal trigger 与 canonical table/index shape；Commit/Layer/Schema hash 重算、完整 DAG/ref/referential/checkpoint consistency 属于显式 `lithograph_integrity_check()`、初始化/迁移验证和 recovery/maintenance gate。需要证明完整 immutable history 未被离线篡改时，调用方必须显式运行 `lithograph_integrity_check()`。该边界不降低 corruption detection：显式 integrity surface 仍执行完整检查，运行时访问自身触及的 canonical object 也继续 fail closed；它只禁止普通 API 每次 invocation 重复扫描全部 canonical history，否则 read/write latency 会随全图/全历史线性放大并违反[Large-scale Invariants](runtime.md#large-scale-invariants) large-scale invariant。

<a id="crash-recovery"></a>

### Crash Recovery

Canonical write 与 Branch move 共用 SQLite transaction，因此 crash 后只允许出现 commit 前状态或 commit 后状态，不存在 Branch 指向半写 Layer 的合法状态。SQLite recovery 完成后 Lithograph 再执行自身 metadata/integrity checks。

SQL / Native explicit transaction 在 `tx_commit` 前没有 public intermediate Commit；process crash 或实际 SQLite connection teardown 会由 SQLite rollback 未提交 transaction，恢复后只能看到 `tx_begin` 前的 base State。`tx_commit` finalize 期间仍服从同一 canonical write + Branch move crash boundary，只允许完整旧 State 或完整新 Commit。Explicit transaction 的 connection-local staged state 不进入 storage format，也不能在 reopen 后恢复为“悬挂 transaction”。

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

#### Managed Semantic Storage Format 4

Format `4` 在 format `3` 基础上为[Vector](vector.md) persistent Embedding Result Cache 增加持久布局；这次 migration 只增加可删除 derived cache 与明确允许的 operational metadata key，不改变 Raw Vector Property、HNSW TEMP layout 或 canonical graph/version encoding。

Format `4` 增加一张 `main` reserved table，逻辑字段固定为：

```text
_lithograph_embedding_cache
  entry_id INTEGER PRIMARY KEY AUTOINCREMENT
  space_hash BLOB(32) NOT NULL
  text_hash BLOB(32) NOT NULL
  text_bytes INTEGER NOT NULL
  dimension INTEGER NOT NULL
  coordinate_type INTEGER NOT NULL   -- v1 managed semantic 只写 FLOAT32
  vector_blob BLOB NOT NULL
  payload_bytes INTEGER NOT NULL
  UNIQUE(space_hash, text_hash)
```

`entry_id` 只提供 database-local FIFO eviction 顺序，不参与任何 canonical hash/Schema/history identity，也不得被 API 当成稳定业务 ID。`payload_bytes` 是 capacity accounting 元数据；读取 entry 前仍验证实际 blob/dimension/type，不因 counter 正常就信任损坏 payload。Exact DDL、UNIQUE backing index、reserved inventory 与 migration schema 常量必须由 storage layer 单一来源生成并检查，不运行时按 Provider/Index 名创建表。

`_lithograph_meta` 在 format `4` 额外允许 `semantic.embedding_cache.enabled` 与 `semantic.embedding_cache.max_bytes` operational keys。它们不进入 Commit/Layer/Schema hash，也不随 Branch/Tag/time-travel 改变；`cache.configure` 只修改这两个 key。Fresh/migration 未显式覆盖时使用[Cache policy、清理与运维](vector.md#cache-policy-and-maintenance)定义的当前默认 `enabled=true` / `maxBytes=1_073_741_824`；`cache.stats()` 始终返回 effective values。

- Fresh database 的显式 `lithograph_init()` 创建 format `4`；format `3` 的显式 init 原子执行 `3 -> 4`，format `1/2` 按已有链在同一外层 migration transaction 完成到 `4`。失败保留原格式和原 inventory。
- Migration 只创建空 Embedding cache table/metadata policy，不扫描 graph、不调用 Embedding Provider、不构建 Semantic/HNSW cache，也不修改任何历史 Commit/Schema hash。
- 新 Engine 在尚未显式 init 升级的 format `1/2/3` 上继续按各自 legacy contract 读取已有 Raw Vector/历史数据；创建 Semantic Index、持久化 Embedding cache 或其它需要 format `4` reserved inventory 的 operation 返回 `STORAGE_ERROR` 并要求显式 `lithograph_init()`。普通 Raw Vector read/SEARCH 不因 Managed Semantic 能力而要求 semantic provider。
- maximum-format-3 的旧 Engine 遇到 format `4` 按 `FORMAT_TOO_NEW` fail closed；不能通过删 `_lithograph_embedding_cache` 或手改 metadata 做 downgrade。
- Cache `clear`/FIFO eviction/普通 Semantic query publish/rebuild publish 都在短 SQLite transaction/savepoint 中保持 table 与 metadata 自洽；Provider 调用不在该 write transaction 内执行。SQL scalar 的原 connection 因并发 WAL commit 持有 stale read snapshot 而返回 `SQLITE_BUSY` 时，query publish 可以在同一 `main` file 的短生命周期 sibling connection 重试；该 connection 只执行本 cache transaction，不能承载 Provider、graph read 或 canonical/ref write。crash 或 publish 失败后允许旧完整 cache set 或新完整 batch，不允许半写 vector blob 被标记为可用；这些 derived-cache 写入不参与 Commit/Layer/Schema 或 Branch ref 更新。
- Format 4 的 minimum/current SQLite real-load、migration/reopen/read-only、crash rollback、exact inventory、legacy format 与六平台 artifact acceptance 由 [Managed Semantic 开发计划](../development/phases/13-managed-semantic-vector.md) 验收。Derived data 可重建不等于 storage-format gate 可以省略。
