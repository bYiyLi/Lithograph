# Lithograph 技术设计

本文是 Lithograph 的产品与技术设计真源。开发计划、阶段状态和验收记录位于 `docs/development/`。

## 1. 产品定义

Lithograph 是一个运行在标准 SQLite 上的、可加载的 Property Graph 数据库扩展。它在同一个 SQLite 数据库文件内提供三项一体化能力：

1. 完整的 Cypher 25 当前图查询与数据库语义；
2. 面向大规模单机图的 Property Graph 存储、执行、Schema、Index、Full-text 与 Vector Search；
3. Git / TerminusDB 风格的版本化图：每次写入形成不可变 Commit，支持 Branch、History、Time-travel、Diff、Patch、Merge、Rebase、Squash、Reset 与 Revert。

```text
Application / SQLite client
            |
            | SQLite loadable extension API
            | Native Lithograph C ABI
            v
+-------------------------------------------+
| Lithograph SQLite Extension               |
|                                           |
| Cypher 25 Frontend                        |
|        |                                  |
| Logical / Physical Planner                |
|        |                                  |
| Query Executor                            |
|        |                                  |
| Versioned Property Graph Storage          |
|   |            |              |           |
| Schema       Index/Search      History    |
+---|------------|--------------|-----------+
    v            v              v
                 SQLite
```

一个 SQLite database 对应一个 Lithograph logical graph。Branch 和 Commit 是这个 logical graph 的不同版本，不是独立数据库副本。

Lithograph 始终把 SQLite connection 的 `main` schema 视为当前 logical graph。`ATTACH DATABASE` 得到的其它 schema 不参与 Lithograph；要操作另一个 database 的 graph，调用方必须把该 database 作为新的 SQLite connection 的 `main` 打开。Lithograph 可以使用 SQLite `temp` schema 保存 query spill/cache，但 `temp` 永远不是 canonical graph state。

Lithograph 不包含 Knowledge、Ontology 业务模型、Agent、Memory、RAG 或其它调用方语义。调用方使用 Cypher 25 Graph Type、Label、Relationship Type 与 Property 定义自己的模型。

## 2. 设计驱动

### 2.1 SQLite 原生部署

Lithograph 必须是标准 SQLite loadable extension，不维护 SQLite fork，不要求独立 Server 或 Daemon。扩展通过 `sqlite3_lithograph_init` 注册公开接口，并使用 SQLite 自身事务、WAL、B-tree、文件格式与并发控制。

### 2.2 Cypher 25 语义兼容

Lithograph 不创建“类似 Cypher”的查询语言。公开图查询语言就是 Cypher 25。兼容要求以可观察语义为准，包括结果、类型、`null`、重复行、路径、更新、Schema、Constraint、Index、Search、错误和事务行为；仅解析相同语法但产生不同语义不算兼容。

### 2.3 版本化是存储基础，不是后加功能

版本历史从第一条图数据开始存在。图数据、Schema 与 Index 定义共同进入 Commit 历史。Branch 只移动引用，不复制完整数据库。历史 Commit 不被后续写入修改。

### 2.4 大规模单机图

遍历、索引、历史查询和结果返回不能依赖把完整图或完整结果集加载到内存。邻接访问的成本必须与命中的邻接数据相关，而不是与全部 Relationship 数量相关。

## 3. Cypher 25 兼容合同

### 3.1 Compatibility Profile

Lithograph 的首个完整兼容基线命名为 `CY25-2026.08`。

该 Profile 冻结以下来源截至 **2026-09-09** 可观察到的 Cypher 25 当前图能力：

- Neo4j Cypher Manual 的 Cypher 25 当前语言、值与类型、函数、查询、Pattern、Path、Mutation、Schema、Constraint、Index、Procedure 与 `LOAD CSV` 语义；
- Neo4j 2026.07 已发布 Cypher 25 语义；
- Neo4j Aura 2026.08 已公开的 Cypher 25 新能力，包括 native `UUID` 与 string interpolation。

Cypher 25 会继续演进。Lithograph 的“完整兼容”始终针对一个冻结 Profile 判断；新的 Cypher 25 能力通过新的 Compatibility Profile 升级，不改变旧 Profile 的测试基线。

查询中的 `CYPHER 25` prefix 被接受。Lithograph 不实现 Cypher 5 compatibility mode。

### 3.2 完整兼容范围

`CY25-2026.08` 覆盖当前 graph/database 语义：

- `MATCH`、`OPTIONAL MATCH`、`FILTER`、`WHERE`、`RETURN`、`WITH`、`LET`、`UNWIND`、`FOR`；
- `UNION`、`WHEN`、`NEXT`、subquery、subquery expressions、aggregation、`GROUP BY`、ordering 与 pagination；
- node / relationship patterns、quantified patterns、variable-length patterns、path selectors、shortest paths、match modes 与 path modes；
- `CREATE`、`INSERT`、`MERGE`、`SET`、`REMOVE`、`DELETE`、`DETACH DELETE`、`FOREACH`；
- Cypher 25 runtime value/type system、property types、temporal、spatial、`VECTOR`、`UUID`、list、map、node、relationship、path 与 `null` semantics；
- Cypher 25 built-in scalar、predicate、list、string、temporal、spatial、vector、aggregation 与 conversion functions；
- Graph Type、element type、property type、key、unique、existence / `NOT NULL` 与其它 current-graph constraints；
- lookup、range、text、point、full-text、vector indexes，以及 `SHOW INDEXES`；
- vector `SEARCH` subclause，以及 current Cypher full-text query procedures；
- current-graph `SHOW` surfaces，例如 functions、procedures、indexes、constraints 与 current graph type；
- `LOAD CSV`；
- `EXPLAIN` 与 `PROFILE`；
- `CALL { ... } IN TRANSACTIONS` 与 `IN CONCURRENT TRANSACTIONS` 的 query-engine semantics。

Neo4j DBMS 自身的多数据库管理、数据库 alias、用户/角色/权限、cluster/server、OIDC/ABAC、Java UDF 部署和系统数据库管理命令属于 Neo4j 产品管理面，不属于 Lithograph 的 current-graph Cypher compatibility Profile。`USE`、`graph.byName()`、`graph.names()` 等依赖 DBMS/composite-database graph selection 的 surface 同样不进入该 Profile；Lithograph 的 version selection 使用第 4/10 节的 Branch/Commit context，而不是伪装成 Neo4j composite database。

### 3.3 Compatibility Oracle

兼容性验证使用三层证据：

1. openCypher TCK 作为继承语义基线；
2. `CY25-2026.08` feature matrix 对 Cypher 25 新增/改变语义逐项建立 fixture；
3. Neo4j 对同一 fixture 的可观察结果作为差异核对 oracle，涉及 Neo4j 专有管理面时不进入 current-graph Profile。

GraphQLite v0.6.0 已证明 SQLite extension + Cypher + openCypher TCK 路线可行，并达到 97.7% openCypher TCK；Lithograph 复用其测试方法和实现经验，但目标是 `CY25-2026.08` 的 100% applicable matrix，而不是 GraphQLite 当前 coverage。

## 4. SQLite Extension 接口

### 4.1 加载与初始化

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

首次初始化生成一个 RFC 9562 UUID 作为 `databaseId`，保存在 `_lithograph_meta`，在该 database 的整个生命周期和 storage migration 中保持不变。`storageFormat` 首版固定为 `1`。

重复执行 `lithograph_init()` 是幂等的。数据库格式高于当前 Extension 可理解版本时直接返回 `FORMAT_TOO_NEW`，不得自动降级或重写历史。

`_lithograph_meta` 中必须存在 Lithograph magic marker、`databaseId` 与 `storageFormat` 才视为已初始化。如果数据库里已经存在任意 `_lithograph_*` user object，但缺少有效 marker，`lithograph_init()` 返回 `STORAGE_ERROR`，不覆盖、不删除、不迁移这些对象。Lithograph 不依赖宿主 `PRAGMA foreign_keys` 是否开启来维持内部正确性；所有 canonical referential invariants 由 Engine 明确验证，SQLite physical constraints 只作为附加防线。

“未初始化”只表示 `main` 中既不存在有效 `_lithograph_meta`，也不存在任何 reserved internal-schema evidence。若 metadata 缺失、被重命名或损坏，但 `main` 中仍存在 reserved object / canonical-internal child object，则该 database 属于可检测的损坏/冲突状态而不是 pristine uninitialized：`lithograph_init()` 与 `lithograph_version()` 返回 `STORAGE_ERROR`；`lithograph_integrity_check()` 在仍可完成检查时返回 `ok: false` 与结构化 `STORAGE_ERROR`。

Lithograph 不使用 `PRAGMA user_version`，避免占用宿主应用的数据库版本字段；内部格式版本保存在 `_lithograph_meta`。

除 `lithograph_version()` 外，所有要求 graph state 的 API 在尚未执行 `lithograph_init()` 时返回 `NOT_INITIALIZED`。`lithograph_version()` 不要求先初始化，并分别返回 Extension version、ABI version、支持的 storage-format range 与当前 database 的 storage-format version；`main` 中不存在 Lithograph metadata 时当前 format 与 `databaseId` 为 `null`，更高但 marker 可读的 format 仍报告其实际版本。若 metadata 已存在但损坏，或 metadata 读取本身发生 `BUSY` / resource / I/O 等错误，则返回对应稳定 error，不把失败静默伪装成未初始化。

若 marker 可读但 `storageFormat` 低于当前 Extension 的 minimum supported format，只有存在该旧 format 到当前 format 的显式 migration path 时，`lithograph_init()` 才可以在第 14.3 节 transaction contract 下迁移；没有已实现 migration path 时按内部格式不兼容/损坏返回 `STORAGE_ERROR`。`lithograph_version()` 不把这种低于 minimum 的状态当作正常可用 database 返回成功 JSON。高于 maximum 的可读 format 仍按前述规则报告实际版本，由需要理解内部语义的 API 返回 `FORMAT_TOO_NEW`。

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

### 4.2 SQL Bridge

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

`lithograph()` 每一次 SQLite scalar-function invocation 都对应一次 Cypher execution。用于 mutating Cypher 时，调用方应把它作为独立的 `SELECT lithograph(...)` statement 调用；如果 SQL 本身产生多次 scalar invocation，每次 invocation 都是独立 Cypher write 并各自遵守第 9 节 transaction/Commit 语义。

`lithograph()` 返回完整 JSON envelope，因此受 SQLite 单值长度限制和宿主可用内存约束。结果可能较大时，read query 应使用 `lithograph_rows()` 或 Native streaming API；超过 SQLite/Engine 可用资源时返回 `RESOURCE_ERROR`，不得截断结果。

每个会产生 SQLite side effect 的 SQL Bridge invocation（包括 `lithograph_init()`、mutating `lithograph()` 和 version-ref mutation）必须创建唯一内部 SAVEPOINT。成功时 `RELEASE`，Lithograph error/panic/cancel 时先 `ROLLBACK TO` 再 `RELEASE`，然后才把 error 返回 SQLite。这样一次 invocation 不会留下半写 Layer/Commit/ref/schema。SQLite function callback 本身不能依赖“外层 SQL statement 失败会自动撤销递归写入”。

如果 host SQLite 因 authorizer、connection failure 或其它 SQLite-level failure 拒绝正常的 `ROLLBACK TO` / `RELEASE` cleanup，Lithograph 必须 fail closed：`ROLLBACK TO` 失败后不得继续 `RELEASE` 该 SAVEPOINT，因为最外层 SAVEPOINT 的 `RELEASE` 可能把本应撤销的变化提交；Engine 改为尝试整个 SQLite `ROLLBACK` 以清除未决 write 与 SAVEPOINT。该 recovery 在 caller-owned outer transaction 内也可能终止整个 outer transaction；这是无法完成 invocation-local cleanup 时优先保持 canonical storage 原子性的故障语义。只要进入 full-rollback fallback，本次 invocation 就返回 `INTERNAL_ERROR`，明确表示原 invocation-local transaction boundary 未能保持；若整个 `ROLLBACK` 也失败，仍返回 `INTERNAL_ERROR`，caller 应关闭并丢弃该 connection，不继续依赖其 transaction state。

多个 mutating `lithograph()` invocation 出现在同一个 raw SQL statement 时，语义明确为多个独立 SAVEPOINT/graph operations：在 SQLite autocommit mode 下，前一个成功 invocation 可以在后一个 invocation 失败前已经 durable；Lithograph 不承诺把整个宿主 SQL statement 合成一个 graph transaction。因此需要多次 Cypher write 的原子事务必须使用 caller-owned SQLite `BEGIN ... COMMIT` 或 Native API。推荐的 raw SQL write 形式始终是一个 statement 一个 `lithograph()` invocation。

### 4.3 Native C ABI

同一个 shared library 导出稳定、带 ABI version 的 Native API，供 Rust/Python/Node/其它 bindings 直接执行 Cypher。调用 Native ABI 前，该 shared library 必须已经通过 SQLite extension loading path 在目标 `sqlite3*` connection 上完成初始化注册。

```text
lithograph_v1_execute(...)
lithograph_v1_validate(...)
lithograph_v1_free(...)
```

稳定 ABI v1 使用以下 C contract；ABI version 由 symbol suffix `_v1_` 固定。未来不兼容 ABI 必须新增 `_v2_` symbols，不能改变 v1 symbol 的参数或生命周期：

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

void lithograph_v1_free(void *ptr);
```

`execute` 的 callback 事件顺序固定为 `COLUMNS` 一次、`ROW` 零到多次、`SUMMARY` 一次。`COLUMNS` payload 是 column-name JSON array；`ROW` 是同顺序 value array；`SUMMARY` 使用第 13.1 节 summary object。callback 返回非零值时 query 被取消并返回 `SQLITE_INTERRUPT`；若 query 已写入但尚未 durable commit，则整个 write rollback。

`error_json` 只在失败时设置，使用第 13 节 tagged JSON/error contract，由 Extension 分配并必须通过 `lithograph_v1_free` 释放。callback payload 只在 callback 调用期间有效，不需要也不能由 caller 释放。

Native API 接收现有 `sqlite3*`、Cypher text、parameter JSON、option JSON 和 event callback。它是完整 Cypher execution surface；SQL Bridge 是面向普通 SQLite 客户端的 adapter，并调用同一个 Engine。

`CALL { ... } IN TRANSACTIONS` 需要 Query Engine 拥有真实 transaction boundary。Native API 在没有 caller-owned active transaction 时执行该语义。SQL Bridge 本身运行在一个进行中的 SQLite statement 内，因此遇到需要独立 commit boundary 的 `IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 时返回 `TRANSACTION_BOUNDARY_REQUIRED`，要求调用 Native API；其它 Cypher 语义不因此分叉。

### 4.4 Query Options

公开 options JSON 使用以下稳定字段：

```json
{
  "branch": "main",
  "at": "commit/<64-hex-id>",
  "author": "alice@example.com",
  "message": "update graph"
}
```

规则：

- `branch` 临时选择本 query 的 Branch；省略时使用当前 connection checkout 的 Branch；
- `at` 选择只读历史 Snapshot，接受 `commit/<id>` 或 `branch/<name>`；使用 `at` 的 query 不允许修改；
- `author` 和 `message` 是该 execution 内所有新 Commit 的 metadata 来源；省略时存 `null`。Version procedure 不定义第二套 author/message 来源；
- `branch` 与 `at` 互斥。

对 Version Procedure：`branch` query option 只为“对当前 Branch 操作”的 procedure 临时选择 target（`patch.apply`、`merge`、`rebase`、`squash`、`reset`、`revert`）；它不永久改变 connection checkout。`branch.create/delete/checkout/list` 自己显式指定或管理 Branch，和 query-level `branch` option 同时出现时返回 `INVALID_ARGUMENT`。任何 version mutation 与 `at` 同时出现都返回 `READ_ONLY_SNAPSHOT`。

Parameters JSON 与 result JSON 共用第 13.1 节 tagged-value encoding。普通 JSON primitive/list/map 直接映射到对应 Cypher value；需要保留 INTEGER64 边界、Temporal、Point、Vector 或 UUID 类型时必须使用 `$type` tagged form。

## 5. Property Graph 与 Value Model

### 5.1 Graph Elements

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

### 5.2 Identity

内部 `NodeId` 和 `RelationshipId` 使用正 `INTEGER64`，在整个 SQLite database 的全部 Branch / Commit 范围内全局分配且从不复用已提交 identity。

外部 `elementId()` 使用稳定字符串：

```text
n:<decimal-node-id>
r:<decimal-relationship-id>
```

Branch 与 Time-travel 不改变同一历史 element 的 `elementId`。

Label、Relationship Type 与 Property Key 使用 append-only integer dictionary encoding。Dictionary 中存在一个名称不代表该名称在所有 Snapshot 中可见；可见性由 Snapshot 数据决定。

### 5.3 Runtime Values 与 Persistent Properties

Query runtime 使用完整 Cypher 25 value model，包括 `NULL`、Boolean、Integer、Float、String、List、Map、Node、Relationship、Path、Temporal、Duration、Point、Vector 与 UUID。

持久化 Property 只接受 Cypher 25 允许的 property value types。Map、Node、Relationship、Path 等 constructed/structural runtime value 不作为 Property value 持久化。

`SET n.key = null` 与对应 Relationship 操作表示删除该 Property，而不是持久化一个 `NULL` property slot。

Vector 按原始 coordinate type 与 dimension 保存，不用 JSON list 替代，因此整数宽度、浮点精度与 dimension 可被 Schema 和 Search 正确验证。

## 6. Engine Structure

```text
Cypher text + parameters + version context
                  |
                  v
+---------------- Frontend ----------------+
| Lexer / Parser -> AST -> Semantic Analyze |
| Scope -> Type -> Schema validation        |
+--------------------|----------------------+
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

## 7. Query Planning 与 Execution

### 7.1 Logical Operators

Planner 至少覆盖以下 operator families：

- scan / seek：Node、Label、Relationship Type、Property Index、Full-text、Vector；
- graph expansion：`ExpandAll`、`ExpandInto`、variable/quantified expansion、shortest/path-selector operators；
- row operations：Filter、Project、Let、Unwind/For、Aggregate、Distinct、Sort、Skip、Limit；
- composition：Union、Apply、Optional、Semi/Anti、Cartesian、Subquery、When、Next；
- write：Create Node/Relationship、Set/Remove、Delete/Detach、Merge、Schema/Index changes；
- control：Eager/materialization barrier、transaction batch boundary、profile instrumentation。

### 7.2 Physical Planning

Physical Planner 使用规则重写与 cost estimate 联合选择访问路径。统计信息至少包括：

- Node / Relationship 总量；
- label / type cardinality；
- label/type 组合 cardinality；
- property index distinct/cardinality；
- relationship degree distribution；
- Search index metadata。

Planner 可以把 filter、projection、typed comparison 与部分 index seek 下推给 SQLite，但不能为了 SQL 转译便利改变 Cypher semantics。复杂 path、merge、version snapshot 与 semantic barriers 由 Lithograph executor 原生执行。

### 7.3 Streaming

Executor 使用 row pipeline。没有 `ORDER BY`、global aggregation、`DISTINCT` 或其它 semantic materialization 要求时，结果按 batch streaming，不随总结果行数线性占用内存。

`ORDER BY`、aggregation、eager write barrier 等需要物化时，可以使用内存并在超过内部预算后 spill 到 SQLite TEMP storage。

### 7.4 Snapshot Pinning

每个 query 在执行开始时解析 Branch / Commit 并 pin 到一个 immutable Commit。Branch head 在 query 执行期间发生变化不会改变该 query 已看到的 Snapshot。

### 7.5 Cancellation 与 Connection State

Executor 在 batch/operator boundary 与长路径/搜索循环中检查 SQLite interrupt state；host 调用 `sqlite3_interrupt()` 后 query 尽快停止并返回 `SQLITE_INTERRUPT`。Native event callback 返回非零是同一 cancellation 语义的另一入口。

Active Branch、temporary query options、prepared-plan/cache handle、current error/cancellation state 全部属于单个 `sqlite3*` connection 或单个 query。禁止使用 process-global mutable query/branch/parser state。跨线程使用同一 `sqlite3*` 是否允许完全遵循 host SQLite threading mode；Lithograph 不为一个不允许并发使用的 connection 增加第二套线程安全保证。

## 8. Version-aware Storage Model

### 8.1 Canonical History

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

### 8.2 Internal Tables

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

`LCE1` 属于 storage-format contract。修改上述编码必须提升 storage format，并通过 migration 保持旧 Commit ID 可验证。

### 8.3 Physical Access Indexes

Canonical delta 与 checkpoint 至少建立以下 B-tree access paths；这是大规模遍历正确实现的一部分，不是可选优化：

```text
relationship delta:
  (layer_id, source_id, type_id, target_id, relationship_id)
  (layer_id, target_id, type_id, source_id, relationship_id)
  (layer_id, relationship_id)

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

### 8.4 Content Addressing

Layer、Schema object 与 Commit 使用 256-bit BLAKE3 content hash。

- Layer hash 基于按 logical key canonical sort 后的 delta bytes；
- Schema hash 基于 canonical schema representation；
- Commit hash 基于 `format_version + parent IDs + layer hash + schema hash + metadata`；
- Commit ID 对外为 64 个 lowercase hex characters。

`format_version` 是 hash input 的一部分。后续 storage migration 不允许静默重算既有 Commit ID。

`committed_at` 使用 UTC Unix epoch microseconds，由成功创建 Commit 的 Engine connection clock 取得。它属于 Commit metadata 和 hash input，但不作为 DAG ancestry 或 merge correctness 的依据。

### 8.5 Snapshot Resolution

Snapshot Resolver：

1. 定位目标 Commit；
2. 找到 first-parent ancestry 中最近 checkpoint；
3. 使用 checkpoint 作为 immutable base；
4. 按祖先到目标顺序叠加后续 delta layer；
5. 形成 query-local overlay，并将目标 Commit 作为 Snapshot identity。

Root Commit 的 base 是空图。

Checkpoint 是可删除、可重建的 derived cache，不改变 Commit DAG。创建 checkpoint 不生成用户可见 Commit。

### 8.6 Checkpoint 与 Derived Cache

Lithograph 自动根据 delta-chain 深度、累计 delta 体量和 query cost 创建 checkpoint；这些阈值属于 semantic-neutral performance policy，不是持久化兼容合同。

Statistics、range/text index materialization、FTS index、vector HNSW graph 与 Snapshot overlay cache 同样属于 derived data。删除它们不得改变 query result，只影响性能；缺失时必须能够从 canonical history 重建或回退到正确的 scan。

## 9. Transaction 与 Concurrency Model

### 9.1 SQLite Transaction 是 durability boundary

所有 canonical graph change、Commit row、Layer、Schema object 与 Branch head move 必须处于同一个 SQLite transaction。任何一步失败，整个 write 不可见。

默认 auto-commit 模式下，一个成功的 top-level mutating Cypher query：

```text
pin active branch head
    -> execute against pinned snapshot
    -> validate constraints
    -> canonicalize delta
    -> write immutable layer/commit
    -> compare-and-move branch head
    -> SQLite COMMIT
```

上图的 `SQLite COMMIT` 对 Native API 表示 Engine 自己拥有的 transaction commit；对 SQL Bridge 表示内部 SAVEPOINT 成功 release 后，由宿主 SQLite autocommit/outer transaction 决定最终 durability。SQL Bridge 不能从 function callback 提前 commit caller-owned transaction。

每个成功的 **graph / Schema / Index mutating query** 都产生一个 Commit，即使 effective delta 为空；这样 graph history 与 write intent 一致。Version ref/control procedure 不一概产生 Commit：`branch.create/delete`、`reset` 与 fast-forward merge 只原子修改 ref，`branch.checkout` 只修改 connection-local context，`gc` 只做 reachability cleanup；`patch.apply`、non-fast-forward `merge` 与 `revert` 会产生 Commit。

### 9.2 Caller-owned SQLite Transaction

如果调用方已经 `BEGIN` SQLite transaction，同一 connection 内每个 mutating Cypher query 仍产生独立逻辑 Commit 并连续移动 Branch head，但这些 Commit 与 ref move 只有在外层 SQLite `COMMIT` 后才对其它 connection 可见。外层 `ROLLBACK` 会移除这一 transaction 中创建的全部 graph Commits。

### 9.3 Stale Branch Head

Write 在开始时记录 base Commit，在写 Branch ref 前再次 compare current head。若同一 Branch 已被其它 writer 移动，当前 write 失败为 `BRANCH_HEAD_MOVED`；Lithograph 不自动把两个并发写隐式 merge。

### 9.4 Readers and Writers

Read query pin immutable Commit，因此不会读取半完成 Layer。SQLite 的 concurrency mode 决定物理锁与 WAL 行为；Lithograph 不增加第二套 lock manager。

不同 connection 可以同时 checkout 不同 Branch。SQLite 仍是单文件 write serialization 的最终仲裁者。

### 9.5 Cypher Transaction Subqueries

Native API 在 caller 没有 active transaction 时实现 `CALL { ... } IN TRANSACTIONS`：每个 batch 对应独立 SQLite transaction 与一个或多个按 query semantics 产生的 graph Commits。

精确规则：每个成功且发生 graph/schema/index mutation 的 batch 创建 **一个** graph Commit；read-only batch 不创建 Commit。某个 batch 失败时仅该 batch transaction rollback；后续 batch 是否继续、状态列和最终 query error 按冻结 Cypher Profile 的 `ON ERROR` / status semantics 执行，因此已经 durable commit 的成功 batch 不被后续独立 batch failure 回滚。

`IN CONCURRENT TRANSACTIONS` 可以并行执行不需要 SQLite write lock 的 parse/parameter/materialization preparation，但同一 active Branch 的真正 batch transaction 从“pin latest Branch head”开始进入 Lithograph branch commit coordinator，按获得 coordinator 的顺序执行并最终由 SQLite 串行 durable commit。内部 concurrent batches 因此不会互相触发 `BRANCH_HEAD_MOVED`；每个 batch 都从进入自身 transaction 时的最新 Branch head 开始。外部 writer 在某 batch pin head 后移动同一 Branch 时，该 batch 仍按 9.3 返回 `BRANCH_HEAD_MOVED`。`DISJOINT BY` 等 Cypher 25 semantics 由 executor 保证。

## 10. Versioned Graph Model

### 10.1 Root、Commit 与 Branch

`lithograph_init()` 创建 Root Commit，`main` 指向 Root。

Branch 是唯一可变的版本引用：

```text
main -> C3
feature -> F2
```

创建 Branch 只新增一个 ref，初始指向指定 Branch/Commit Snapshot，不复制图数据。

每个 connection 有一个 active Branch，默认 `main`。`checkout` 只改变 connection-local execution context，不改 graph data。

Branch name 使用 case-sensitive UTF-8 bytes，长度为 1..255 bytes；禁止 NUL、ASCII control characters、开头或结尾 `/`、空 path segment，以及 segment `.` / `..`。`/` 可以用于层级命名。`main` 是 init 创建的保留 Branch，不能删除或重命名。删除其它 Branch 不删除 Commit；如果另一 connection 仍 checkout 已删除 Branch，它的下一次依赖 active Branch 的 query 返回 `BRANCH_NOT_FOUND`，直到 checkout 一个存在的 Branch。

### 10.2 Version Descriptor

所有 version-aware API 使用无歧义 descriptor：

```text
branch/<name>
commit/<64-hex-id>
```

历史 `commit/<id>` Snapshot 永远只读。要从历史状态继续写入，先从该 Commit 创建 Branch。

### 10.3 Version Procedures

版本管理通过 Cypher procedure 提供，不增加自定义 grammar：

```text
CALL lithograph.branch.create(name [, from])
CALL lithograph.branch.checkout(name)
CALL lithograph.branch.list()
CALL lithograph.branch.delete(name)
CALL lithograph.log([version [, limit]])
CALL lithograph.diff(before, after)
CALL lithograph.patch.apply(patch)
CALL lithograph.merge(source [, options])
CALL lithograph.rebase(onto [, options])
CALL lithograph.squash(since)
CALL lithograph.reset(target)
CALL lithograph.revert(commit [, options])
CALL lithograph.gc()
```

Procedure result 是普通 Cypher rows，因此可以与 `YIELD` / `RETURN` 组合。

参数规则：

- `branch.create(name, from)`：`from` 省略时使用 active Branch head；否则接受 version descriptor；
- `branch.checkout(name)`：只接受 Branch name；调用时 SQLite connection 必须处于 autocommit mode，否则返回 `TRANSACTION_BOUNDARY_REQUIRED`，避免 connection-local checkout 与 caller rollback 脱节；
- `branch.delete(name)`：不能删除 `main`，也不能删除当前 connection 的 active Branch；
- `log(version, limit)`：`version` 省略时使用 active Branch；
- `diff(before, after)`：两个参数都必须是 version descriptor；
- `patch.apply(patch)`：Commit author/message 使用 execution-level query options；
- `merge(source, options)`：`source` 接受 Branch/Commit descriptor；procedure options 只支持 `resolutions`；Commit author/message 使用 execution-level query options；
- `rebase(onto, options)`：把 active Branch 在 merge-base 之后的 first-parent commit sequence 逐个 replay 到 `onto`；procedure options 只支持 `resolutions`；replayed Commit 默认保留各自旧 author/message，不使用 execution-level author/message 覆盖历史 intent；
- `squash(since)`：`since` 必须是 active Branch head 的 ancestor descriptor，把 `since..HEAD` 的最终结构化变化压成一个新 Commit；新 Commit author/message 使用 execution-level query options；
- `reset(target)`：把 active Branch ref 移到 target descriptor 当前解析出的 Commit；
- `revert(commit, options)`：commit 必须是 `commit/<id>`；procedure options 只支持 `mainline`；Commit author/message 使用 execution-level query options；
- `gc()`：没有参数。

`merge/rebase.options.resolutions` 是 list of map：

```text
[
  {conflictId: "...", choice: "ours"},
  {conflictId: "...", choice: "theirs"},
  {conflictId: "...", choice: "value", value: <typed Cypher value>}
]
```

未知 conflictId、重复 conflictId、非法 choice 或与 slot 类型不匹配的 explicit value 都返回 `INVALID_ARGUMENT`，且不写任何 ref/Commit。

公开 procedure 结果合同：

| Procedure | 结果 |
| --- | --- |
| `branch.create` | `name, commit` 一行 |
| `branch.checkout` | `name, commit` 一行 |
| `branch.list` | 每个 Branch 一行 `name, commit, active`，按 name binary ascending |
| `branch.delete` | `name, previousCommit` 一行 |
| `log` | 每个 Commit 一行 `commit, parents, author, message, committedAt` |
| `diff` | 一行 `patch` map |
| `patch.apply` | 一行 `commit` |
| `merge` | 一行 `status, commit, conflicts`；冲突时 `commit = null` |
| `rebase` | 一行 `status, commit, rewritten, conflicts`；冲突时 `commit = null` |
| `squash` | 一行 `from, previousHead, commit` |
| `reset` | 一行 `from, to` |
| `revert` | 一行 `commit` |
| `gc` | 一行 deleted Commit/Layer/cache counters |

`log(version, limit)` 默认遍历该 version 可达的 DAG，按 reverse-topological order 返回；同一 topology level 先按 `committed_at` descending，再按 Commit ID ascending，保证结果确定。`limit` 必须为正整数；省略表示全部可达 Commit。

### 10.4 Diff 与 Patch

Diff 是结构化 graph patch，不是 SQL row diff 或文本 diff。Patch operation 的封闭集合：

```text
AddNode
DeleteNode
AddLabel
RemoveLabel
AddRelationship
DeleteRelationship
SetProperty
RemoveProperty
SetSchema
CreateIndex
DropIndex
```

Patch 是一个 map：

```text
{
  format: 1,
  databaseId: "<uuid>",
  from: "commit/...",
  to: "commit/...",
  operations: [ ... ]
}
```

每个 operation 都包含 `op`、stable logical slot，以及该 operation 所需的 typed `before` / `after`。`patch.apply` 只接受 `databaseId` 与当前 database 相同的 patch；跨 database patch/import 不属于当前合同。`from/to` 用于 provenance，不要求 active Branch 当前 head 等于 `from`，真正 applicability 由所有 `before` conditions 决定。

每个 operation 使用稳定 `elementId`、label/type/property name 和 before/after value 表示。`DETACH DELETE` 产生显式 Relationship deletions 与 Node deletion，因此 patch 可独立验证和重放。

`lithograph.diff(A, B)` 对任意 Branch / Commit 产生从 A 变为 B 的 canonical ordered patch。若 A 是 B 的 ancestor，可以直接 compose Layers；否则使用 Snapshot / merge-base 优化，但输出语义相同。

`lithograph.patch.apply` 在 active Branch 上验证 patch 的 `before` condition，全部成立后以一个新 Commit 原子应用；任一 condition 不成立则整体失败。

### 10.5 Three-way Merge

Merge 使用 Git 风格 three-way model：

```text
merge-base
   /   \
 ours  theirs
   \   /
  merge commit
```

目标 Branch 当前 head 是 `ours` / first parent；source 是 `theirs` / second parent。Merge result layer 相对 `ours` 保存。

Merge 开始时同时解析并 pin `ours` 与 `source` Commit。Source Branch 后续移动不改变本次 merge 已 pin 的 `theirs`；目标 Branch 在写入前仍执行 9.3 的 head compare，目标发生变化则返回 `BRANCH_HEAD_MOVED`。

默认 merge 行为与 Git 的普通 fast-forward 语义一致：

- `theirs == ours` 或 `theirs` 是 `ours` ancestor -> `status = up_to_date`，不写 Commit、不移动 Branch；
- `ours` 是 `theirs` ancestor -> fast-forward target Branch 到 `theirs`，`status = fast_forward`，不创建额外 Merge Commit；
- 其它 divergence -> 执行 three-way merge，成功后创建 two-parent Merge Commit，`status = merged`。

Merge-base 使用 Git-style “best common ancestors”：先找所有同时可达且不是另一 common ancestor 祖先的 best bases。只有一个时直接作为 base。存在多个 criss-cross best bases 时，按 Commit ID ascending 递归合成一个 **virtual base**；virtual-base merge 使用同一 logical-slot three-way rule，但冲突 slot 记录为内部 `unknown` sentinel。最终 merge 中，base 为 `unknown` 且 `ours != theirs` 时必须报告 conflict；`ours == theirs` 时可以自动接受该相同值。Virtual base 不写入 Commit DAG。

Conflict 的最小 logical slot：

- Node existence：`node/<id>`；
- Label membership：`node/<id>/label/<label>`；
- Relationship existence：`relationship/<id>`；
- Property：`node|relationship/<id>/property/<key>`；
- Schema / Constraint / Index object：对应 canonical schema identifier。

三方规则：

- 只有一侧相对 base 改变 -> 接受该侧；
- 两侧产生相同最终值 -> 自动合并；
- 两侧对同一 slot 产生不同最终值 -> conflict；
- delete-vs-modify -> conflict；
- 一侧删除 Node、另一侧新增或修改仍依赖该 Node 的 Relationship -> conflict；
- graph slot 虽无直接冲突，但 merged state 违反最终 Graph Type / Constraint -> constraint conflict。

存在 conflict 时 Merge **不写任何部分结果**，返回结构化 conflicts：`conflict_id + slot + base + ours + theirs`。

再次调用 `lithograph.merge` 时可提供 per-conflict resolution：`ours`、`theirs` 或显式 replacement value。全部 conflict 解决且 constraints 通过后才创建 two-parent Merge Commit。

Conflict ID 是对 `merge-base identity + ours commit + theirs commit + slot + base/ours/theirs canonical values` 的 BLAKE3 hash；同一 merge 输入重复执行得到相同 conflict ID。Branch head 在首次 conflict 计算后发生变化时，旧 resolution 不允许套用，返回 `BRANCH_HEAD_MOVED`。

### 10.6 Rebase

`rebase(onto)` 使用 Git-style commit replay，但以结构化 graph patch 为单位：

1. pin active Branch head 为 `oldHead`，pin `onto` Commit；
2. 使用 10.5 的 merge-base algorithm 得到 base；
3. 取 base（exclusive）到 `oldHead` 的 **first-parent** Commit sequence，按旧到新顺序 replay；
4. 对每个旧 Commit `C`，使用 `diff(parent1(C), C)` 作为该 Commit 的 intent；在当前 replay head 上以 `parent1(C)` / current replay head / `C` 做 logical-slot three-way application；
5. 无冲突时创建一个新的 single-parent Commit，保留旧 Commit 的 `author` / `message`，使用新的 `committed_at`；旧 Commit 如果是 Merge Commit，其 second-parent topology 默认被 flatten，replay 的是它相对 first parent 的实际 graph/schema change；
6. 全部旧 Commit replay 成功后才把 active Branch 原子移动到最后一个新 Commit。

整个 rebase 在一个 SQLite transaction 内 staged。任何 replay conflict、constraint violation、resource failure 或目标 Branch stale-head 都 rollback **全部新 Commit** 并保持 Branch 不变，不产生半完成 rebase。

Rebase conflict 使用与 merge 相同的 logical slot 和 `base/ours/theirs` shape，并额外包含 `sourceCommit`。`options.resolutions` 使用相同 conflictId resolution format。没有需要 replay 的 Commit 时返回 `status = up_to_date`；成功重写时 `status = rebased`，`rewritten` 按旧到新顺序返回 `{from,to}` pairs。

### 10.7 Squash

`squash(since)` 要求 `since` 是 active Branch head 的 ancestor。若 `since == HEAD`，返回 `INVALID_ARGUMENT`，因为没有 Commit 可 squash。

Squash 计算 `diff(since, HEAD)`，创建一个 parent=`since` 的新 single-parent Commit，Layer 表示该完整结构化变化，然后原子把 active Branch 移到新 Commit。原历史 Commit 保持 immutable；如果没有其它 Branch 引用，它们只是变成 unreachable，直到 explicit GC。

Squash 不把原 Commit author/message 列表嵌入新 Commit。新 Commit metadata 使用 execution-level query options 的 `author/message`；省略时为 `null`。Squash 后 graph/schema/index Snapshot 必须与原 HEAD 完全相同。

### 10.8 Reset、Revert 与 History

- `reset(target)` 原子移动 active Branch ref 到已有 Commit，不删除 Commit；
- `revert(commit)` 计算该 Commit 相对 parent 的 inverse patch，并在 active Branch 创建一个新 Commit；普通 Commit 固定使用 parent1；Merge Commit 必须通过 options 指定 `mainline: 1|2`，否则返回 `INVALID_ARGUMENT`；Root Commit 不能 revert；
- `log` 沿 Commit DAG 返回 id、parents、author、message、timestamp；
- 使用 `options.at = commit/<id>` 可 time-travel 查询任意历史 Snapshot。

Canonical history 不自动 GC。`lithograph.gc()` 只删除从任何 Branch 都不可达的 Commit / Layer；derived checkpoint/index/cache 可以自动回收，因为可重建。

GC 对 canonical objects 按 reachability 删除：Commit 不可达后，其 Layer 只有在没有其它 reachable Commit 引用时才删除；Schema object 同理。Dictionary identity/name 是 database-global append-only metadata，即使当前没有 reachable Snapshot 使用也不回收，避免 ID 重用和历史/patch 解释变化。

Lithograph 的 Git-like contract 是**单个 SQLite database 内的本地版本控制**。Commit/Branch/Diff/Merge/Rebase/Squash 等全部在同一个 `databaseId` 内工作。跨 SQLite database 或跨网络的 clone/fetch/push/pull 属于复制/传输层，不是 Lithograph Extension v1 的 Version Procedure contract；SQLite backup/file replication 可以复制整个 repository，但两个独立 `databaseId` 不通过 Version API 隐式合并 identity space。

## 11. Schema、Constraint 与 Index Model

### 11.1 Schema-free 与 Graph Type

Root Graph Type 是 empty/open，允许 schema-free data。Cypher 25 Graph Type 只约束其声明覆盖的数据，保持 Cypher 25 open graph type semantics。

Graph Type、standalone constraints 和 index definitions 共同构成 versioned Schema state。每个 Commit 直接引用完整 canonical Schema object hash，因此历史 Snapshot 自动看到当时的 Schema。

### 11.2 Schema Change

Schema command 在同一个 write transaction 中：

1. 构造新 Schema；
2. 验证当前 Snapshot 全部受影响数据；
3. 构建/更新所需 derived index；
4. 全部成功后创建 Commit 并移动 Branch。

已有数据不满足新 constraint 时整个 command 失败，旧 Schema 与 Branch head 保持不变。

### 11.3 Index 类型

Lithograph 实现 Cypher 25 current-graph index surface：

- lookup index；
- range index；
- text index；
- point index；
- full-text index；
- vector index。

Index definition 是 versioned Schema；physical index content 是 derived cache。历史 Snapshot 缺少对应 physical cache 时允许构建 cache 或使用正确但更慢的 fallback，不得返回错误结果。

### 11.4 Range / Text / Point

Range / Text / Point index 使用 SQLite B-tree-backed derived tables，key 编码保持 Cypher ordering、comparison、collation 与 type semantics。Planner 根据 statistics 选择 seek / scan。

### 11.5 Full-text

Full-text index 使用 SQLite FTS5 作为 backend。Lithograph 支持的 SQLite runtime 必须启用 FTS5。

对外使用 Cypher 25 full-text DDL 和 procedures，例如：

```text
db.index.fulltext.queryNodes(...)
db.index.fulltext.queryRelationships(...)
```

Analyzer、score、skip、limit 等 visible behavior 由 Cypher Profile 约束。FTS table 是 derived cache，不是历史真源。

### 11.6 Vector

Vector property 保留 dimension 与 coordinate type。Vector index 使用 HNSW ANN architecture，并支持 Cypher 25 vector index metadata、similarity、additional filtering properties 与 `SEARCH` subclause。

HNSW physical graph 是 derived cache，可以按 `(index definition, commit)` 重建。Vector cache 的缺失不能改变语义；没有 cache 时可以使用 exact scan 作为 correctness fallback。

## 12. LOAD CSV 与 External I/O

`LOAD CSV` 支持 `file://`、`http://` 与 `https://` source。读取权限继承宿主进程的 OS / network authority；Lithograph 不注入隐藏 credentials。

I/O error、malformed CSV、type/constraint error 按 Cypher query failure 传播。普通 `LOAD CSV` 位于当前 query transaction；`CALL ... IN TRANSACTIONS` 使用第 9.5 节 transaction semantics。

## 13. Result 与 Error Contract

### 13.1 Lithograph JSON

SQL Bridge 的完整 envelope：

```json
{
  "columns": ["name"],
  "rows": [["Alice"]],
  "summary": {
    "queryType": "read",
    "commit": "commit/...",
    "counters": {}
  }
}
```

`summary.queryType` 使用封闭值 `read | write | schema | version | mixed`。`summary.commit` 是该 query 执行所 pin 的最终可观察 Snapshot：read query 为读取 Commit，graph/schema write 为新 Commit，ref-only version operation 为操作后的 active-branch Commit。`summary.counters` 至少固定包含：

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

### 13.2 Error Categories

公开稳定 error categories：

```text
PARSE_ERROR
SEMANTIC_ERROR
TYPE_ERROR
SCHEMA_ERROR
CONSTRAINT_ERROR
NOT_INITIALIZED
INVALID_ARGUMENT
VERSION_NOT_FOUND
BRANCH_NOT_FOUND
BRANCH_HEAD_MOVED
MERGE_CONFLICT
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

Native API 返回 SQLite primary result code + 结构化 `error_json`；SQL Bridge 使用 `LITHOGRAPH_<CATEGORY>: <message>` 作为 SQLite error text，并通过 `sqlite3_result_error_code` 保留对应 primary code。错误必须包含可定位 query position 时的 line/column，不把内部 SQLite table/schema 细节作为公开合同泄漏。

## 14. Integrity、Recovery 与 Migration

### 14.1 Integrity Invariants

`lithograph_integrity_check()` 至少验证：

- 当前 storage format 的 canonical internal-schema inventory 完整且无额外 reserved object、大小写变体 collision 或未声明的 internal-table child trigger/index；
- Branch ref 指向存在 Commit；
- Commit parents、Layer、Schema object 均存在；
- Commit / Layer / Schema hash 可重算且匹配；
- Relationship endpoint 在对应 Snapshot 存在；
- dictionary ID 唯一且 name 唯一；
- checkpoint 与 derived index 声明的 Commit 可解析；
- internal storage format 与 Extension 兼容。

### 14.2 Crash Recovery

Canonical write 与 Branch move 共用 SQLite transaction，因此 crash 后只允许出现 commit 前状态或 commit 后状态，不存在 Branch 指向半写 Layer 的合法状态。SQLite recovery 完成后 Lithograph 再执行自身 metadata/integrity checks。

### 14.3 Storage Migration

Storage format version 记录在 `_lithograph_meta`。升级迁移必须：

- 在 SQLite transaction 中执行；
- 保持已存在 Commit ID 和 history semantics；
- 迁移失败整体 rollback；
- 新 Engine 继续读取历史 `format_version`；
- 旧 Engine 遇到更高 format version 直接拒绝写入和读取需要新格式语义的 graph。

## 15. Deployment 与 Runtime Boundary

### 15.1 Implementation Language 与 SQLite ABI

Lithograph core 使用 **Rust 2024 Edition**。SQLite loadable-extension boundary 使用 SQLite 官方 `sqlite3ext.h` / `sqlite3_api_routines` host API table，通过 Rust `rusqlite` 的 `loadable_extension` 支持和底层 `libsqlite3-sys` loadable-extension bindings 实现。

Extension artifact **不得**启用 bundled SQLite，也不得直接静态/动态链接一个私有 SQLite 副本来满足 Engine 调用。所有 SQLite API 调用必须解析到加载该 Extension 的 host SQLite API table；这样同一个 artifact 才能被 stock SQLite 以标准 `.load` 机制跨平台加载，并避免两个 SQLite runtime 同时操作同一 `sqlite3*`。

Rust 负责 parser/semantic model、planner、executor、version engine、typed values、storage abstraction、vector index 和 C ABI。允许生成 parser code 或依法复用 grammar，但不允许把外部 parser library 的 AST 作为 storage/executor 公共合同。首个实现依赖基线固定 `rusqlite 0.40.1` + `libsqlite3-sys` loadable-extension path；依赖升级只能在保持本节 ABI 不变量并通过最低 SQLite、当前 SQLite 与跨平台真实 load acceptance 后进行。

### 15.2 SQLite Baseline

支持基线是 SQLite **3.45.0+**，并要求：

- loadable extension support；
- FTS5；
- thread-safety 与 transaction behavior 符合 SQLite 官方公开 API。

Lithograph 不静默修改宿主的 journal mode、synchronous level 或其它 durability PRAGMA。

### 15.3 Supported Artifacts

发布矩阵：

```text
macOS   arm64, x86_64
Linux   x86_64, aarch64
Windows x86_64, arm64
```

所有平台共享同一 storage format、Cypher Profile 与 test corpus。

## 16. Security Boundary

Lithograph 是 embedded extension，没有独立 account / role / authentication layer。读取和写入数据库文件、`LOAD CSV` 文件、HTTP(S) 与加载 Extension 的权限都继承宿主进程和 SQLite connection。

执行 Cypher、初始化、migration、version mutation 和 integrity-maintenance 的 SQL entrypoints 注册为 direct-only surface，不能从持久化 trigger/view/schema expression 隐式触发。纯信息函数只有在确认无副作用后才可注册为 innocuous。

`_lithograph_*` 是内部表。直接 SQL 修改这些表不属于公开 API；由于数据库文件所有者最终拥有 SQLite 全权限，Lithograph 不伪装成能阻止文件所有者篡改，而通过 hash、referential integrity 和 `lithograph_integrity_check()` 检测损坏。

该 integrity contract 证明 database 内仍存在的 canonical evidence 是否自洽，不提供独立于数据库文件的 authenticity / anti-tamper trust anchor。拥有文件全权限的主体若删除全部 Lithograph internal evidence，结果在单文件内部与从未初始化的 pristine database 不可区分；若同时自洽重写全部 canonical data/hash，也不能仅靠同一文件证明其历史真实性。需要这种外部真实性保证的部署必须由宿主另行提供 trusted backup、signature/attestation 或其它外部锚点，Lithograph 不在 embedded database 内伪造该能力。

## 17. Large-scale Invariants

以下是架构不变量，不依赖某台机器的绝对 latency：

- 单 hop 邻接扩展使用 source/target/type 可寻址结构，不扫描全部 Relationship；
- equality/range indexed lookup 使用 index seek，不扫描全部 matching domain；
- streaming query memory 与 executor batch / semantic barrier 相关，不与最终 row count 线性增长；
- historical query 从 checkpoint + bounded overlay 解析，不要求从 Root 重放全部 history；
- derived index / checkpoint 可 rebuild，不阻塞 canonical history correctness；
- planner statistics 可以增量刷新，不能要求每个 query 扫描全图计算 cardinality；
- 10M Node / 100M Relationship benchmark tier 必须作为 release hardening 的真实规模验证，覆盖 traversal、indexed lookup、write、history、diff 与 search；通过条件是正确完成、无 OOM、无意外全图扫描，并建立可持续 regression baseline。

## 18. 关键架构决定与取舍

### D1 标准 SQLite Extension，不 Fork SQLite

- 决定：使用公开 loadable-extension API。
- 依据：产品目标是“SQLite 一个插件”，并保留 SQLite 发行版、工具和单文件生态。
- 备选：修改 SQLite parser / fork SQLite。
- 取舍：stock SQL console 不能直接新增顶层 `MATCH` grammar，因此通过 SQL Bridge 或 Native ABI 输入 Cypher。

### D2 Rust Core

- 决定：Rust 负责 Engine，C ABI 只作为 SQLite / binding boundary。
- 依据：需要 native extension、内存安全、复杂 parser/executor、并发与版本数据结构。
- 备选：纯 C/C++；直接延续 GraphQLite C architecture。
- 取舍：需要维护 C ABI wrapper，但避免把整个 Engine 的生命周期放在 C ownership 中。

### D3 原生 Planner/Executor，不把“Cypher -> SQL 文本”作为核心架构

- 决定：Cypher 先进入 typed logical/physical plan，再通过 Storage primitives 执行；安全的 scalar/index 部分可下推 SQLite。
- 依据：完整 Cypher 25 path、subquery、mutation、Search 与 version snapshot 很难以 clause-pattern SQL transpiler 保持一致语义和可优化性。
- 备选：复制 GraphQLite 的 SQL transpilation / dispatcher 架构。
- 取舍：Engine 工作量更大，但兼容面和优化边界清晰。

### D4 Version-aware Storage 从第一天存在

- 决定：所有 graph write 从 Root Commit 起写 immutable Layer，不先做 mutable graph 再改造成版本数据库。
- 依据：Identity、transaction、Schema、index、merge 和历史查询都会被版本模型改变。
- 备选：先实现当前状态，后续追加 audit log。
- 取舍：Storage foundation 更复杂，但避免后续重写数据模型。

### D5 Immutable History + Mutable Branch Ref

- 决定：Commit/Layer 永不更新；Branch 是移动指针；Checkpoint/index 为 derived cache。
- 依据：直接获得 TerminusDB/Git 风格 history、branch、time-travel、diff/merge 基础。
- 备选：在 mutable rows 上增加 `valid_from/valid_to`。
- 取舍：需要 Snapshot Resolver 和 checkpoint，但普通 Cypher 不被时间字段污染。

### D6 Schema 随数据版本化

- 决定：Commit 同时引用 Graph State 与完整 Schema hash。
- 依据：历史 Snapshot 必须在当时 Schema 下自洽，Branch merge 也必须合并 Schema 变化。
- 备选：Schema 始终使用数据库当前版本。
- 取舍：merge 增加 schema conflict 类型，但 time-travel 语义完整。

### D7 Full-text 使用 FTS5，Vector 使用 HNSW derived index

- 决定：FTS5 处理全文，HNSW 处理 ANN vector search；两者都不是 canonical graph truth。
- 依据：分别利用 SQLite 成熟全文能力和适合 `SEARCH` 的 ANN 结构。
- 备选：统一自研 Search engine；只使用 brute-force vector scan。
- 取舍：需要维护两类 derived index lifecycle，但可以从任意 Snapshot 重建并保持版本正确性。

### D8 Engine Compatibility 与 Adapter Capability 分离

- 决定：Native ABI 是完整 execution surface，SQL Bridge 是 stock SQLite adapter；两者共享同一 parser/planner/executor。
- 依据：SQLite SQL statement 内无法可靠取得 Cypher transaction-owning clause 所需的独立 commit boundary。
- 备选：fork SQLite 或通过隐式辅助连接绕过 host transaction。
- 取舍：极少数 transaction-boundary query 必须从 Native API 调用，但 Cypher Engine 本身仍按 Profile 完整实现。

## 19. 参考基线

外部项目只提供 evidence 和实现参考，不覆盖本文设计：

- SQLite Loadable Extensions: <https://www.sqlite.org/loadext.html>
- SQLite Virtual Table Mechanism: <https://www.sqlite.org/vtab.html>
- Cypher Manual: <https://neo4j.com/docs/cypher-manual/current/>
- Neo4j Aura August 2026 Cypher additions: <https://feedback.neo4j.com/changelog/neo4j-aura-august-2026-release>
- GraphQLite: <https://github.com/colliery-io/graphqlite>
- TerminusDB Version Control: <https://terminusdb.org/docs/version-control-operations/>
- TerminusDB architecture explanation: <https://terminusdb.org/docs/terminusdb-explanation/>
- Git merge semantics: <https://git-scm.com/docs/git-merge>

外部研究证据的快照与采用边界另见 `docs/research/reference-baseline.md`。
