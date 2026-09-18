# SQLite 接口、外部 I/O 与结果合同

[设计入口](../design.md) · [开发状态与验收](../development/README.md)

本文件拥有 SQL Bridge、Native C ABI、options、外部 I/O、结果编码与错误映射。事务状态机由 [Storage](storage.md#transactions-and-concurrency) 定义；Procedure 的专用参数分别由 [Versioning](versioning.md)、[Standard Index maintenance](schema-and-indexes.md#build-publish-maintenance)、[Full-text](full-text.md)、[Vector](vector.md) 定义，不在此复制。

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

在 format `2` 基础上，持久 derived Standard Index 使用 format `3`，其 fresh/init、旧格式读取、exact internal-schema inventory 与迁移边界由[Performance Storage Format 3](storage.md#storage-format-3)统一定义；Managed Semantic persistent Embedding cache 使用[Managed Semantic Storage Format 4](storage.md#storage-format-4)定义的 format `4`。任何更高格式都不能在旧 format 中静默添加未声明的 reserved table/index。公开 Native execution ABI 和 Cypher 25 grammar/Profile 不因此升级；Embedding Provider contract 是独立的 additive SQLite-extension ABI。

重复执行 `lithograph_init()` 是幂等的。数据库格式高于当前 Extension 可理解版本时直接返回 `FORMAT_TOO_NEW`，不得自动降级或重写历史。

`_lithograph_meta` 中必须存在 Lithograph magic marker、`databaseId` 与 `storageFormat` 才视为已初始化。如果数据库里已经存在任意 `_lithograph_*` user object，但缺少有效 marker，`lithograph_init()` 返回 `STORAGE_ERROR`，不覆盖、不删除、不迁移这些对象。Lithograph 不依赖宿主 `PRAGMA foreign_keys` 是否开启来维持内部正确性；所有 canonical referential invariants 由 Engine 明确验证，SQLite physical constraints 只作为附加防线。

“未初始化”只表示 `main` 中既不存在有效 `_lithograph_meta`，也不存在任何 reserved internal-schema evidence。若 metadata 缺失、被重命名或损坏，但 `main` 中仍存在 reserved object / canonical-internal child object，则该 database 属于可检测的损坏/冲突状态而不是 pristine uninitialized：`lithograph_init()` 与 `lithograph_version()` 返回 `STORAGE_ERROR`；`lithograph_integrity_check()` 在仍可完成检查时返回 `ok: false` 与结构化 `STORAGE_ERROR`。

Lithograph 不使用 `PRAGMA user_version`，避免占用宿主应用的数据库版本字段；内部格式版本保存在 `_lithograph_meta`。

除 `lithograph_version()` 外，所有要求 graph state 的 API 在尚未执行 `lithograph_init()` 时返回 `NOT_INITIALIZED`。`lithograph_version()` 不要求先初始化，并分别返回 Extension version、ABI version、支持的 storage-format range 与当前 database 的 storage-format version；`main` 中不存在 Lithograph metadata 时当前 format 与 `databaseId` 为 `null`，更高但 marker 可读的 format 仍报告其实际版本。若 metadata 已存在但损坏，或 metadata 读取本身发生 `BUSY` / resource / I/O 等错误，则返回对应稳定 error，不把失败静默伪装成未初始化。

普通 graph/query/transaction API 的每次 invocation 只执行**结构性初始化校验**：验证 metadata marker、当前 storage-format 可理解、canonical internal-schema inventory / metadata shape 正确，并拒绝会截获 internal write 的 unsafe TEMP trigger。它们**不得**在每次调用前重算完整 Commit / Layer / Schema hash、遍历完整 Commit DAG 或扫描完整 graph 来证明 canonical history integrity；否则 query latency 会随 database 总规模线性增长并违反[Large-scale Invariants](runtime.md#large-scale-invariants) large-scale invariant。完整 canonical-history / graph integrity scan 由显式 `lithograph_integrity_check()`、初始化/迁移验收和其它明确要求完整 integrity evidence 的维护路径负责。普通执行仍依赖 canonical storage primitives 自身的 point/overlay invariant 校验，并在实际访问到损坏记录时 fail closed。

若 marker 可读但 `storageFormat` 低于当前 Extension 的 minimum supported format，只有存在该旧 format 到当前 format 的显式 migration path 时，`lithograph_init()` 才可以在[Storage Migration](storage.md#storage-migration) transaction contract 下迁移；没有已实现 migration path 时按内部格式不兼容/损坏返回 `STORAGE_ERROR`。`lithograph_version()` 不把这种低于 minimum 的状态当作正常可用 database 返回成功 JSON。高于 maximum 的可读 format 仍按前述规则报告实际版本，由需要理解内部语义的 API 返回 `FORMAT_TOO_NEW`。

SQL information/init API 返回 JSON object：

```text
lithograph_init()
  -> {databaseId, storageFormat, root, branch}

lithograph_version()
  -> {extension, abi, cypherProfile,
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

SELECT ordinal, row
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
| `lithograph_rows(query [, params [, options]])` | eponymous-only table-valued function，流式返回 `ordinal`、`columns` 与 `row` |
| `lithograph_validate(query)` | 只执行 parse + semantic/type/schema validation，不执行 query |
| `lithograph_version()` | 返回 Extension 与 storage-format version |
| `lithograph_integrity_check()` | 检查 Commit DAG、Branch ref、Layer hash、referential integrity 与内部表结构 |

`lithograph_rows` 的输入通过 hidden columns 实现，结果逐行拉取，不物化完整 result set。

`lithograph_rows` 的 `columns` 是 JSON string array，`row` 是按同一顺序排列的 JSON value array；`columns` 在每一结果行重复，因此即使存在相同显示名也不会丢失列位置。该 adapter 只接受**无副作用、无外部 I/O、无 connection-state mutation** 的 read-only query / procedure。Graph/schema/version mutation、`LOAD CSV`、Branch checkout、GC 或其它会产生 SQLite/OS/network side effect 的 query 返回 `READ_ONLY_ADAPTER`，避免 SQLite query planner 重复扫描 virtual table 时重复执行副作用。此类 Cypher 使用 `lithograph()` 的一次函数调用或 Native C ABI。

`lithograph()` 每一次 SQLite scalar-function invocation 都对应一次 Cypher execution。用于 mutating Cypher 时，调用方应把它作为独立的 `SELECT lithograph(...)` statement 调用；如果 SQL 本身产生多次 scalar invocation，每次 invocation 都是独立 Cypher write 并各自遵守[Transaction 与 Concurrency Model](storage.md#transactions-and-concurrency) transaction/Commit 语义。

`lithograph()` 返回完整 JSON envelope，因此受 SQLite 单值长度限制和宿主可用内存约束。结果可能较大时，read query 应使用 `lithograph_rows()` 或 Native streaming API；超过 SQLite/Engine 可用资源时返回 `RESOURCE_ERROR`，不得截断结果。

每个会产生 SQLite side effect 的 SQL Bridge invocation（包括 `lithograph_init()`、mutating `lithograph()` 和 version-ref mutation）必须创建唯一内部 SAVEPOINT。成功时 `RELEASE`，Lithograph error/panic/cancel 时先 `ROLLBACK TO` 再 `RELEASE`，然后才把 error 返回 SQLite。这样一次 invocation 不会留下半写 Layer/Commit/ref/schema。SQLite function callback 本身不能依赖“外层 SQL statement 失败会自动撤销递归写入”。

如果 host SQLite 因 authorizer、connection failure 或其它 SQLite-level failure 拒绝正常的 `ROLLBACK TO` / `RELEASE` cleanup，Lithograph 必须 fail closed：`ROLLBACK TO` 失败后不得继续 `RELEASE` 该 SAVEPOINT，因为最外层 SAVEPOINT 的 `RELEASE` 可能把本应撤销的变化提交；Engine 改为尝试整个 SQLite `ROLLBACK` 以清除未决 write 与 SAVEPOINT。该 recovery 在 caller-owned outer transaction 内也可能终止整个 outer transaction；这是无法完成 invocation-local cleanup 时优先保持 canonical storage 原子性的故障语义。只要进入 full-rollback fallback，本次 invocation 就返回 `INTERNAL_ERROR`，明确表示原 invocation-local transaction boundary 未能保持；若整个 `ROLLBACK` 也失败，仍返回 `INTERNAL_ERROR`，caller 应关闭并丢弃该 connection，不继续依赖其 transaction state。

`SQLITE_NOMEM` 是这个 cleanup-failure 分支的已验证宿主特例：如果错误来自 scalar callback 内部的 FTS5 tokenizer 构造，SQLite 会在该 callback 剩余期间保持 malloc-failed 状态，使 `ROLLBACK TO`、`RELEASE` 和 full `ROLLBACK` 都继续返回 `SQLITE_NOMEM`；外层 `sqlite3_step` / `sqlite3_exec` 返回后才可能再次执行 rollback。SQL Bridge 仍保留真实 `SQLITE_NOMEM` primary code；当 tokenizer probe 在 canonical Schema 持久化之前失败时，不得发布 Commit/Branch 变化。但由于 callback 内无法恢复 invocation-local transaction boundary，该 connection 属于上段定义的 cleanup-failure quarantine，caller 必须关闭并丢弃，不能把“外层返回后手动 rollback 可以成功”当成 Lithograph invocation 已满足 cleanup 合同。Native API 不受 scalar callback 的这个宿主限制，仍按自身 fail-closed transaction contract 返回结构化错误并清理 active explicit transaction。

多个 mutating `lithograph()` invocation 出现在同一个 raw SQL statement 时，语义明确为多个独立 SAVEPOINT/graph operations：在 SQLite autocommit mode 下，前一个成功 invocation 可以在后一个 invocation 失败前已经 durable；Lithograph 不承诺把整个宿主 SQL statement 合成一个 graph transaction。caller-owned SQLite `BEGIN ... COMMIT` 只能把多个已经形成的 Lithograph Commit 合并到同一个 durability boundary，不会折叠版本历史。调用方若需要“多个独立 Cypher execution 共同形成一个 Lithograph Commit”，必须使用[Native Explicit Transaction](storage.md#native-explicit-transaction)的 Native explicit transaction。推荐的 raw SQL write 形式始终是一个 statement 一个 `lithograph()` invocation。

<a id="native-c-abi"></a>

### Native C ABI

同一个 shared library 导出稳定、带 ABI version 的 Native API，供 Rust/Python/Node/其它 bindings 直接执行 Cypher。调用 Native ABI 前，该 shared library 必须已经通过 SQLite extension loading path 在目标 `sqlite3*` connection 上完成初始化注册。

```text
lithograph_v1_execute(...)
lithograph_v1_validate(...)
lithograph_v1_tx_begin(...)
lithograph_v1_tx_execute(...)
lithograph_v1_tx_commit(...)
lithograph_v1_tx_abort(...)
lithograph_v1_free(...)
```

`lithograph_v1_tx_*` 是 additive Native ABI family；它不改变既有 `lithograph_v1_execute` / `validate` symbol 的参数与生命周期。因为一个 `sqlite3*` 同时最多存在一个 Lithograph explicit transaction，v1 直接以 connection 作为 transaction identity，不再引入第二个 opaque handle / allocator / lifetime。ABI version 由 symbol name 中的 `v1` segment 固定；未来真正不兼容 ABI 必须新增 `lithograph_v2_*` symbols，不能改变以下 v1 contract：

```c
typedef enum lithograph_event_kind_v1 {
  LITHOGRAPH_EVENT_COLUMNS_V1 = 1,
  LITHOGRAPH_EVENT_ROW_V1 = 2,
  LITHOGRAPH_EVENT_SUMMARY_V1 = 3
} lithograph_event_kind_v1;

typedef int (*lithograph_event_callback_v1)(
  void *user_data,
  lithograph_event_kind_v1 kind,
  const unsigned char *json,
  size_t json_len
);

int lithograph_v1_execute(
  sqlite3 *db,
  const char *query, size_t query_len,
  const char *params_json, size_t params_len,
  const char *options_json, size_t options_len,
  lithograph_event_callback_v1 callback,
  void *user_data,
  char **error_json
);

int lithograph_v1_validate(
  sqlite3 *db,
  const char *query, size_t query_len,
  char **error_json
);

int lithograph_v1_tx_begin(
  sqlite3 *db,
  const char *options_json, size_t options_len,
  char **result_json,
  char **error_json
);

int lithograph_v1_tx_execute(
  sqlite3 *db,
  const char *query, size_t query_len,
  const char *params_json, size_t params_len,
  const char *options_json, size_t options_len,
  lithograph_event_callback_v1 callback,
  void *user_data,
  char **error_json
);

int lithograph_v1_tx_commit(
  sqlite3 *db,
  char **result_json,
  char **error_json
);

int lithograph_v1_tx_abort(
  sqlite3 *db,
  char **error_json
);

void lithograph_v1_free(void *ptr);
```

Native v1 pointer/length 规则与现有 `execute` 一致：可选 UTF-8 JSON 输入省略时使用 `NULL, 0`；非 `NULL` 输入在调用期间可读，Engine 在返回前复制。`result_json` / `error_json` 是可选 writable out-pointer；提供时调用开始先写 `NULL`。成功的 `tx_begin` / `tx_commit` 只通过 `result_json` 返回 NUL-terminated UTF-8 JSON；失败只通过 `error_json` 返回[Result 与 Error Contract](#results-and-errors)结构化错误。两类输出均由 Lithograph 分配，调用方使用 `lithograph_v1_free` 释放且只能释放一次。

`execute` 的 callback 事件顺序固定为 `COLUMNS` 一次、`ROW` 零到多次、`SUMMARY` 一次。`COLUMNS` payload 是 column-name JSON array；`ROW` 是同顺序 value array；`SUMMARY` 使用[Lithograph JSON](#lithograph-json) summary object。callback 返回非零值时 query 被取消并返回 `SQLITE_INTERRUPT`；若 query 已写入但尚未 durable commit，则整个 write rollback。

`error_json` 只在失败时设置，使用[Result 与 Error Contract](#results-and-errors) tagged JSON/error contract，由 Extension 分配并必须通过 `lithograph_v1_free` 释放。callback payload 只在 callback 调用期间有效，不需要也不能由 caller 释放。

Native API 接收现有 `sqlite3*`、Cypher text、parameter JSON、option JSON 和 event callback。普通 `lithograph_v1_execute` 是单 execution surface；`lithograph_v1_tx_begin/execute/commit/abort` 在同一个 connection-local explicit transaction 上执行，`tx_execute` 复用同一 parser/planner/executor，不建立第二套 Cypher engine。SQL Bridge 是面向普通 SQLite 客户端的 adapter，并调用同一个 Engine，但不加入 Native explicit transaction lifecycle。

`CALL { ... } IN TRANSACTIONS` 需要 Query Engine 拥有真实 transaction boundary。Native API 在没有 caller-owned active transaction 或 Native explicit transaction 时执行该语义。SQL Bridge 本身运行在一个进行中的 SQLite statement 内，因此遇到需要独立 commit boundary 的 `IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 时返回 `TRANSACTION_BOUNDARY_REQUIRED`，要求调用普通 Native execution；explicit transaction 内同样拒绝这类会再拥有独立 transaction boundary 的 query。其它 Cypher 语义不因此分叉。

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

Native explicit transaction 的 begin options 使用独立的 transaction-level shape：

```json
{
  "branch": "main",
  "expectedHead": "commit/<64-hex-id>",
  "author": "alice@example.com",
  "message": "apply one logical change"
}
```

- `branch` 省略时使用当前 connection active Branch；开始后 target Branch 固定，后续 `tx_execute` 不能切换 Branch；
- `expectedHead` 可省略；提供时只接受 resolved `commit/<id>`，并在取得 writer ownership 后与 target Branch 当前 head 原子比较，不一致返回 `BRANCH_HEAD_MOVED` 且 transaction 不开始；
- `author` / `message` 只属于最终唯一 Commit；各 `tx_execute` 不再接受自己的 Commit metadata；
- `tx_execute` 可以使用 query-local `graphView`，但 `branch` / `at` / `author` / `message` / `mergeSession` 等会选择另一 transaction/version context 的 options 返回 `INVALID_ARGUMENT`；
- Version Procedure、Branch/Tag/Commit Data mutation、checkout/GC，以及 `CALL { ... } IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 不能在 explicit transaction 内嵌套执行，返回 `TRANSACTION_BOUNDARY_REQUIRED`；
- `LOAD CSV` 和未来其它拥有 file/network external-I/O authority 的 query 同样不能在 explicit transaction 内执行，返回 `TRANSACTION_BOUNDARY_REQUIRED`。External I/O 继续由普通 execution / [Cypher Transaction Subqueries](storage.md#transaction-subqueries) batching 管理，避免从 `tx_begin` 起长期占用 SQLite single-writer ownership。普通 current-graph read/write 与 Schema/Constraint/Index command 仍可在 explicit transaction 内执行。

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

Native explicit transaction 已从 `tx_begin` 起持有 single-writer ownership，因此不接受 `LOAD CSV` 或 `db.index.semantic.query*` / `db.index.semantic.rebuild`；`tx_execute` 在 external I/O 开始前返回 `TRANSACTION_BOUNDARY_REQUIRED` 并按[Native Explicit Transaction](storage.md#native-explicit-transaction) fail-closed abort。需要批量导入时使用普通 `LOAD CSV` 或 Cypher `IN TRANSACTIONS`；需要 Semantic query/rebuild 时使用独立普通 execution，不把网络/文件/model 等待时间包进 multi-execution version-atomicity boundary。Semantic Index create/drop 的纯本地 Schema operation 不属于该 external-I/O 禁止项。

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

`summary.queryType` 使用封闭值 `read | write | schema | version | mixed`。普通 `lithograph()` / `lithograph_v1_execute` 中，`summary.commit` 是该 query 执行所 pin 的最终可观察 Snapshot：read query 为读取 Commit，graph/schema write 为新 Commit，ref-only version operation 为操作后的 active-branch Commit。Native explicit transaction 的 `tx_execute` statement 只作用于 staged state，因此其 `summary.commit = null`；最终 durable Commit 由 `tx_commit` 单独返回。Merge Session candidate query 同样不是 durable Commit，因此 `summary.commit = null`，并额外返回 `summary.mergeSession={"id":"merge-session/...","revision":N}`；其它 execution 省略 `mergeSession` 字段。`summary.counters` 至少固定包含：

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

`summary.metrics` 在 SQL Bridge scalar result 与 Native `SUMMARY` event 使用同一 shape，固定包含 `rows`、`dbHits`、`elapsedMicros`。普通 execution 也返回该字段；`PROFILE` 在不改变 query rows/value semantics 的前提下使用同一基础计量，并额外返回 `summary.profile`：

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

`lithograph_rows` 的 `row` 使用同一 value encoding，因此 scalar 与 streaming adapter 不产生两套结果语义。

`lithograph_rows` 的 `columns` 是 column-name array，`row` 是同顺序 value array；Native `COLUMNS` / `ROW` event 使用完全相同的两个 payload shape。空结果仍通过 Native `COLUMNS` event 暴露列信息；SQL table-valued adapter 的零行结果没有可携带 metadata 的 row，调用方如必须取得空结果的 columns，应使用 `lithograph()` envelope、`EXPLAIN`/metadata surface 或 Native API。

Cypher statement 只有显式结果生成 surface（例如最终 `RETURN` / procedure `YIELD`）才能产生公开 result columns / rows。以 graph/schema/version mutation clause 结束且没有最终结果投影的 statement 固定返回 `columns=[]`、`rows=[]`；executor 为执行后续 mutation 保存的内部 binding row 不属于 Result Contract，也不得经 SQL Bridge、streaming adapter 或 Native ABI 暴露。

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
READ_ONLY_ADAPTER
READ_ONLY_SNAPSHOT
FORMAT_TOO_NEW
BUSY
RESOURCE_ERROR
STORAGE_ERROR
IO_ERROR
INTERNAL_ERROR
```

结构化 error JSON 固定为：

```json
{
  "category": "TYPE_ERROR",
  "message": "...",
  "sqliteCode": 1,
  "line": 1,
  "column": 12
}
```

没有 query position 时 `line/column` 为 `null`。`sqliteCode` 使用 SQLite primary result code：parse/semantic/type/schema/version argument error 通常为 `SQLITE_ERROR`；ABI misuse 为 `SQLITE_MISUSE`；host lock contention 的 `SQLITE_BUSY/SQLITE_LOCKED` 映射 `BUSY` 并保留实际 primary code；`SQLITE_NOMEM/SQLITE_TOOBIG/SQLITE_FULL` 映射 `RESOURCE_ERROR`；`SQLITE_INTERRUPT` 保持 interrupt code；I/O/open/read-only-file 类错误映射 `IO_ERROR`。

Native API 返回 SQLite primary result code + 结构化 `error_json`；SQL Bridge 通常使用 `LITHOGRAPH_<CATEGORY>: <message>` 作为 SQLite error text，并通过 `sqlite3_result_error_code` 保留对应 primary code。SQLite 对少数 resource result code 有宿主级 canonicalization：已验证 `SQLITE_NOMEM` 在 scalar function 设置自定义 text 后仍会由 SQLite 对外改写成 `out of memory`。这类 code 不能为了保留 Lithograph 前缀而伪装成 `SQLITE_ERROR`；SQL Bridge 以真实 primary code 为权威，Native API 继续提供完整结构化 category/message。错误必须包含可定位 query position 时的 line/column，不把内部 SQLite table/schema 细节作为公开合同泄漏。
