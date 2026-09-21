# SQLite 接口、外部 I/O 与结果合同

[设计入口](../design.md) · [开发状态与验收](../development/README.md)

本文件拥有 application-facing SQLite SQL execution surface、options、外部 I/O、结果编码与错误映射。事务状态机由 [Storage](storage.md#transactions-and-concurrency) 定义；Procedure 的专用参数分别由 [Versioning](versioning.md)、[Standard Index maintenance](schema-and-indexes.md#build-publish-maintenance)、[Full-text](full-text.md)、[Vector](vector.md) 定义，不在此复制。Embedding Provider 的 C ABI 由 [Vector](vector.md#embedding-provider) 拥有，它是 extension-to-extension SPI，不是 application query API。

<a id="sqlite-extension"></a>

## SQLite Extension 接口

<a id="loading-and-initialization"></a>

### 加载与初始化

发行物是普通 shared library：

```text
macOS   lithograph.dylib
Linux   lithograph.so
Windows lithograph.dll
```

标准加载：

```sql
.load ./lithograph
SELECT lithograph_init();
```

SQLite CLI 的 `.load` 已处理 extension loading。其它 host application 必须按 SQLite 官方接口在目标 connection 上启用并调用 `sqlite3_load_extension()`（或等价 binding API）；Lithograph 不要求也不尝试全局打开任意 extension loading。

Lithograph v1 的 canonical graph storage 固定属于目标 connection 的 SQLite `main` database。即使该 connection 存在 TEMP object 或 `ATTACH` 的其它 database，所有 `_lithograph_*` canonical object 的检查、创建和读写都显式限定 `main`，TEMP / attached 同名 object 不得 shadow Lithograph storage。若要把另一个 SQLite 文件作为独立 Lithograph database 使用，应让该文件作为另一个 connection 的 `main` 打开；v1 不通过同一 connection 的 attached schema 承载第二个 graph repository。

`_lithograph_` internal namespace 的保留遵循 SQLite identifier 的大小写不敏感语义：`_lithograph_*`、`_LITHOGRAPH_*` 等大小写变体属于同一 reserved namespace，不能借大小写绕过 collision/integrity 检查。每个 storage format 都定义 exact canonical internal-schema inventory；除该 format 明确定义的 table/index/trigger 等 schema object 外，`main` reserved namespace 中的额外 object，以及挂接在 canonical internal table 上但不属于该 format 的 trigger/index，均视为内部结构损坏。普通 TEMP 同名 table/view 仍不得 shadow `main` canonical storage；但 SQLite 允许 connection-local TEMP trigger 挂接到非 TEMP table，因此在当前 Extension 可理解的 initialized format 上，任何 target table 名落入 reserved namespace 的 TEMP trigger 都视为 unsafe connection state 并由 integrity/version/init 拒绝，避免 TEMP trigger 截获 canonical internal write。更高且当前无法理解的 storage format 仍优先保持前述 version-discovery / `FORMAT_TOO_NEW` contract。

`.load` 只注册 Extension API，不修改数据库内容。`lithograph_init()` 在当前 database 内原子创建或迁移 `_lithograph_*` 内部结构，并创建表示空图的 Root Commit 与默认 `main` Branch。

首次初始化生成一个 RFC 9562 UUID 作为 `databaseId`，保存在 `_lithograph_meta`，在该 database 的整个生命周期和 storage migration 中保持不变。Storage format `1` 是首个 canonical graph-storage baseline；format `2` 增加 Commit Data / Tag sidecar 与 Merge Session operational storage。支持到 format `2` 的 Engine 对 fresh database 直接创建 format `2`；已存在 format `1` database 只能通过[Storage Migration](storage.md#storage-migration)定义的显式 `1 -> 2` migration 升级，既有 Commit ID 不重算。

在 format `2` 基础上，持久 derived Standard Index 使用 format `3`，其 fresh/init、旧格式读取、exact internal-schema inventory 与迁移边界由[Performance Storage Format 3](storage.md#storage-format-3)统一定义。Managed Semantic 的 text -> Vector result cache 由具体 Embedding Provider 自己拥有，不增加 Lithograph main database 的 reserved table、operational metadata 或 storage format；当前目标 storage format 因此仍为 format `3`。Embedding Provider contract 是独立的 SQLite-extension SPI。

重复执行 `lithograph_init()` 是幂等的。数据库格式高于当前 Extension 可理解版本时直接返回 `FORMAT_TOO_NEW`，不得自动降级或重写历史。

`_lithograph_meta` 中必须存在 Lithograph magic marker、`databaseId` 与 `storageFormat` 才视为已初始化。如果数据库里已经存在任意 `_lithograph_*` user object，但缺少有效 marker，`lithograph_init()` 返回 `STORAGE_ERROR`，不覆盖、不删除、不迁移这些对象。Lithograph 不依赖宿主 `PRAGMA foreign_keys` 是否开启来维持内部正确性；所有 canonical referential invariants 由 Engine 明确验证，SQLite physical constraints 只作为附加防线。

“未初始化”只表示 `main` 中既不存在有效 `_lithograph_meta`，也不存在任何 reserved internal-schema evidence。若 metadata 缺失、被重命名或损坏，但 `main` 中仍存在 reserved object / canonical-internal child object，则该 database 属于可检测的损坏/冲突状态而不是 pristine uninitialized：`lithograph_init()` 与 `lithograph_version()` 返回 `STORAGE_ERROR`；`lithograph_integrity_check()` 在仍可完成检查时返回 `ok: false` 与结构化 `STORAGE_ERROR`。

Lithograph 不使用 `PRAGMA user_version`，避免占用宿主应用的数据库版本字段；内部格式版本保存在 `_lithograph_meta`。

除 `lithograph_version()` 外，所有要求 graph state 的 API 在尚未执行 `lithograph_init()` 时返回 `NOT_INITIALIZED`。`lithograph_version()` 不要求先初始化，并分别返回 Extension version、Cypher Profile、支持的 storage-format range 与当前 database 的 storage-format version；`main` 中不存在 Lithograph metadata 时当前 format 与 `databaseId` 为 `null`，更高但 marker 可读的 format 仍报告其实际版本。若 metadata 已存在但损坏，或 metadata 读取本身发生 `BUSY` / resource / I/O 等错误，则返回对应稳定 error，不把失败静默伪装成未初始化。

普通 graph/query/transaction API 的每次 invocation 只执行**结构性初始化校验**：验证 metadata marker、当前 storage-format 可理解、canonical internal-schema inventory / metadata shape 正确，并拒绝会截获 internal write 的 unsafe TEMP trigger。它们**不得**在每次调用前重算完整 Commit / Layer / Schema hash、遍历完整 Commit DAG 或扫描完整 graph 来证明 canonical history integrity；否则 query latency 会随 database 总规模线性增长并违反[Large-scale Invariants](runtime.md#large-scale-invariants) large-scale invariant。完整 canonical-history / graph integrity scan 由显式 `lithograph_integrity_check()`、初始化/迁移验收和其它明确要求完整 integrity evidence 的维护路径负责。普通执行仍依赖 canonical storage primitives 自身的 point/overlay invariant 校验，并在实际访问到损坏记录时 fail closed。

若 marker 可读但 `storageFormat` 低于当前 Extension 的 minimum supported format，只有存在该旧 format 到当前 format 的显式 migration path 时，`lithograph_init()` 才可以在[Storage Migration](storage.md#storage-migration) transaction contract 下迁移；没有已实现 migration path 时按内部格式不兼容/损坏返回 `STORAGE_ERROR`。`lithograph_version()` 不把这种低于 minimum 的状态当作正常可用 database 返回成功 JSON。高于 maximum 的可读 format 仍按前述规则报告实际版本，由需要理解内部语义的 API 返回 `FORMAT_TOO_NEW`。

SQL information/init API 返回 JSON object：

```text
lithograph_init()
  -> {databaseId, storageFormat, root, branch}

lithograph_version()
  -> {extension, cypherProfile,
      storageFormat: {min, max, current}, databaseId}

lithograph_validate(query)
  -> {valid: true, cypherProfile}

lithograph_integrity_check()
  -> {ok, errors, checked}
```

`lithograph_integrity_check()` 在发现当前 Extension 可理解、可读取但损坏的内部状态时返回 `ok: false` 与结构化 errors；只有检查过程本身无法执行时才作为 SQLite function error 失败。storage format 高于当前 Extension 可理解范围属于无法执行完整 integrity check，直接返回 `FORMAT_TOO_NEW`，不把兼容性拒绝伪装成 corruption result。

<a id="sql-bridge"></a>

### SQL Bridge

Stock SQLite parser 不能由 loadable extension 增加新的顶层 `MATCH ...` grammar，因此 raw SQLite 客户端通过已注册 SQL function / table-valued function 调用 Cypher：

```sql
SELECT lithograph(
  'MATCH (p:Person) RETURN p.name ORDER BY p.name',
  '{}',
  '{}'
);

SELECT ordinal, event, data
FROM lithograph_rows(
  'MATCH (p:Person) RETURN p.name AS name',
  '{}',
  '{}'
);
```

公开 SQL API：

| API | 语义 |
| --- | --- |
| `lithograph_init()` | 初始化或迁移当前 database |
| `lithograph(query [, params [, options]])` | 执行一个 Cypher query，返回完整 Lithograph JSON envelope |
| `lithograph_tx_begin(options_json)` | 开始当前 connection 的 Lithograph explicit transaction，返回 pinned base Commit |
| `lithograph_tx_commit()` | finalize 唯一最终 Commit 并提交 Engine-owned SQLite transaction |
| `lithograph_tx_abort()` | rollback Engine-owned SQLite transaction，返回 `{"aborted":true}` |
| `lithograph_rows(query [, params [, options]])` | eponymous-only table-valued function，真正流式返回 `ordinal`、`event` 与 JSON `data` |
| `lithograph_validate(query)` | 只执行 parse + semantic/type/schema validation，不执行 query |
| `lithograph_version()` | 返回 Extension 与 storage-format version |
| `lithograph_integrity_check()` | 检查 Commit DAG、Branch ref、Layer hash、referential integrity 与内部表结构 |

`lithograph()` 与 `lithograph_rows()` 共用完全相同的 execution-input contract：`query` 必须提供且是非 `NULL` SQL TEXT，去除首尾 whitespace 后不能为空；`params` / `options` 可以省略，省略时分别等价于 `'{}'`，但**显式 SQL `NULL` 不等价于省略**，非 TEXT/NULL 都返回 `INVALID_ARGUMENT`。`params` TEXT 必须解析为 JSON object，key 是 parameter name，value 使用 [Lithograph JSON](#lithograph-json) parameter encoding且不能包含不能作为 Cypher parameter 的 structural graph value；`options` TEXT 同样必须是 JSON object并遵守 [Query Options](#query-options)，未知 key/type/shape 返回 `INVALID_ARGUMENT`。Scalar 接受 1–3 个参数；rows 的 hidden `query` 必需，hidden `params/options` 可省略且使用同一默认/错误规则。两种 surface 不允许因 adapter 不同而接受不同 input JSON。

`lithograph()` 与 `lithograph_rows()` 是同一个 Cypher execution core 的两种结果消费方式，不维护两套 query capability。只要 Query Engine 在当前 execution context 支持该 Cypher，两个 surface 都可以执行，包括 read、graph/schema/version mutation、Full-text、Raw Vector、Managed Semantic、`LOAD CSV` 与 transaction-owning subquery；是否可写由 SQLite connection / 当前 transaction context 和 Engine 自身语义决定，`lithograph_rows()` 不再实现第二套 read-only 分类。

`lithograph_rows` 的公开 virtual-table shape 固定为：

```text
ordinal INTEGER
event   TEXT
data    TEXT
query   HIDDEN
params  HIDDEN
options HIDDEN
```

该 eponymous-only virtual table 必须通过 SQLite virtual-table direct-only 机制注册为 `SQLITE_VTAB_DIRECTONLY`；不能从持久化 view、trigger、generated/schema expression 或其它 schema-stored SQL 间接执行。现在 rows surface 可以拥有 graph/schema/version mutation、filesystem/network I/O 与 transaction boundary，这一限制是安全/副作用入口合同，不因 query 自身恰好只读而放宽。

每个 scan 对应一次独立 Cypher execution，event 顺序固定为：

```text
0       | columns | ["name", ...]
1..N    | row     | [<Lithograph JSON values>...]
N + 1   | summary | {<summary object>}
```

`columns` event 恰好一次，`data` 是 column-name JSON array；`row` 零到多次，`data` 是按 columns 顺序排列的 Lithograph JSON value array；`summary` 只有 execution 最终成功后才出现，`data` 使用与 `lithograph()` envelope 相同的 summary object。零行 query 仍返回 `columns -> summary`，没有 `RETURN` / `YIELD` 的 statement 返回 `columns=[] -> summary`。失败、interrupt、cancel 或未完成 cursor close 通过 SQLite statement error / lifecycle cleanup 表达，不增加 `error` event，也不得伪造 success summary。

Execution 是否 incomplete 只由内部是否已经到达 **finalize-success** 判断，不由调用方最终是否成功拿到 summary bytes 判断。正常路径中，cursor 先 finalize-success、进入不可回退的 terminal-success state，再向 SQLite 暴露最后一个 `summary` row；调用方拿到 summary 因此可以证明 execution 已成功，也不需要再额外 `xNext` 到 EOF。finalize-success 之后发生的 `xClose`、statement finalize、connection 正常释放，甚至 summary `xColumn` 的 post-finalize host handoff failure，都只回收 adapter/read/spill 资源，不再次调用 cancel/rollback，也不把 active explicit transaction 的已成功 staged statement改成 abort。只有 **finalize-success 尚未发生** 时的 early-close/drop 才属于 incomplete execution并触发 cancel/rollback。因而“没有收到 summary”通常表示失败/未完成，但在明确的 post-finalize result-delivery failure场景不能作为无 side effect 的证明。

因为 incomplete side-effecting stream 的 `xClose` 可能需要执行 `ROLLBACK TO/RELEASE` 或 explicit-transaction abort，rows cursor 的 close path 必须真实执行 cancel/rollback cleanup，不能只释放 Rust object 后忽略未完成 execution。**Stock SQLite 不提供可传播的 virtual-table `xClose` error channel**：SQLite 3.45.0 与当前 runtime 的 VDBE 在释放 `CURTYPE_VTAB` cursor 时调用 `sqlite3_module.xClose` 但丢弃其返回值，因此即使自定义 module 的 `xClose` 返回非 `SQLITE_OK`，对应 `sqlite3_step` / statement finalize 也不能稳定观察该错误。Lithograph 不通过 SQLite fork、未公开 API 或辅助 graph connection 改变这一宿主事实。

因此 close cleanup 的 fail-closed contract 分成两层：正常 cleanup 成功时，early-close 完成 rollback/release并释放 cursor；cleanup 失败时仍执行下文定义的 full-`ROLLBACK` recovery，并把当前 `sqlite3*` 标记为 **Lithograph cleanup-failure quarantine**。由于 host 已经吞掉 `xClose` 返回值，正在关闭的 statement 可能仍表现为成功 finalize；但该 connection 后续任何 Lithograph entrypoint 在执行工作前都必须稳定返回 `INTERNAL_ERROR`，要求 caller 关闭并丢弃该 connection。Lithograph 不声称能够阻止 caller 在该 handle 上执行任意非 Lithograph SQL。所用 vtab wrapper 如果把 `xClose` 实现为 cursor destructor/drop，则该 dispatch 可以承担这个 non-panicking cleanup + quarantine 动作；这里不再为了一个 stock SQLite 不会传播的返回码增加无效 FFI shim。

Public `summary` 的共同含义是“本次 Lithograph execution 已成功到达自己的 terminal boundary”，不是跨所有 transaction context 的统一 durability 承诺：

| Execution context | `summary` 后的含义 |
| --- | --- |
| SQLite autocommit 的普通 read/write | 本次 Lithograph execution 已成功 finalize；普通 write 的新 Commit/ref 已 publication |
| caller-owned SQLite `BEGIN ... COMMIT/ROLLBACK` | 本次 execution 已在 outer SQLite transaction 内成功并形成其逻辑 Commit/ref state，但最终文件 durability / other-connection visibility仍由 caller 后续 `COMMIT` 或 `ROLLBACK` 决定；outer rollback可以删除这些 Commit |
| active Lithograph explicit transaction | statement `summary.commit = null`，只表示 staged execution 成功；最终 durable Commit只由 `lithograph_tx_commit()` 返回 |
| `CALL ... IN TRANSACTIONS` | outer summary表示整个 Cypher execution按 Profile成功完成；各成功 inner batch在此前已经独立 durable，outer failure/no-summary也不撤销这些 batch |

因此“收到 row”从不等于普通 write 已成功；“收到 summary”表示该 execution 的 Lithograph boundary成功，但调用方仍必须按自己选择的 outer transaction context判断最终 durability。

`xFilter` 只解析 hidden inputs、prepare execution 并把 cursor 定位到 `columns` event；为了得到 columns metadata 不得提前执行 graph/schema/version mutation、Provider/file I/O 或 transaction subquery batch。调用方只有在继续推进 cursor 时才开始/继续 Cypher runtime work。因此只消费 `columns` 后立即停止的 scan 不产生 execution side effect；success `summary` 则必须在对应普通 write SAVEPOINT 已成功 release、或该 execution 的其它成功 boundary 已完成之后才可观察。

`lithograph_rows` 是 pull-based **execution stream**，不是“先把完整结果收集后再分批输出”的 adapter。除 Cypher operator / transaction semantics自身要求的 semantic barrier 外，Engine必须随着 SQLite cursor消费增量推进 execution，最终 row count增长不得导致 retained final-result collection同阶增长；`ORDER BY`、`DISTINCT`、aggregation，以及 [Cypher Transaction Subqueries](storage.md#transaction-subqueries) 中“必须先知道一个 inner batch最终 transaction/error outcome 才能决定 outer result”的 boundary，都使用 bounded memory与必要的 TEMP/disk spill。普通单 transaction mutating execution可以在最终提交前流出 provisional result rows；只有生成最终 `summary` 时才完成本 execution的 success/commit boundary。transaction subquery则不得公开尚未确定 batch outcome 的 provisional successful rows，成功 batch commit后再增量排出该 batch结果。若 cursor在 summary前被关闭或取消，尚未 durable 的普通 execution/current inner batch必须 rollback；已经 durable 的 transaction-subquery batch按其 Cypher语义保留。

这里的 success/commit boundary 约束 Lithograph 自己拥有的 execution state：既包括可纳入当前 SQLite transaction 的 `main` graph/schema/index/ref/maintenance state，也包括本次 execution 要修改的 connection-local Lithograph state（例如 active Branch checkout）。除 `IN TRANSACTIONS` 明确定义的 intermediate durable batch 外，普通 execution 的这些 Lithograph-owned side effect 在 success `summary` 可观察前必须仍可 rollback/defer；即使 procedure/result row 已经先流给 caller，error/cancel/early-close 也不能留下 checkout 已改变、ref 已移动或 maintenance 已发布的“无 SUMMARY 成功”状态。外部世界不属于这个原子边界：已经完成的 HTTP/file read、第三方服务计费，以及 Provider 按自己配置写入的独立 cache database 可以在 outer Lithograph execution 最终失败或被取消后继续存在；它们不得被描述成已被 Lithograph rollback。反过来，这些外部/Provider side effect 也不能让失败的 Lithograph canonical write 被当作成功提交。

SQLite planner / outer SQL 如果让同一 virtual table 发生多次 scan，则每次 scan 都是一次独立 execution；Lithograph 只保证单个 `xFilter -> xNext* -> xClose` cursor lifecycle 内 execution 恰好 prepare 一次、随消费增量推进并正确完成/取消，不承诺“一条任意 outer SQL text 永远只执行一次 Cypher”。需要 mutation 只发生一次的调用方应让 outer SQL 只产生一次 scan。

`ordinal/event/data` 是普通 visible virtual-table columns，因此外层 SQLite 可以对它们执行 projection、`WHERE`、`ORDER BY`、`LIMIT` 等关系操作。**这些 visible-column 条件不是 Lithograph execution options。** v1 的 `xBestIndex` 只把 hidden `query/params/options` 作为 execution inputs，不以 `ordinal/event/data` constraint 做会改变 Cypher work 的 predicate/limit pushdown：即使 outer SQL 最终只保留 `event='summary'`，cursor 仍必须按正常顺序推进完整 execution；即使只投影 `data`，event lifecycle 也不改变。未来若增加 visible-column optimization，也只能跳过已证明无副作用的 adapter serialization，不能跳过 Cypher operator/transaction work 或改变 terminal boundary。

Outer SQL 自己仍会影响**消费**：例如 `LIMIT 1` 在 `columns` 后关闭 cursor，因此普通 side-effect execution 不会运行；`WHERE event='row'` 会让 SQLite 继续扫描到 EOF、使 underlying execution 可以成功 terminal，但 outer filter 可以把 `summary` row 从最终 SQL result 隐藏。因而“summary event 只在成功后由 vtab 产生”不等于任意 outer SELECT 都保证把 summary 交给 application。需要以 summary 作为 terminal success evidence 的调用方必须消费不会过滤/截断该 event 的 rows stream，或使用 `lithograph()` 完整 envelope。

“多次 scan”允许**顺序**重复 execution，不允许同一 `sqlite3*` 上把两个拥有 Lithograph `main` side effect / maintenance / transaction boundary 的 execution cursor 生命周期互相嵌套。一个 side-effecting streaming execution 从第一次真正 runtime side effect 开始到 terminal summary/error/close 期间，另一个会读取或修改 graph/version state 的 Lithograph execution 在开始副作用/建立第二个含糊 Snapshot 前返回 `TRANSACTION_BOUNDARY_REQUIRED`；否则 nested SAVEPOINT 可能让内层已经报告成功的 execution 被外层随后 rollback。多个纯 read cursor 仍可按 [Query Engine](query-engine.md#query-scoped-resolved-state) 的 read-guard 规则共存。前一个 execution 完整 terminal 后，同一 SQL planner 后续 rescan 可以正常创建下一次独立 execution。

`lithograph()` 每一次 SQLite scalar-function invocation 都对应一次 Cypher execution。用于 mutating Cypher 时，调用方应把它作为独立的 `SELECT lithograph(...)` statement 调用；如果 SQL 本身产生多次 scalar invocation，每次 invocation 都是独立 Cypher write 并各自遵守[Transaction 与 Concurrency Model](storage.md#transactions-and-concurrency) transaction/Commit 语义。这条普通 SQL Bridge 语义不因下述 explicit-transaction 封装而改变；Lithograph 不在 caller-owned `BEGIN ... COMMIT` 结束时自动合并已形成的 graph Commits。

`lithograph()` 返回完整 JSON envelope，因此明确允许**adapter collector** 为了单值结果收集全部 rows，并受 SQLite 单值长度限制和宿主可用内存约束；但它必须驱动与 `lithograph_rows()` 相同的 incremental execution state machine，不能保留另一套“完整执行/完整物化后才产生结果”的 scalar-only Query Engine。大结果使用 `lithograph_rows()`。超过 SQLite/Engine 可用资源时返回 `RESOURCE_ERROR`，不得截断结果。

为同时满足 shared core 与 ordinary-write rollback，内部 terminal lifecycle 必须是两阶段而不是“先 commit 再序列化”：execution 在 rows 消费完成后先进入 **terminal-ready**，冻结最终 columns/counters/metrics；若本 execution 要产生 Commit，则同时按 [Content Addressing](storage.md#content-addressing) 的 finalization preparation 冻结 `committed_at`、计算 prospective Commit ID，使完整 summary candidate 在 publication 前已经确定，但普通 Lithograph-owned write/ref/maintenance/connection-state side effect 此时仍可 rollback。adapter 随后完成自己要公开的结果编码和 SQLite size/resource checks。`lithograph()` 在这一阶段组装并验证完整 `{columns,rows,summary}`；`lithograph_rows()` 只编码/验证将要公开的 `summary` event payload。只有这些可检测的 adapter failure 都通过后，execution 才执行 **finalize-success**（Commit/ref publication、release/commit、install connection-local state），随后 scalar 返回已构造 envelope，streaming cursor 才暴露 public `summary` row。terminal-ready 到 finalize-success 之间的 encoding/length/resource failure走 cancel/rollback，prospective Commit ID 不进入 history，也不产生 public summary。`CALL ... IN TRANSACTIONS` 已 durable 的 earlier batch仍是既有例外，不能被这个两阶段 terminal 反向撤销。

“结果 preflight”只承诺把 Lithograph 自己能够提前完成的 JSON encoding、shape、`SQLITE_LIMIT_LENGTH` 与已知 resource check 放在 finalize-success 前；它不能保证 finalize 之后调用 SQLite 最终 `sqlite3_result_*` / vtab `xColumn` handoff 永远不再失败。若普通 scalar 的完整 envelope已经 preflight并 finalize-success，随后 host result handoff因 `SQLITE_NOMEM` 等失败，或 rows 已 finalize-success 后在最后一个 `summary` row 的 `data/event/ordinal` handoff阶段发生宿主错误，Lithograph **不能再把本 execution 自动回到 finalize-success 之前**：autocommit 下已 durable 的 Commit/ref/maintenance保持；active Lithograph explicit transaction 中已成功 staged statement保持并等待后续 tx commit/abort；caller-owned SQLite transaction 中本 statement state仍留在 outer transaction，caller仍可以显式 `ROLLBACK` 整个 outer transaction。connection-local success 同样不因 result handoff error 自动恢复旧值。调用方不得把“没有拿到成功 JSON/summary”直接解释成“没有 side effect”并盲目重放；应结合所处 transaction context，通过 Branch/history/state或 outer rollback确认结果。相反，`columns` 或普通 `row` event 的 handoff failure发生在 ordinary execution finalize-success之前，必须触发 cancel/rollback。

每个会产生 SQLite side effect 的 SQL operation 继续拥有自己设计定义的原子 boundary：例如 `lithograph_init()` 保持既有 invocation SAVEPOINT，Cypher execution / maintenance 使用 execution-owned SAVEPOINT 或该 operation 明确拥有的独立 transaction boundary。**本节新增的 terminal-ready -> result preflight -> finalize-success 规则只约束 `lithograph()` / `lithograph_rows()` 的 Cypher execution surface，以及下述 explicit-transaction lifecycle 明确引用该规则的操作，不因此重开 init 等无关 API 的历史 result-delivery contract。**

对普通 side-effecting Cypher execution，scalar SAVEPOINT 覆盖整个 callback；streaming scan 的 SAVEPOINT 从第一次 runtime side effect 持续到 finalize-success、error/cancel 或 `xClose`。普通 write 成功时只有在 terminal-ready payload/envelope 已编码并通过 size/resource checks 后才 `RELEASE` / finalize，随后 public summary/result 才可观察；Lithograph error/panic/cancel 时先 `ROLLBACK TO` 再 `RELEASE`，然后才把 error 返回 SQLite。这样一次 execution 不会留下半写 Layer/Commit/ref/schema。SQL callback/cursor 本身不能依赖“外层 SQL statement 失败会自动撤销递归写入”。`CALL ... IN TRANSACTIONS` 按其专用 inner-transaction contract，不套一个会吞掉已 durable batch 的 outer SAVEPOINT。

因此 side-effecting `lithograph_rows()` 的 writer/savepoint lifetime 与 caller 消费速度直接相关：一旦 execution 开始真实 write，它必须保持可 rollback boundary直到 finalize-success 或 cancel/close，慢 consumer、应用停顿、长 external I/O 或巨大 result stream都会延长 SQLite single-writer hold / WAL 等待。这是“stream provisional rows但最终仍原子提交”的必要成本，不允许通过提前 commit、后台继续 execution 或把剩余 rows预物化来隐藏。调用方应持续消费到 terminal或及时 close；需要避免长 writer时应调整 query/transaction batching，而不是改变 rows事务语义。

如果 host SQLite 因 authorizer、connection failure 或其它 SQLite-level failure 拒绝正常的 `ROLLBACK TO` / `RELEASE` cleanup，Lithograph 必须 fail closed：`ROLLBACK TO` 失败后不得继续 `RELEASE` 该 SAVEPOINT，因为最外层 SAVEPOINT 的 `RELEASE` 可能把本应撤销的变化提交；Engine 改为尝试整个 SQLite `ROLLBACK` 以清除未决 write 与 SAVEPOINT。该 recovery 在 caller-owned outer transaction 内也可能终止整个 outer transaction；这是无法完成 invocation-local cleanup 时优先保持 canonical storage 原子性的故障语义。只要进入 full-rollback fallback，当前 connection 就进入 cleanup-failure quarantine；在 scalar/`xNext` 等拥有可传播 error channel 的路径上本次 invocation 同时返回 `INTERNAL_ERROR`，在 stock SQLite 丢弃返回码的 `xClose` 路径上则由下一次 Lithograph entrypoint 报告同一 quarantine。若整个 `ROLLBACK` 也失败，connection 仍保持 quarantine，caller 必须关闭并丢弃，不能继续依赖其 transaction state。

`SQLITE_NOMEM` 是这个 cleanup-failure 分支的已验证宿主特例：如果错误来自 scalar callback 内部的 FTS5 tokenizer 构造，SQLite 会在该 callback 剩余期间保持 malloc-failed 状态，使 `ROLLBACK TO`、`RELEASE` 和 full `ROLLBACK` 都继续返回 `SQLITE_NOMEM`；外层 `sqlite3_step` / `sqlite3_exec` 返回后才可能再次执行 rollback。SQL execution surface 仍保留真实 `SQLITE_NOMEM` primary code；当 tokenizer probe 在 canonical Schema 持久化之前失败时，不得发布 Commit/Branch 变化。但由于 callback 内无法恢复 invocation-local transaction boundary，该 connection 属于上段定义的 cleanup-failure quarantine，caller 必须关闭并丢弃，不能把“外层返回后手动 rollback 可以成功”当成 Lithograph invocation 已满足 cleanup 合同。

多个 mutating `lithograph()` invocation 出现在同一个 raw SQL statement 时，语义明确为多个独立 SAVEPOINT/graph operations：在 SQLite autocommit mode 下，前一个成功 invocation 可以在后一个 invocation 失败前已经 durable；Lithograph 不承诺把整个宿主 SQL statement 合成一个 graph transaction。caller-owned SQLite `BEGIN ... COMMIT` 只能把多个已经形成的 Lithograph Commit 合并到同一个 durability boundary，不会折叠版本历史。调用方若需要“多个独立 Cypher execution 共同形成一个 Lithograph Commit”，必须使用[Explicit Transaction](storage.md#native-explicit-transaction)。推荐的普通 raw SQL write 形式仍是一个 statement 一个 `lithograph()` invocation。

#### SQL Explicit Transaction lifecycle

`lithograph_tx_begin/commit/abort` 只管理 connection-local explicit transaction lifecycle，不提供第三个 Cypher execution surface。同一 `sqlite3*` 最多只有一个 active Lithograph explicit transaction；SQL 调用方不额外执行 `BEGIN` / `COMMIT` / `ROLLBACK`：`lithograph_tx_begin` 内部取得 writer ownership 并执行 `BEGIN IMMEDIATE`，`lithograph_tx_commit` finalize graph Commit 后执行 SQLite `COMMIT`，`lithograph_tx_abort` 执行 SQLite `ROLLBACK`。

精确 SQL 签名与结果为：

```text
lithograph_tx_begin(options_json)
  -> {"baseCommit":"commit/<id>"}

lithograph_tx_commit()
  -> {"commit":"commit/<id>","counters":{...}}

lithograph_tx_abort()
  -> {"aborted":true}
```

active transaction 内，普通 `lithograph(query, params, options)` 与 `lithograph_rows(query, params, options)` 自动在同一 staged graph/Schema/Index state 上执行；不再存在 `lithograph_tx_execute()`。每次 execution 有独立 statement clock、result stream 与 counters，但 `summary.commit = null`，因为 durable Commit 只由最终 `lithograph_tx_commit()` 产生。前一个成功 execution 的 staged changes 对后一个 execution 可见。

`tx_begin` 要求恰好一个非 `NULL` UTF-8 JSON object；未知 option、错误 type/JSON shape 返回 `INVALID_ARGUMENT`。没有 active transaction 时 `tx_commit/tx_abort` 返回 `INVALID_ARGUMENT` + `SQLITE_MISUSE`。外层 caller-owned SQLite transaction 已存在时 `tx_begin` 返回 `TRANSACTION_BOUNDARY_REQUIRED`，避免 Engine-owned transaction 与 caller-owned durability boundary 重叠。

Lifecycle scalar 自身的 result failure 也有固定语义：`lithograph_tx_begin()` 在取得 writer ownership、pin/校验 base/expectedHead 后，必须在向 SQLite 报告 function success 前完成 `{baseCommit}` JSON 构造和可检测的 length/resource preflight；该阶段失败就 rollback 刚建立的 Engine-owned transaction并清除 active state，调用方不得观察“begin 返回失败但 transaction 其实已开始”。`lithograph_tx_abort()` 则以 rollback 为主操作：一旦 SQLite rollback + state cleanup 成功，transaction 已经 terminal aborted；之后 `{aborted:true}` 的宿主 result handoff 即使失败也不能恢复 active transaction，调用方应把该 connection 视为已无 active Lithograph tx，而不是重试 abort 假定旧 state 仍存在。

active transaction 内任一 `lithograph()` execution failure、`lithograph_rows()` failure/interrupt/cancel、或 streaming cursor 在 success `summary` 前关闭，都必须 fail closed：自动 rollback 整个 explicit transaction、清除 staged state 与 connection-local active state，后续必须重新 `tx_begin`。成功 execution 本身不提交 SQLite transaction；`tx_commit` 在 SQLite `COMMIT` 前完成可检测的 result 构造/长度检查。一旦 SQLite `COMMIT` 已成功，后续宿主结果传递失败不能反向撤销 durable Commit，调用方不得因此盲目重放 commit 或整个 transaction。

`lithograph_tx_begin/commit/abort` 都是有 transaction/storage side effect 的 direct-only scalar，只使用 `SQLITE_UTF8 | SQLITE_DIRECTONLY`，不得声明 `SQLITE_DETERMINISTIC` 或 `SQLITE_INNOCUOUS`。connection 未显式 commit/abort 就 teardown 时，SQLite rollback 与 connection-state destructor 共同清除未提交 state。

Explicit transaction 固定 target Branch、base Commit、transaction clock、`expectedHead` 与最终 Commit metadata；active 期间 execution 不能通过 query options 切换 `branch` / `at` / `author` / `message` / `mergeSession`。Version Procedure、Branch/Tag/Commit Data mutation、checkout/GC 等拥有独立 version/ref lifecycle 的 operation 继续返回 `TRANSACTION_BOUNDARY_REQUIRED`。`CALL { ... } IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 也必须拒绝，因为它们要求独立 durable inner transaction boundary，不能嵌套进“多 execution 最终只提交一次”的 explicit transaction。

External I/O 本身不是拒绝理由：普通 `LOAD CSV`、Managed Semantic query 等只要不另外拥有独立 transaction/version lifecycle，就可以在 explicit transaction 的 `lithograph()` / `lithograph_rows()` execution 中运行；它们的失败按上述 fail-closed 规则 abort 整个 explicit transaction。像 Semantic/Standard Index `rebuild` 这类显式选择 committed target 并拥有 maintenance lifecycle 的 procedure 仍按各自专题拒绝 active explicit transaction。调用方应自行控制长 external I/O 带来的 writer-hold 成本，Lithograph 不为了性能偏好改变事务语义。

`lithograph_validate()`、`lithograph_init()` 与 `lithograph_integrity_check()` 不属于 active explicit transaction 的 execution surface，active 时返回 `TRANSACTION_BOUNDARY_REQUIRED`，避免把 committed-only metadata/integrity 结果误解释成 staged state validation；`lithograph_version()` 只读取 extension/database version metadata，可以继续调用。需要验证某条 Cypher 是否能作用于当前 staged state 时，直接通过正常 execution surface 执行并依赖其 parse/semantic/type/schema checks。

<a id="native-c-abi"></a>

### Application-facing Native execution ABI（不提供）

Lithograph 不导出 application-facing Cypher execution / validation / transaction C ABI。Application、binding 与 database client 统一通过 SQLite SQL surface 使用 `lithograph()`、`lithograph_rows()`、`lithograph_validate()` 与 `lithograph_tx_begin/commit/abort`；不保留 `lithograph_v1_execute`、`lithograph_v1_validate`、`lithograph_v1_tx_*` 或 `lithograph_v1_free` compatibility layer。

本规则不影响 [Embedding Provider Contract](vector.md#embedding-provider) 的 `EmbeddingProviderV1`。后者是 Lithograph extension 与独立 Provider extension 之间的插件 SPI，不向 Application 提供 Cypher query execution。

`CALL { ... } IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 由普通 `lithograph()` 与 `lithograph_rows()` 在 SQLite autocommit mode 下直接执行；每个 inner batch 保留 Cypher 定义的独立 commit/error 语义。caller-owned SQLite transaction 或 active Lithograph explicit transaction 已经占有外层 transaction boundary 时返回 `TRANSACTION_BOUNDARY_REQUIRED`，不能用 SAVEPOINT 把独立 commit 偷换成可被外层整体 rollback 的“子事务”。

两种 result consumption shape 都不能改变上述 durability：`lithograph_rows()` 在 later batch/error/early-close 后保留 earlier durable batch；如果已 commit batch 正在 drain 时 row JSON/result handoff失败，该 batch同样保持 durable并终止 outer stream/后续 batch。`lithograph()` 即使在 earlier batch已提交后才发生最终 envelope serialization、allocation 或 `SQLITE_LIMIT_LENGTH` failure，也只能把本次 SQL invocation报错，不能反向撤销已经 durable 的 inner batch。调用方不得把这种 result-delivery failure当作“没有任何 side effect”并盲目重放整个 transaction-owning query。

<a id="query-options"></a>

### Query Options

公开 options JSON 使用以下稳定字段：

```json
{
  "branch": "main",
  "author": "alice@example.com",
  "message": "update graph",
  "graphView": {
    "requireAllLabels": ["tenant_acme"],
    "excludeAnyLabels": ["internal"]
  }
}
```

规则：

- `branch` 临时选择本 query 的 Branch；省略时使用当前 connection checkout 的 Branch；
- `at` 选择只读 Snapshot，接受 `commit/<id>`、`branch/<name>` 或 `tag/<name>`；Branch / Tag 在 execution 开始时先解析并 pin 到一个 immutable Commit，使用 `at` 的 query 不允许修改；
- `author` 和 `message` 是该 execution 内所有新 Commit 的 metadata 来源；省略时存 `null`。Version procedure 不定义第二套 author/message 来源；
- `branch` 与 `at` 互斥；
- `graphView` 是 query-local Graph View specification；省略或 `{}` 表示完整 Snapshot graph。它不持久化、不命名、不进入 Commit/Layer/Schema hash，也不改变 connection checkout；
- `graphView.requireAllLabels` 是 Node 必须同时拥有的 Label 集合；省略或空数组表示没有正向 Label 限制；
- `graphView.excludeAnyLabels` 是 Node 不能拥有的 Label 集合；命中任意一个即不可见；省略或空数组表示没有排除 Label；
- `graphView` 必须是 object，当前只允许 `requireAllLabels` / `excludeAnyLabels` 两个成员；成员值必须是 string array。其它成员、错误 JSON type 或非 string item 返回 `INVALID_ARGUMENT`；显式 `null` 不等价于省略；
- 两个 Label array 按精确 Label name 的 set 语义规范化，同一数组中的重复项去重；同一 Label 在规范化后同时出现在 `requireAllLabels` 与 `excludeAnyLabels` 时返回 `INVALID_ARGUMENT`；
- Label 按 Lithograph Label dictionary 的精确名称语义比较，不做 case folding 或 Unicode normalization。解析 Graph View 本身不得创建 Label dictionary entry：当前 graph state 中尚不存在的 required Label 使既有 Node 均不可见，尚不存在的 excluded Label 当前没有过滤效果；如果后续合法 Cypher write 通过正常 Label mutation 创建该名称，后续 clause 按更新后的 graph state 重新计算 visibility；
- Graph View v1 不定义独立 Relationship selector：Relationship 只有在其 source 和 target Node 都可见时才可见，因此 view 是由 Node visibility 诱导出的 Property Subgraph；
- `graphView` 可以与 `branch` 或只读 `at` 组合；它们决定 base Snapshot，初始 visibility 按该 Snapshot 计算，read-write query 的后续 clause 再按[Graph View Execution Boundary](query-engine.md#graph-view)基于前序 staged writes 后的 graph state 重新计算；
- `graphView` 只约束 graph-data query / mutation / Search 的可见数据。Schema、Constraint、Index definition 和 Version Procedure 不属于 Graph View；这些 command/procedure 与 `graphView` 同时出现时返回 `INVALID_ARGUMENT`，避免把子图错误解释成独立 Schema 或 Version repository。

对 Version Procedure：`branch` query option 只为“对当前 Branch 操作”的 procedure 临时选择 target（`commit.create`、`patch.apply`、`merge.start`、`rebase`、`squash`、`reset`、`revert`）；它不永久改变 connection checkout。`merge.finalize` 的 target 已由 Session 固定，`merge.get/list/conflicts/resolve/abort` 也不重新选择 target；这些 procedure 与 query-level `branch` 同时出现时返回 `INVALID_ARGUMENT`。`merge.start/get/list/conflicts/resolve/abort` 不保存未来 Commit metadata，因此与 `author` / `message` 同时出现也返回 `INVALID_ARGUMENT`；只有 `merge.finalize` 接受 `author/message`，且仅 diverged `merged` 结果真正写入新 Commit。`branch.create/delete/checkout/list`、`tag.*` 与 `commit.data.*` 自己显式指定或管理 target，同样不接受 query-level `branch`。任何 version mutation 与 `at` 同时出现都返回 `READ_ONLY_SNAPSHOT`。

Parameters JSON 与 result JSON 共用[Lithograph JSON](#lithograph-json) tagged-value encoding。普通 JSON primitive/list/map 直接映射到对应 Cypher value；需要保留 INTEGER64 边界、Temporal、Point、Vector 或 UUID 类型时必须使用 `$type` tagged form。

SQL explicit transaction 的 begin options 使用独立的 transaction-level shape：

```json
{
  "branch": "main",
  "expectedHead": "commit/<64-hex-id>",
  "author": "alice@example.com",
  "message": "apply one logical change"
}
```

- `branch` 省略时使用当前 connection active Branch；开始后 target Branch 固定，active transaction 内的普通 execution 不能切换 Branch；
- `expectedHead` 可省略；提供时只接受 resolved `commit/<id>`，并在取得 writer ownership 后与 target Branch 当前 head 原子比较，不一致返回 `BRANCH_HEAD_MOVED` 且 transaction 不开始；
- `author` / `message` 只属于最终唯一 Commit；active transaction 内各 `lithograph()` / `lithograph_rows()` execution 不再接受自己的 Commit metadata；
- active transaction 内 execution 可以使用 query-local `graphView`，但 `branch` / `at` / `author` / `message` / `mergeSession` 等会选择另一 transaction/version context 的 options 返回 `INVALID_ARGUMENT`；
- Version Procedure、Branch/Tag/Commit Data mutation、checkout/GC，以及 `CALL { ... } IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 不能在 explicit transaction 内嵌套执行，返回 `TRANSACTION_BOUNDARY_REQUIRED`；
- External I/O 本身不禁止 explicit transaction execution；普通 `LOAD CSV`、Managed Semantic query 等只要不另行拥有独立 transaction/version lifecycle 就可以执行。显式 rebuild/maintenance procedure 是否允许由其 own lifecycle contract 决定。调用方承担长 I/O 延长 SQLite single-writer ownership 的成本；失败时整个 explicit transaction fail closed abort。

Merge Session candidate inspection 使用同一 execution options surface，而不是增加第二套 query API：

```json
{
  "mergeSession": {
    "id": "merge-session/<uuid>",
    "revision": 7
  },
  "graphView": {
    "requireAllLabels": ["tenant_acme"]
  }
}
```

- `mergeSession.id` 是[Three-way Merge 与 Merge Session](versioning.md#merge-session)定义的 opaque Merge Session identity，不是 version descriptor；`revision` 必须与该 Session 当前 revision 精确相等，否则返回 `MERGE_SESSION_CHANGED`；
- `mergeSession` 与 `branch` / `at` / `author` / `message` 互斥，可以与 query-local `graphView` 组合；
- 只有当前没有 unresolved merge conflict 的 Session 才能读取 candidate；仍有 unresolved conflict 时返回 `MERGE_CONFLICT`；
- candidate execution 只允许普通 current-graph read、Search 与 Schema/Constraint/Index introspection。graph/schema/index mutation 返回 `READ_ONLY_SNAPSHOT`；Version Procedure、transaction-owning subquery 与 `LOAD CSV` 返回 `TRANSACTION_BOUNDARY_REQUIRED`；
- candidate query 绑定 `(session, revision)` 而不是 Commit，因此 `summary.commit = null`，并通过[Lithograph JSON](#lithograph-json)的 `summary.mergeSession` 返回实际 session/revision。调用方可以用同一 revision 执行多次一致性检查，随后把该 revision 交给 `merge.finalize`；若期间 resolution 改变，finalize 必须以 `MERGE_SESSION_CHANGED` 拒绝旧验证结果。

<a id="external-io"></a>

## LOAD CSV 与 External I/O

`LOAD CSV` 支持 `file://`、`http://` 与 `https://` source。Managed Semantic 的 Embedding Provider 同样可能拥有 network / local-model external-I/O authority。两者的读取/调用权限都继承宿主进程和实际 SQLite extension runtime；Lithograph 不注入隐藏 credentials。

I/O error、malformed CSV、type/constraint error 按 Cypher query failure 传播。普通 `LOAD CSV` 位于当前 query transaction；`CALL ... IN TRANSACTIONS` 使用[Cypher Transaction Subqueries](storage.md#transaction-subqueries) transaction semantics。

两种 SQL execution surface 都可以执行 external-I/O query；`lithograph_rows()` 不因为 filesystem/network/model I/O 而返回 adapter-level 拒绝。只读 SQLite `main` connection 也可以执行纯读 Managed Semantic query：Lithograph 只读取 graph/schema/history，OpenAI-compatible 等 Provider 若启用自己的独立 cache database，可以按其配置单独写该 cache，而不得反向写 Lithograph `main`。

`db.index.semantic.query*` / `db.index.semantic.rebuild` 与 graph mutation、transaction-owning subquery 的组合限制由 Managed Semantic 自身的 Query Engine contract 决定，不由 `lithograph_rows()` 追加分类。Provider cache hit/miss、publish、eviction 都是 Provider 内部行为，对 Lithograph queryType、Commit、Branch 与 correctness 不可见；一个合法 cache entry 即使由最终失败/取消的 Lithograph execution 产生，也可以继续保留并被之后相同 embedding-space request 复用。

<a id="results-and-errors"></a>

## Result 与 Error Contract

<a id="lithograph-json"></a>

### Lithograph JSON

SQL Bridge 的完整 envelope：

```json
{
  "columns": ["name"],
  "rows": [["Alice"]],
  "summary": {
    "queryType": "read",
    "commit": "commit/...",
    "counters": {},
    "metrics": {
      "rows": 1,
      "dbHits": 3,
      "elapsedMicros": 42
    }
  }
}
```

`summary.queryType` 使用封闭值 `read | write | schema | version | mixed`。普通非 explicit-transaction `lithograph()` / `lithograph_rows()` execution 中，`summary.commit` 是该 query 执行所 pin 的最终可观察 Snapshot：read query 为读取 Commit，graph/schema write 为新 Commit，ref-only version operation 为操作后的 active-branch Commit。active explicit transaction 内的 execution 只作用于 staged state，因此 `summary.commit = null`；最终 durable Commit 由 `lithograph_tx_commit()` 单独返回。Merge Session candidate query同样不是 durable Commit，因此 `summary.commit = null`，并额外返回 `summary.mergeSession={"id":"merge-session/...","revision":N}`；其它 execution 省略 `mergeSession` 字段。`summary.counters` 至少固定包含：

```text
nodesCreated
nodesDeleted
relationshipsCreated
relationshipsDeleted
propertiesSet
propertiesRemoved
labelsAdded
labelsRemoved
constraintsAdded
constraintsRemoved
indexesAdded
indexesRemoved
```

没有发生的 counter 返回 `0`，不因 query 类型省略 key。

`summary.metrics` 在 `lithograph()` envelope 与 `lithograph_rows()` 的 `summary` event 使用同一 shape，固定包含 `rows`、`dbHits`、`elapsedMicros`。其中 `elapsedMicros` 在 shared execution 进入 terminal-ready 时冻结，覆盖本 execution 的 query/operator work 到“全部公开 rows 已产生、summary candidate 已确定”为止；不包含 adapter 对最终 envelope/summary JSON 的 serialization/result handoff，也不包含 terminal-ready 之后的 finalize-success SAVEPOINT release / SQLite COMMIT / connection-state install。这些成本按 [Performance Evidence Contract](runtime.md#performance-evidence) 另行测量，不能为了让单个 metric 看起来完整而破坏“result encoding failure 必须发生在 ordinary side effect finalize 前”的原子边界。普通 execution 也返回该字段；`PROFILE` 在不改变 query rows/value semantics 的前提下使用同一基础计量，并额外返回 `summary.profile`：

```json
{
  "operators": [
    {"id": 0, "operator": "LabelIndexScan", "rows": 2, "dbHits": 4},
    {"id": 1, "operator": "Project", "rows": 2, "dbHits": 0}
  ]
}
```

`profile.operators` 与该 execution 的 Physical Plan 使用相同 operator 顺序和 zero-based `id`；`operator` 是 Lithograph Physical Operator 的稳定类别名，`rows` 是该 operator runtime boundary 实际产出的 row 数，`dbHits` 是归属于该 boundary 的底层 graph/index access 数。Planner-only annotation 不伪造 runtime count；如果一个 plan operator 被 executor 融合且没有独立可观察的 storage access，则其 `dbHits` 可以为 `0`，不能通过平均分摊或估算制造计数。所有 operator `dbHits` 之和必须等于 `summary.metrics.dbHits`。普通 execution 和 `EXPLAIN` 省略 `summary.profile`；`EXPLAIN` 不执行 graph/storage operator，因此除计划输出自身的 row accounting 外不得伪造 storage hits。

Lithograph JSON v1 的 value encoding：

- `null`、Boolean、String 使用普通 JSON；
- Cypher Integer 在 JavaScript safe integer 范围内使用 JSON integer，范围外使用 `{"$type":"Integer","value":"<decimal>"}`；
- finite Float 使用 JSON number；NaN / ±Infinity 使用 `{"$type":"Float","value":"NaN|Infinity|-Infinity"}`；
- List 使用 JSON array；Map 默认使用 JSON object；如果 Map 本身需要表达 reserved `$type` key，则使用 `{"$type":"Map","entries":{...}}` 转义；
- Node：`{"$type":"Node","elementId":"n:...","labels":[...],"properties":{...}}`；
- Relationship：`{"$type":"Relationship","elementId":"r:...","type":"...","start":"n:...","end":"n:...","properties":{...}}`；
- Path：`{"$type":"Path","nodes":[...],"relationships":[...]}`，两个数组均按 traversal order；
- Date/Time/LocalDateTime/ZonedDateTime/Duration 使用各自 `$type` + ISO/Cypher canonical textual value；ZonedDateTime 额外保留 exact zone id；
- Point：`{"$type":"Point","crs":"...","coordinates":[...]}`；
- Vector：`{"$type":"Vector","coordinateType":"...","dimension":N,"values":[...]}`；
- UUID：`{"$type":"UUID","value":"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"}` lowercase canonical text。

Parameters 接受同一 tagged encoding。识别到已知 `$type` 的 object 按 tagged value 解码；调用方要传递一个普通 Cypher Map 且其 key 包含已知 `$type` 时必须使用 `$type: "Map"` wrapper。未知 `$type` 返回 `INVALID_ARGUMENT`，不静默降级为普通 Map。

这种 adapter encoding 只解决 SQLite/JSON 边界，不改变 Engine 内部 Cypher Value 类型。

`lithograph_rows` 的 `row` event 使用同一 value encoding，因此 scalar 与 streaming surface 不产生两套结果语义。`columns` / `row` / `summary` 的 event/data shape 由 [SQL Bridge](#sql-bridge) 固定，零行结果也通过独立 `columns` event 暴露列信息。

Cypher statement 只有显式结果生成 surface（例如最终 `RETURN` / procedure `YIELD`）才能产生公开 result columns / rows。以 graph/schema/version mutation clause 结束且没有最终结果投影的 statement 固定返回 `columns=[]`、`rows=[]`；executor 为执行后续 mutation 保存的内部 binding row 不属于 Result Contract，也不得经 `lithograph()` / `lithograph_rows()` 暴露。

<a id="error-categories"></a>

### Error Categories

公开稳定 error categories：

```text
PARSE_ERROR
SEMANTIC_ERROR
TYPE_ERROR
SCHEMA_ERROR
CONSTRAINT_ERROR
NOT_INITIALIZED
INVALID_ARGUMENT
GRAPH_VIEW_VIOLATION
VERSION_NOT_FOUND
BRANCH_NOT_FOUND
TAG_NOT_FOUND
BRANCH_HEAD_MOVED
MERGE_CONFLICT
MERGE_SESSION_NOT_FOUND
MERGE_SESSION_CHANGED
TRANSACTION_BOUNDARY_REQUIRED
READ_ONLY_SNAPSHOT
FORMAT_TOO_NEW
BUSY
INTERRUPTED
RESOURCE_ERROR
STORAGE_ERROR
IO_ERROR
INTERNAL_ERROR
```

Application-facing failure 只通过 SQLite error channel 返回，不增加 `error` row/event、`last_error` function 或第二套 error object API。SQL surface 的稳定 error text 固定为：

```text
LITHOGRAPH_<CATEGORY>: <message>
LITHOGRAPH_<CATEGORY>: <message> [line=<N>,column=<N>]
```

只有 Core 提供 query source position 时才追加第二种**尾部 suffix**；`line` / `column` 都是 1-based decimal integer，中间固定使用英文逗号且不插入额外空格。没有 position 时使用第一种形式，不伪造 `0` / `null`。调用方若需要机器识别，解析稳定 category prefix 与可选 trailing location suffix；`<message>` 本身仍是人类可读诊断，不承诺可机器解析。

SQL surface 同时通过 `sqlite3_result_error_code` 保留 SQLite primary result code；parse/semantic/type/schema/version argument error 通常为 `SQLITE_ERROR`，API misuse 为 `SQLITE_MISUSE`，host lock contention 的 `SQLITE_BUSY/SQLITE_LOCKED` 映射 `BUSY`，host/user cancellation 的 `SQLITE_INTERRUPT` 映射 `INTERRUPTED` 并保留 `SQLITE_INTERRUPT`，`SQLITE_NOMEM/SQLITE_TOOBIG/SQLITE_FULL` 映射 `RESOURCE_ERROR`，I/O/open/read-only-file 类错误映射 `IO_ERROR`。Interrupt 不是 memory/disk resource exhaustion，不能为了沿用旧 adapter enum 把它写成 `RESOURCE_ERROR`。SQLite 对少数 resource result code 有宿主级 canonicalization：例如 `SQLITE_NOMEM` 可能把自定义 text 改写成 `out of memory`。真实 primary code 始终比保留 Lithograph prefix 更优先，不能为了 error text 伪装成 `SQLITE_ERROR`。错误不得泄漏内部 SQLite table/schema 或 Provider secret。
