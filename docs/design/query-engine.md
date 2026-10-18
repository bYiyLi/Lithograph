# Property Graph 与查询引擎

[设计入口](../design.md) · [开发状态与验收](../development/README.md)

本文件拥有图元素和值语义、Frontend/Planner/Executor、Graph View 与 query-scoped 生命周期。公开 options/结果格式见 [接口合同](interfaces.md)，持久编码、事务和 Snapshot resolution 见 [Storage](storage.md)。

<a id="property-graph-and-values"></a>

## Property Graph 与 Value Model

<a id="graph-elements"></a>

### Graph Elements

一个 Snapshot 包含：

```text
Node
  - NodeId
  - 0..N Labels
  - Property map

Relationship
  - RelationshipId
  - Source NodeId
  - exactly 1 Relationship Type
  - Target NodeId
  - Property map
```

允许 self-loop 和同一 `(source, type, target)` 上的 parallel relationships。Relationship 的身份独立于端点与类型；Cypher 中 Relationship 创建后不原地改变端点或类型。

<a id="identity"></a>

### Identity

内部 `NodeId` 和 `RelationshipId` 使用正 `INTEGER64`，在整个 SQLite database 的全部 Branch / Commit 范围内全局分配且从不复用已提交 identity。

外部 `elementId()` 使用稳定字符串：

```text
n:<decimal-node-id>
r:<decimal-relationship-id>
```

Branch 与 Time-travel 不改变同一历史 element 的 `elementId`。

Label、Relationship Type 与 Property Key 使用 append-only integer dictionary encoding。Dictionary 中存在一个名称不代表该名称在所有 Snapshot 中可见；可见性由 Snapshot 数据决定。

<a id="runtime-values-and-properties"></a>

### Runtime Values 与 Persistent Properties

Query runtime 使用完整 Cypher 25 value model，包括 `NULL`、Boolean、Integer、Float、String、List、Map、Node、Relationship、Path、Temporal、Duration、Point、Vector 与 UUID。

Value comparison 固定遵循 `CY25-2026.08` current semantics，而不是旧 openCypher TCK 的历史 cross-type 规则：

- `=` / `<>` 对 `null` 传播 `null`；Integer/Float 按 numeric value 比较；Path 可以按交替 Node/Relationship sequence 与等价 List 比较；其余不具 equality-comparability 的不同 value family 必须返回稳定 `TYPE_ERROR`，不能静默降为 `false`；
- `<` / `<=` / `>` / `>=` 对 `null` 传播 `null`。同 family 使用该 family 的 comparison semantics；不同 family 使用 Cypher 25 value hierarchy，顺序为 `MAP < NODE < RELATIONSHIP < LIST < PATH < VECTOR < POINT < ZONED DATETIME < LOCAL DATETIME < DATE < ZONED TIME < LOCAL TIME < DURATION < STRING < BOOLEAN < UUID < NUMBER`；同 family 明确定义为 non-comparable 的 value（例如 Duration direct comparison，以及 Profile 中对应的 Point/Vector direct comparison）返回 `null`，不能借用 `ORDER BY` 的内部 total-order key 改变 predicate 结果；
- List direct comparison 使用 lexicographic semantics，并保留 `null` element 导致的 unknown result；`ORDER BY` 的 total ordering 与 direct comparison 是两个独立 contract；
- `STARTS WITH` / `ENDS WITH` / `CONTAINS` / regex 等 String predicate 对 `null` 或非-String input 返回 `null`；需要 String-only access path 时由 planner/type proof 决定是否安全下推，不能把非-String value 改成 query error。

持久化 Property 只接受 Cypher 25 允许的 property value types。Map、Node、Relationship、Path 等 constructed/structural runtime value 不作为 Property value 持久化。Property legality 在 Cypher type/mutation boundary 验证；LCE1 codec 负责 storage-format bytes 的 canonical encode/decode，不作为 Cypher Property 语义验证器，因此 format 1 已冻结的 tagged List bytes 不能因上层 property-type 规则而改变。

`SET n.key = null` 与对应 Relationship 操作表示删除该 Property，而不是持久化一个 `NULL` property slot。

Vector 按原始 coordinate type 与 dimension 保存，不用 JSON list 替代，因此整数宽度、浮点精度与 dimension 可被 Schema 和 Search 正确验证。

<a id="engine-structure"></a>

## Engine Structure

```text
Cypher text + parameters              execution options
          |                                  |
          v                                  v
+---------------- Frontend ----------------+
| Lexer / Parser -> AST -> Semantic Analyze |
| Scope -> Type -> Schema validation        |
+--------------------|----------------------+
                     |             +-------------------------+
                     |             | Execution Context       |
                     |             | - version context       |
                     |             | - graphView             |
                     |             +------------|------------+
                     |                          |
                     +-------------+------------+
                                   v
              Logical Planner
                                   |
                                   v
              Physical Planner
                                   |
                                   v
+---------------- Executor -----------------+
| row pipeline / path operators / writes    |
| subqueries / aggregation / eager barriers |
+----------|-------------|------------------+
           |             |
           v             v
      Storage API    Search API
           |             |
           +------|------+
                  v
         Version/Snapshot Resolver
                  |
                  v
                SQLite
```

Frontend 的 AST 与 Cypher semantic model 是 Lithograph 自己的内部合同。可以参考或依法复用 GraphQLite/openCypher grammar，但 GraphQLite 的 parser AST、SQL transformer 或 clause dispatcher 不作为 Lithograph core interface。

静态标识符的 AST 保留 source text 和 position；名称进入 planner/executor 的执行结构时统一解码一次：去掉外围反引号，将双反引号还原为名称内的单个反引号。Node Label、Relationship Type、Label predicate、Schema/Index 和 mutation 共享实际名称语义，不按中文或英文特判。普通形式与合法反引号形式必须指向同一名称；动态 Label/Type 表达式已经返回实际 String，不再进行静态标识符解码。

<a id="planning-and-execution"></a>

## Query Planning 与 Execution

<a id="logical-operators"></a>

### Logical Operators

Planner 至少覆盖以下 operator families：

- scan / seek：Node、Label、Relationship Type、Property Index、Full-text、Vector；
- graph expansion：`ExpandAll`、`ExpandInto`、variable/quantified expansion、shortest/path-selector operators；
- row operations：Filter、Project、Let、Unwind/For、Aggregate、Distinct、Sort、Skip、Limit；
- composition：Union、Apply、Optional、Semi/Anti、Cartesian、Subquery、When、Next；
- write：Create Node/Relationship、Set/Remove、Delete/Detach、Merge、Schema/Index changes；
- control：Eager/materialization barrier、transaction batch boundary、profile instrumentation。

<a id="physical-planning"></a>

### Physical Planning

Physical Planner 使用规则重写与 cost estimate 联合选择访问路径。统计信息至少包括：

- Node / Relationship 总量；
- label / type cardinality；
- label/type 组合 cardinality；
- property index distinct/cardinality；
- relationship degree distribution；
- Search index metadata。

统计属于 derived data，不进入 Commit / Layer identity，也不成为查询 correctness 的真源。Checkpoint 可以把与该 Snapshot 对应的 versioned statistics 放在 checkpoint metadata 中；Planner 读取最近 checkpoint 的统计，再只按 checkpoint -> target Commit 的 bounded overlay 变化做增量修正。缺失、损坏或版本未知的统计必须退化为保守的 unknown estimate，而不是为每个 query 重新扫描完整 Snapshot 计算 cardinality；estimate 缺失只能影响 plan quality，不能改变结果。

Planner 可以把 filter、projection、typed comparison 与部分 index seek 下推给 SQLite，但不能为了 SQL 转译便利改变 Cypher semantics。复杂 path、merge、version snapshot 与 semantic barriers 由 Lithograph executor 原生执行。

<a id="streaming"></a>

### Streaming

Executor 使用 **pull-based row pipeline**。Owning adapter 每次请求下一 event/batch 时，executor 只推进产生该输出所需的最小后续工作；adapter 停止消费、interrupt 或 drop 后，不在后台继续把剩余 query 跑完。没有 semantic materialization 要求时，执行状态只保留 operator continuation、bounded input/output batch 与必要的 graph/index cursor，不随 upstream 或最终 result row count 线性增长。

该 invariant 适用于所有 result-producing path，而不只基础 `MATCH` scan：Union/Apply/Optional/Cartesian/Subquery/When/Next 等 composition、procedure CALL/YIELD、LOAD CSV、path expansion、read-write clause pipeline、Schema/Version procedure result，以及 transaction program 都必须以 continuation/state machine 推进。实现不得先调用一个返回完整 `Vec<Row>` / `RowSet` 的 program executor，再把该集合切成小 batch 冒充 streaming；如果某个 legacy operator 只能返回完整集合，必须改造成 incremental producer 或显式归类为下一段定义的 semantic barrier。

`ORDER BY`、global aggregation、`DISTINCT`、必要的 eager write barrier，以及 [Cypher Transaction Subqueries](storage.md#transaction-subqueries) 中需要先知道单个 inner batch最终 transaction/error outcome的 result barrier 等确实需要 materialization 时，可以使用 bounded memory并在超过内部预算后 spill 到 SQLite TEMP/disk。Barrier只允许保留其语义所需的 state；即使单个 barrier输入/输出很大，也不能退化成与全部 rows 同阶的 retained RAM。Barrier完成后结果继续作为 incremental downstream producer drain，不能再复制成第二份完整 row collection。

`lithograph_rows()` 直接暴露该 pull pipeline；`lithograph()` 也驱动同一 pipeline，只允许其 SQL scalar adapter collector在 Core 之外为了构造单个 JSON envelope收集最终 rows。因此 scalar可能因完整结果产生 O(result rows) adapter memory，而同 query 的 Core/streaming execution不能据此保留 O(result rows) internal collection。first-row/first-event、完整消费和 retained-memory证据按 [Performance Evidence Contract](runtime.md#performance-evidence) 分层测量。

<a id="snapshot-pinning"></a>

### Snapshot Pinning

普通 query 在执行开始时解析 Branch / Commit 并 pin 到一个 immutable Commit。Branch head 在 query 执行期间发生变化不会改变该 query 的 base Snapshot；同一 query 内前序 write clause 产生的 staged changes 仍按 Cypher clause-composition semantics 对后序 clause 可见。

`CALL { ... } IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 是例外：它们按[Cypher Transaction Subqueries](storage.md#transaction-subqueries)让每个 inner batch transaction 各自 pin 对应的 Branch head，而不是让整个 outer query 共用一个 immutable Commit。query-level `graphView` selector 在这些 batch 间保持不变，但 visibility 必须基于各 batch 自己的 pinned Snapshot 与该 batch 内已经完成的 staged clause writes 计算。

<a id="temporal-clocks"></a>

### Temporal Clock Boundary

普通 auto-commit execution 下，一次 top-level Lithograph execution 同时是 Cypher temporal clock 的 transaction boundary 和 statement boundary；`date/time/localtime/localdatetime/datetime.transaction()` 与 `.statement()` 都在 execution 开始时取值，因此两者相等且在所有 cursor batch 中稳定。SQL explicit transaction 下，`.transaction()` 在 `lithograph_tx_begin()` 成功时固定，并在 active transaction 内全部 `lithograph()` / `lithograph_rows()` execution 中保持同一 instant；`.statement()` 则在每次 execution 开始时重新取得并在该 execution 内稳定。`.realtime()` 每次求值读取 wall clock，不保证稳定。timezone 参数只改变同一 instant 的本地表示，不改变 clock identity。

caller-owned outer SQLite transaction 可以把多个普通 Lithograph execution 的 durability 合并到一次 `COMMIT` / `ROLLBACK`，但不把这些 execution 合并成一个 Cypher transaction 或一个 Lithograph Commit；每次 invocation 仍取得自己的 transaction/statement clock。[Cypher Transaction Subqueries](storage.md#transaction-subqueries)的 transaction subquery 由每个 inner batch transaction 各自取得 transaction clock。

<a id="cancellation-and-connection-state"></a>

### Cancellation 与 Connection State

Executor 在 batch/operator boundary 与长路径/搜索循环中检查 SQLite interrupt state；host 调用 `sqlite3_interrupt()` 后 query 尽快停止并返回 `SQLITE_INTERRUPT`。`lithograph_rows()` 的 SQL cursor 在 `xClose`、outer SQL early-stop 或 connection teardown 时也必须取消未完成 execution；尚未 durable 的普通 write rollback，active explicit transaction 则按 fail-closed contract 整体 abort。真实 SQLite interrupt 的恢复、实际 transaction 状态确认与 caller-owned whole-transaction rollback 边界见 [Runtime](runtime.md#sqlite-interrupt-recovery)。

中断回调必须传入表达式和函数求值，覆盖 `range()`、列表推导/谓词、`reduce()` / `allReduce()`、flatten、集合/长值遍历及字符串构造，不能只在两条结果行之间检查。表达式求值与循环累计工作量，每至多 256 个工作单元检查一次；可分块的长文本处理每至多 64 KiB 检查一次。大规模分配、复制和不可拆分的第三方调用前后另行检查。第三方调用或单次系统调用进行中不保证异步抢占，其已有 timeout/取消合同仍生效；不新增 query timeout option。取消返回现有 `INTERRUPTED` / `SQLITE_INTERRUPT`，并复用失败清理和 transaction-subquery 已 durable batch 的保留语义。

Active Branch、explicit transaction state、temporary query options、prepared-plan/cache handle、current error/cancellation state 全部属于单个 `sqlite3*` connection 或单个 query。禁止使用 process-global mutable query/branch/parser/transaction state。跨线程使用同一 `sqlite3*` 是否允许完全遵循 host SQLite threading mode；Lithograph 不为一个不允许并发使用的 connection 增加第二套线程安全保证。

所有 application-facing execution 都发生在 host SQLite SQL callback / virtual-table cursor lifecycle 中，connection serialization 直接服从 host SQLite threading mode 与其 `sqlite3*` 使用规则；Lithograph 不另建 process-global mutex 扩大 SQLite 自己的线程安全承诺。`sqlite3_get_clientdata()` / `sqlite3_set_clientdata()` 只负责 Embedding Provider 与 connection-local state ownership/lifetime，不单独承担 invocation serialization。

<a id="execution-resources"></a>

### Execution Resource State

每次 execution 由其 cursor 持有内部资源状态，基础执行、复杂 program、子查询、分支、mutation 和 concurrent batch 共享同一次 execution 的内存预算；不同 execution 独立。各次推进或 worker 累计自己的中断检查工作量，保持每条运行路径的检查间隔有界。每次推进借用宿主中断回调，不保存失效 callback，不使用进程级可变 query 状态。execution option 和错误形状由 [Interfaces](interfaces.md#query-options) 拥有，预算的外部边界由 [Runtime](runtime.md#execution-memory-boundary) 拥有。

在参数解码前解析 options 并建立预算，参数解码的临时对象与生成的 Cypher Value 都计账。受控分配在分配/扩容前预留容量，计算使用 checked arithmetic，溢出按资源拒绝处理；使用拥有生命周期的预留对象，在释放时归还，移动转移预留，复制新增预留。需要同时存活的输入、输出和序列化副本同时计账，不能在创建输出副本前提前释放仍存活的输入费用。

计账覆盖执行阶段参数、表达式中间值、List/Map/String/Vector/图值、row/batch、排序/聚合/去重工作缓冲、公开结果编码和 scalar collector，按容器容量及必要的保守开销计算。`range()` 先检查元素数和容量再分配，并保留原有 200 万元素上限；不能只优化 `size(range(...))` 替代通用预算。semantic barrier 在达到字节预算前向既有 TEMP spill 路径 flush，无法容纳的单值或必要工作状态明确失败；结果释放时归还预算，streaming 不能按累计消费行数永久扣减。取消、错误、early-close 和成功结束都释放预留与 TEMP 状态；不改变既有 finalize-success、回滚或 durable batch 边界。

<a id="graph-view"></a>

### Graph View Execution Boundary

Graph View 是 Lithograph Execution API 的 query-local visibility / mutation boundary。它解决调用方需要在同一个 versioned Property Graph 内把不同逻辑数据空间交给完整 Cypher 执行、又不能依赖 query rewrite 或 result post-filter 的问题。

Graph View 的 **selector specification 在一次 execution 内固定，element membership 不固定为 query-start 的 ID 集合**。Visibility 是一个作用于当前 Cypher graph state 的规则：read-only query 的 graph state 就是 pinned Snapshot；read-write query 中，每个 clause 读取前一 clause 输出的 graph state，因此必须观察全部前序 clause 已完成的 writes，同时不能观察后序 clause 的 writes。

普通 query 的执行关系是：

```text
normalize graphView selector
          ↓
Branch / Commit resolution
          ↓
pin immutable Snapshot
          ↓
current clause graph state
  = base Snapshot
    + completed prior-clause staged writes
          ↓
evaluate Graph View visibility
          ↓
visible Property Subgraph for this clause
          ↓
Executor / Search
```

Planner 持有规范化的 selector / execution context，不把 Graph View 预展开成固定 element-ID allowlist；Physical operator 在对应 clause 的实际 graph state 上执行 visibility check。Graph View v1 使用[Query Options](interfaces.md#query-options)的 Label selector：Node 在当前 clause graph state 中满足全部 `requireAllLabels` 且不命中任何 `excludeAnyLabels` 时可见；Relationship 当且仅当两个端点都可见时可见。Graph View v1 自身是 element-level visibility；属性可见性另由 [Access Control Execution Boundary](#query-access-controls) 约束，未配置 query 属性规则时，一个 element 的 Property 按正常 Cypher 语义可见。NodeId / RelationshipId、Label、Type、Property 与 Schema 本身不因 view 改写或复制。

所有会观察 graph data 的 execution path 必须把该可见子图当作本次 Cypher 的输入图，而不是执行完成后再过滤结果。至少包括：

- Node/Label/Relationship scan 与 index seek；
- `ExpandAll` / `ExpandInto`、variable/quantified path、shortest/path selector；
- `MATCH` / `OPTIONAL MATCH`、subquery、aggregation、`count()`、`DISTINCT` 与 cardinality；
- element reference dereference 与基于 graph element 的 function/procedure；
- full-text、vector、`SEARCH` 及其它返回 graph element 的 Search path。

因此隐藏 Node 不参与 aggregation/cardinality，隐藏 Relationship 不能作为 path 的中间边，Search 的 `skip` / `limit` / top-k 不能先让隐藏结果占用名额后再做 post-filter。Index/Search 可以在物理层产生更宽的 candidate set，但在任何 Cypher-visible semantic step 前必须执行 Graph View visibility check。

Graph View 不要求把整个子图预先 materialize。Planner / Storage 应利用现有 Label membership 与 seek/adjacency 结构按需检查 visibility；view 本身不建立第二套 canonical storage 或 per-view derived history。

Graph-data mutation 在同一个 boundary 内执行：

- 一个 mutating clause 开始时，既有 Node / Relationship 必须在该 clause 的 input graph state 中可见才能作为 mutation target；不可见既有 element 等价于本 clause 不存在，不能通过 element reference、`MERGE` 或其它 operator 绕过；
- 一个 mutating clause 的全部 Cypher-defined effects 完成后（包括 `MERGE` 的 `ON CREATE` / `ON MATCH` effects），该 clause 新建且仍存在的 Node 必须满足当前 Graph View；新建 Relationship 的两个端点必须可见。Clause 内部尚未对后续 clause 暴露的临时执行步骤不单独形成 visibility boundary；
- `SET` / `REMOVE` Label 不允许一个 clause 在完成后把其修改且仍存在的 Node 留在当前 Graph View 外；这种 write 返回 `GRAPH_VIEW_VIOLATION`，整个 top-level mutating query 按[Transaction 与 Concurrency Model](storage.md#transactions-and-concurrency)回滚；因此在要求 Label `A` 的 view 中，`CREATE (n) SET n:A` 会在 `CREATE` clause 完成时失败，而 `CREATE (n:A)` 可以成功并被后续 clause 观察；
- 删除可见 Node 时，如果保持 referential integrity 必须同时修改一个不可见 Relationship，则该 delete / detach delete 返回 `GRAPH_VIEW_VIOLATION`，不得跨 view 隐式删除；
- graph-data mutation 不因 Graph View 改变 Commit 粒度、Branch compare-and-move 或 caller-owned transaction 语义。

Graph View **不投影 Schema / Constraint / Index definition**。当前 Commit 的完整 versioned Schema 仍是该 execution 的唯一 Schema；Graph Type、property type、KEY / UNIQUE / existence 等 validation 按原 Cypher/Lithograph contract 对 mutation 的完整 candidate canonical graph state 执行，包括 Graph View 外的 element。`MERGE` 的 match 部分只看到 view 内 element；如果它因此尝试创建一个与 view 外 element 冲突的 UNIQUE / KEY value，最终返回正常的 `CONSTRAINT_ERROR` 并回滚，而不是把 constraint 解释成 view-local constraint。由这种 validation 产生的间接存在性信号不违反 Graph View contract，因为 Graph View 明确不是 authorization boundary。

Graph View 是执行语义，不是认证或权限系统。持有原始 Lithograph execution surface 的调用方可以省略 `graphView` 访问完整 graph；上层产品若把它用于租户或内部数据隔离，必须控制调用方能提交的 options。Lithograph 只保证在**已经选择的** Graph View 内没有 query/operator/Search/write bypass。

<a id="query-access-controls"></a>

### Access Control Execution Boundary

访问配置与错误编码由 [接口合同](interfaces.md#query-access-controls) 拥有。本边界在正式 AST、query-local execution context 和对外结果编码中落实，不使用 query text 正则识别 Cypher，不新增 parser bridge，也不改变 Cypher grammar、storage format、Commit identity 或 Provider SPI。

一次 execution 共享规范化的访问规则和已编译 matcher；PreparedQuery、嵌套子查询、字符串插值、transaction batch 与 concurrent worker 传递同一策略，不使用 process-global mutable policy。省略或空配置使用现有执行路径，不为无规则执行建立全图拷贝或逐返回 batch 重新解析。

`readAccess.query` 是图属性的逻辑可见性：Node/Relationship 的受限属性在每个 clause 实际 graph state 中按不存在处理。静态和动态属性访问、pattern property predicate、`keys()`、`properties()`、Map projection、Path 中的 graph object、条件、排序、聚合及嵌套表达式都使用该视图。普通 Map/scalar 保留正常语义。物化出的图对象不含受限属性；Canonical Snapshot、内部完整性与 Schema/Constraint validation 始终读取真实状态，不把 masking 后的值作为 storage correctness source。

Planner 不能用隐藏属性的原始 index seek/ordering/statistics 产生未受限的匹配结果。必要时退回受限求值，未受限字段继续复用正常索引。Full-text/Vector/Managed Semantic Search 依赖隐藏属性时返回 `ACCESS_DENIED`，不能通过命中、排序、score 或枚举泄露。图数据 Procedure 适用同一 query 规则；必须完整返回 Patch/Diff/conflict data 的版本读取若包含被 query 规则禁止的属性，返回 `ACCESS_DENIED`，不伪造删减后仍声称完整的版本对象。Schema、Commit Data 等 metadata 不按 key 伪装成 graph property 过滤。

`readAccess.result` 在 scalar/rows 共用的最终公开 JSON 编码边界递归过滤图对象属性。List、普通 Map 中的 Node/Relationship 和 Path 的元素同样受控；普通 Map 自身的 key、alias 和已提取 scalar 不变。因此 result-only 模式允许显式取值，`properties(n)`、`n{.*}` 产生的普通 Map 也保留真实字段。内部 spill、排序、去重与表达式 value encoding 不受结果规则影响。可检测的过滤/编码失败必须发生在既有最终成功/发布边界之前，保持普通 invocation 的 rollback 与 staged transaction abort 合同。

`writeAccess` 在实际属性 mutation 前检查；显式向禁止属性写入相同值也必须拒绝，不能被 no-op 优化跳过。Map 替换先检查隐含删除，Map 合并和动态 key/null 移除逐目标检查。创建、删除、Label、Schema、版本和维护权限独立叠加；允许删除对象时，对象删除带来的属性清理不被误判为单独的属性修改。

Patch、Revert、Reset、Merge、Rebase、Squash 按操作前后目标 Branch 的有效 graph/Schema 变化检查创建、删除、Label、Schema 和受保护属性。属性最终状态的相同/不同沿用 [Storage 的 canonical value encoding](storage.md#canonical-value-encoding) 和 [版本 logical-slot 合同](versioning.md#diff-and-patch)，不以 Cypher 数值相等或宿主浮点比较替代。历史重组的内部 Layer replay 不单独作为受保护属性写入：Rebase/Squash 的最终有效属性未改变时允许，实际最终值改变则拒绝。复用已有 Layer/Delta/touched diff 路径，不引入第二套 storage model 或每查询全图复制。Merge 的显式 candidate 修改受本次权限约束；`finalize` 必须用本次 execution 的策略重新验证 candidate、fast-forward 与发布效果，不能信任 Session 创建或上一次 inspection 的权限。拒绝不能留下新 Commit、Branch move、Schema 或半完成 Session mutation。

读取能力开关与 `denyFunctions` 在完整、已完成正常 semantic validation 的 AST 上检查，包含所有嵌套结构和字符串插值；Schema command 与已知 Procedure 的固定能力效果也在准备阶段分类。对这些准备阶段禁令，未运行的 branch、零结果与 `EXPLAIN`/`PROFILE` 不豁免。普通图写入的 `allowCreate` / `allowDelete` / `allowLabelChanges` 则检查实际发生的效果：例如 `MERGE` 命中既有对象而未创建时不因 allowCreate=false 拒绝，零行输入未产生创建也不报创建禁令；对真实目标显式写回同一保护属性值仍按属性禁令拒绝。命名 Procedure 按真实 graph/Schema/version/maintenance/external-I/O 效果分类；`allowExternalIO=false` 必须在文件/HTTP、Provider 或宿主 tokenizer 被调用前拒绝，即使 Provider 可能 cache hit。当前 SQLite graph 读取、TEMP/spill 和不调用外部能力的 derived work 不受 external-I/O 禁令影响。

普通 execution、explicit staged execution、独立与 concurrent batch 使用同一语义。每次 explicit execution 的初始 staged state 是本次写入校验基线，后续策略不追溯否定前次已授权变更。transaction subquery 按现有 `ON ERROR` 和独立 commit 合同处理拒绝：失败 batch 不发布，已 durable 的 earlier batch 保留，结果失败或取消不能伪装成整体 rollback。只读 Snapshot、Graph View 和旧 `readOnlyNoExternalIO` 始终取权限交集。

<a id="query-scoped-resolved-state"></a>

### Query-scoped Resolved State

**目标：一次只读 execution 的同一 graph state 只解析一次，不能按返回 batch 重放 lineage / Layer。** 这包含 prepare/planner 与 executor 的共享，不只是把每批重复解析改成两处独立解析。`EXPLAIN` 是另一 execution；不得让 benchmark 的预先 EXPLAIN 悄悄预热随后被计时的 execution。

实现把不借用 SQLite connection 的 resolved state（pinned Commit、checkpoint identity、immutable overlay、versioned Schema、statistics）与短生命周期 storage accessor 分开。QueryCursor 持有前者，各次访问只临时绑定原 connection；不得通过延长 Rust borrow 到 `'static`、悬挂 pointer 或复制整个 overlay 来绕过生命周期。内部 read context 可以调整，但 `lithograph()` envelope、`lithograph_rows()` event stream 与 SQLite error contract 必须保持设计定义的可观察语义。

生命周期固定为：

```text
parse / classify
 -> establish main-database read guard
 -> resolve version + Snapshot + Schema once
 -> plan / execute / consume batches against that state
 -> EOF | LIMIT | cancel | error | drop
 -> release owned buffers / statements / read guard
```

Read guard 必须从 version resolution 前持续到 cursor 结束，保护 checkpoint、index generation 和 canonical rows 在查询期间的 SQLite read view；只保存 Commit ID 不足以抵抗其它 connection 的 GC/cache eviction。SQL execution surface 借用宿主 transaction，并由 adapter 保持一个真正读取 `main` 的 SQLite statement cursor，直到 execution 释放；不能靠各 batch 内独立 SELECT 的短 implicit transaction。Guard 仅持有 read view，不取得 writer、不建立第二 connection、不创建持久 pin registry；不依赖要求特殊编译选项的 `sqlite3_snapshot_*`。Standalone Core caller 同样必须提供跨 prepare/consume 的 read context，不能绕过此保护。

Guard 的 statement handle 由 owning adapter 的 RAII boundary 管理，不能被 Core 当作可跨 connection 使用的缓存。`xFilter` 重扫先释放旧 execution；SQL 外层提前停止、`xClose`、interrupt、panic、连接 teardown 和 prepare failure 都必须释放对应资源。读结束只释放本次拥有的 statement/context，不 `COMMIT`、`ROLLBACK` 或 `RELEASE` caller-owned transaction。多个纯 read cursor 可以共存；同 connection 有 active read cursor 时，另一个 Lithograph mutation、GC 或 persistent-index maintenance 在产生副作用前返回 `TRANSACTION_BOUNDARY_REQUIRED`，避免自己删除/改写尚在使用的 rows。反过来，一个 side-effecting streaming execution 已经进入 runtime write/maintenance boundary 时，同 connection 不再启动另一个 graph/version execution cursor，直到前者 terminal；这避免 nested SAVEPOINT / staged state 让一个 execution 的 success 被另一个 execution 的后续 rollback 撤销。宿主不得在 active execution 中通过 raw SQLite 改写 internal tables、重置同一 connection 的 transaction 或执行 schema replacement；不通过安装全局 authorizer 扩大对宿主的控制。

WAL 下另一 connection 可以继续提交，读者保持旧 read view；rollback-journal 下沿用 SQLite 的 reader/writer 锁规则，不宣称所有模式都不阻塞 writer。长读可能延迟 SQLite WAL checkpoint，应测量 WAL 增长并通过及时关闭 cursor 释放，不偷偷切换到新 Snapshot。这里的 SQLite WAL checkpoint 与 Lithograph graph checkpoint 是两种不同资源。

Write / candidate state 不复用过期的 read membership：

- 普通 read-write execution 保留 immutable base，已完成 clause 的 staged mutation 以单调 state revision 更新；后续 clause 的 accessor、visibility 和 index overlay 必须读取新 revision。
- SQL explicit transaction 内每次 `lithograph()` / `lithograph_rows()` execution 有自己的 statement state，观察前序成功 execution 的 staged changes；最终 commit/abort 清理所有 staged cache。不得把 transaction 起点的 membership 当成整个 transaction 不变的集合。
- Merge candidate 绑定 `(session, revision, candidate identity)`；一次 inspection 在同一 read view 验证 revision 并执行，不能跨 revision 复用。`IN TRANSACTIONS` 仍按[Cypher Transaction Subqueries](storage.md#transaction-subqueries)每个 inner transaction 独立 pin，不跨 batch transaction 复用旧 state。

Storage page 与输出 batch 分离：改变调用方的 `max_rows` 不应导致重复解析或重新执行已消费的查询。复用 prepared storage statements、按需读取 properties，并用 bounded ordered merge 代替每页不必要的 map/set 重建；不以增大 batch 隐藏每 batch 重复工作，也不把整图预装内存。

Graph View 优化仅消除有证据的重复检查：对同一合法 Snapshot access path 已证明存在且可见的起点复用 proof；终点按需/分批检查。空 selector 可以省略 Label predicate，但不能把任意外部 Node/Relationship reference 直接认作有效，也不能关闭 canonical integrity 或触及损坏记录时的 fail-closed 检查。可见性缓存限于 query，key 至少包含 state identity/revision、规范化 selector 与 NodeId，采用固定预算、可逐出；revision 改变必须失效，不能随遍历过的所有 Node 无界增长。

紧凑 row/slot 化只在邻接 keyset、query-scoped resolved state 与 persistent Standard Index base/delta 优化完成后的 profiling 仍证明 allocation、clone 或 string lookup 为热点时实施。优先复用/移动既有 binding，限制在被测 read pipeline；保留变量作用域、重复列名/行、OPTIONAL null、path relationship uniqueness、错误与时钟语义。不预先新增 JIT、并行 executor 或第二套 query engine。Planner 复用已有 `optimize_node_scan` 的最低 cardinality 选择；只修实测的路径覆盖缺口，不把已有能力重新实现一次。
