# Lithograph 技术设计

本文是 Lithograph 的产品与技术设计真源。开发计划、阶段状态和验收记录位于 `docs/development/`。

Phase 00–12 均已实现并完成对应开发验收。Phase 11 已将第 7.8、8.3.1、11.7、14.3.1、17.1–17.3 节定义的性能合同落入当前实现；实施顺序、量化证据与完成状态见 [Phase 11](development/phases/11-performance-optimization.md)。

第 11.5 节的 FTS5 tokenizer 扩展已由 [Phase 12](development/phases/12-fulltext-tokenizer.md) 实现并完成开发验收。当前已发布的 v0.1.0 仍只有两个硬编码 analyzer，因此不能把本节目标当作 v0.1.0 已发布能力。

## 1. 产品定义

Lithograph 是一个运行在标准 SQLite 上的、可加载的 Property Graph 数据库扩展。它在同一个 SQLite 数据库文件内提供三项一体化能力：

1. 完整的 Cypher 25 当前图查询与数据库语义；
2. 面向大规模单机图的 Property Graph 存储、执行、Schema、Index、Full-text 与 Vector Search；
3. 基于 immutable Commit DAG 的版本化状态图：每个 graph / Schema / Index **逻辑写单元**形成 Commit；普通 auto-commit query 是一个写单元，Native explicit transaction 可以把多个独立 execution 组合成一个写单元。系统同时提供 Branch、Tag、可修改的 Commit Data、显式 empty-delta Commit、可分页 History、Time-travel、Diff、Patch、Merge、Rebase、Squash、Reset 与 Revert。Git / TerminusDB 是机制参考，不限定调用方如何解释这些状态。

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

Commit boundary 属于版本模型的一部分：普通 mutating Cypher execution 默认各自产生 Commit；需要把多个 execution 视为一次逻辑状态变化时，调用方使用 Native explicit transaction，在同一 staged Snapshot 上执行并只 finalize 一个 Commit。caller-owned SQLite transaction 只改变 durability visibility，Cypher `IN TRANSACTIONS` 只改变 batch transaction boundary，二者都不隐式重写 Commit 粒度。

Commit 的 graph / Schema / Index Snapshot 与 DAG lineage 是 immutable canonical history。调用方可以另外给已有 Commit 保存可修改的 **Commit Data**，也可以用 **Tag** 给 Commit 建立显式命名引用；Commit Data 与 Tag 都是 version-control sidecar state，不进入 Snapshot、Commit hash、Diff / Patch 或 Merge correctness。若某项业务数据本身必须随 Snapshot versioning、Cypher query、Constraint、Diff 或 Merge 一起演进，它必须保存为正常 graph / Schema 数据，而不是 Commit Data。

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

Lithograph 默认且只执行 Cypher 25。查询接受显式 `CYPHER 25` version prefix，也接受 Cypher 自身的 `CYPHER <query-options>` preamble；两者可以组合为 `CYPHER 25 <query-options>`。与 `EXPLAIN` / `PROFILE` 组合时，兼容 Cypher 25 当前可接受的 preamble 顺序。显式选择 `CYPHER 5` 或其它非 25 version 必须拒绝。这里的 Cypher query options 属于语言 frontend，与第 4.4 节通过 Extension API 传入的 Lithograph execution-level Query Options 是两套独立接口，不得互相解释或覆盖。

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

Neo4j DBMS 自身的多数据库管理、数据库 alias、用户/角色/权限、cluster/server、OIDC/ABAC、Java UDF 部署和系统数据库管理命令属于 Neo4j 产品管理面，不属于 Lithograph 的 current-graph Cypher compatibility Profile。`USE`、`graph.byName()`、`graph.names()` 等依赖 DBMS/composite-database graph selection 的 surface 同样不进入该 Profile；Lithograph 的 version selection 使用第 4/10 节的 Branch/Commit/Tag context，而不是伪装成 Neo4j composite database。

Lithograph 的 `graphView` execution context（第 4.4、7.7 节）是 Adapter / Engine 层对**同一个 Versioned Property Graph 的 query-local 可见子图**进行约束的通用能力，不属于 Cypher 25 grammar 或 compatibility Profile。它不得把 Cypher 25 `USE` 重新解释成子图过滤，也不得引入 Lithograph-specific Cypher clause。

Native explicit transaction（第 4.3、9.2 节）同样是 Lithograph execution/version boundary，不属于 Cypher 25 grammar 或 compatibility Profile。它只改变多个标准 Cypher execution 何时共同 finalize 为 Commit，不改变单个 execution 的 Cypher 语义，也不把 transaction lifecycle 注入 Cypher text。

Full-text 的 FTS5 provider binding 由第 11.5 节明确限定：沿用 Cypher 25 DDL、procedure、配置 key 和值类型，不新增 grammar；但 analyzer 字符串的取值、默认分词行为和评分数值依赖 backend。FTS5 tokenizer specification 不是 Neo4j analyzer 名称的可移植替代，不能仅凭相同字段名宣称两种引擎的分词、stop words 或相关性分数完全一致。Phase 12 对这一 binding 的破坏性调整单列在 compatibility inventory；不重写冻结 Cypher 语言 fixture 的预期结果来掩盖差异。

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

首次初始化生成一个 RFC 9562 UUID 作为 `databaseId`，保存在 `_lithograph_meta`，在该 database 的整个生命周期和 storage migration 中保持不变。Storage format `1` 是首个 canonical graph-storage baseline；加入 Commit Data / Tag sidecar 与 Merge Session operational storage 后，首个公开 release 的 current storage format 固定为 `2`。最终 Extension 对 fresh database 直接创建 format `2`；已存在 format `1` database 只能通过第 14.3 节定义的显式 `1 -> 2` migration 升级，既有 Commit ID 不重算。

上述 format `2` 是 Phase 10 已实现基线。Phase 11 为持久 derived Standard Index 增加 format `3`，其 fresh/init、旧格式读取、exact internal-schema inventory 与迁移边界由第 14.3.1 节统一定义；不能在 format `2` 中静默添加未声明的 reserved table/index。公开 C ABI 和 Cypher Profile 不因此升级。

重复执行 `lithograph_init()` 是幂等的。数据库格式高于当前 Extension 可理解版本时直接返回 `FORMAT_TOO_NEW`，不得自动降级或重写历史。

`_lithograph_meta` 中必须存在 Lithograph magic marker、`databaseId` 与 `storageFormat` 才视为已初始化。如果数据库里已经存在任意 `_lithograph_*` user object，但缺少有效 marker，`lithograph_init()` 返回 `STORAGE_ERROR`，不覆盖、不删除、不迁移这些对象。Lithograph 不依赖宿主 `PRAGMA foreign_keys` 是否开启来维持内部正确性；所有 canonical referential invariants 由 Engine 明确验证，SQLite physical constraints 只作为附加防线。

“未初始化”只表示 `main` 中既不存在有效 `_lithograph_meta`，也不存在任何 reserved internal-schema evidence。若 metadata 缺失、被重命名或损坏，但 `main` 中仍存在 reserved object / canonical-internal child object，则该 database 属于可检测的损坏/冲突状态而不是 pristine uninitialized：`lithograph_init()` 与 `lithograph_version()` 返回 `STORAGE_ERROR`；`lithograph_integrity_check()` 在仍可完成检查时返回 `ok: false` 与结构化 `STORAGE_ERROR`。

Lithograph 不使用 `PRAGMA user_version`，避免占用宿主应用的数据库版本字段；内部格式版本保存在 `_lithograph_meta`。

除 `lithograph_version()` 外，所有要求 graph state 的 API 在尚未执行 `lithograph_init()` 时返回 `NOT_INITIALIZED`。`lithograph_version()` 不要求先初始化，并分别返回 Extension version、ABI version、支持的 storage-format range 与当前 database 的 storage-format version；`main` 中不存在 Lithograph metadata 时当前 format 与 `databaseId` 为 `null`，更高但 marker 可读的 format 仍报告其实际版本。若 metadata 已存在但损坏，或 metadata 读取本身发生 `BUSY` / resource / I/O 等错误，则返回对应稳定 error，不把失败静默伪装成未初始化。

普通 graph/query/transaction API 的每次 invocation 只执行**结构性初始化校验**：验证 metadata marker、当前 storage-format 可理解、canonical internal-schema inventory / metadata shape 正确，并拒绝会截获 internal write 的 unsafe TEMP trigger。它们**不得**在每次调用前重算完整 Commit / Layer / Schema hash、遍历完整 Commit DAG 或扫描完整 graph 来证明 canonical history integrity；否则 query latency 会随 database 总规模线性增长并违反第 17 节 large-scale invariant。完整 canonical-history / graph integrity scan 由显式 `lithograph_integrity_check()`、初始化/迁移验收和其它明确要求完整 integrity evidence 的维护路径负责。普通执行仍依赖 canonical storage primitives 自身的 point/overlay invariant 校验，并在实际访问到损坏记录时 fail closed。

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

`SQLITE_NOMEM` 是这个 cleanup-failure 分支的已验证宿主特例：如果错误来自 scalar callback 内部的 FTS5 tokenizer 构造，SQLite 会在该 callback 剩余期间保持 malloc-failed 状态，使 `ROLLBACK TO`、`RELEASE` 和 full `ROLLBACK` 都继续返回 `SQLITE_NOMEM`；外层 `sqlite3_step` / `sqlite3_exec` 返回后才可能再次执行 rollback。SQL Bridge 仍保留真实 `SQLITE_NOMEM` primary code；当 tokenizer probe 在 canonical Schema 持久化之前失败时，不得发布 Commit/Branch 变化。但由于 callback 内无法恢复 invocation-local transaction boundary，该 connection 属于上段定义的 cleanup-failure quarantine，caller 必须关闭并丢弃，不能把“外层返回后手动 rollback 可以成功”当成 Lithograph invocation 已满足 cleanup 合同。Native API 不受 scalar callback 的这个宿主限制，仍按自身 fail-closed transaction contract 返回结构化错误并清理 active explicit transaction。

多个 mutating `lithograph()` invocation 出现在同一个 raw SQL statement 时，语义明确为多个独立 SAVEPOINT/graph operations：在 SQLite autocommit mode 下，前一个成功 invocation 可以在后一个 invocation 失败前已经 durable；Lithograph 不承诺把整个宿主 SQL statement 合成一个 graph transaction。caller-owned SQLite `BEGIN ... COMMIT` 只能把多个已经形成的 Lithograph Commit 合并到同一个 durability boundary，不会折叠版本历史。调用方若需要“多个独立 Cypher execution 共同形成一个 Lithograph Commit”，必须使用第 9.2 节的 Native explicit transaction。推荐的 raw SQL write 形式始终是一个 statement 一个 `lithograph()` invocation。

### 4.3 Native C ABI

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

`lithograph_v1_tx_*` 是 Phase 09 交付的 additive Native ABI family；它不改变既有 `lithograph_v1_execute` / `validate` symbol 的参数与生命周期。因为一个 `sqlite3*` 同时最多存在一个 Lithograph explicit transaction，v1 直接以 connection 作为 transaction identity，不再引入第二个 opaque handle / allocator / lifetime。ABI version 由 symbol name 中的 `v1` segment 固定；未来真正不兼容 ABI 必须新增 `lithograph_v2_*` symbols，不能改变以下 v1 contract：

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

Native v1 pointer/length 规则与现有 `execute` 一致：可选 UTF-8 JSON 输入省略时使用 `NULL, 0`；非 `NULL` 输入在调用期间可读，Engine 在返回前复制。`result_json` / `error_json` 是可选 writable out-pointer；提供时调用开始先写 `NULL`。成功的 `tx_begin` / `tx_commit` 只通过 `result_json` 返回 NUL-terminated UTF-8 JSON；失败只通过 `error_json` 返回第 13 节结构化错误。两类输出均由 Lithograph 分配，调用方使用 `lithograph_v1_free` 释放且只能释放一次。

`execute` 的 callback 事件顺序固定为 `COLUMNS` 一次、`ROW` 零到多次、`SUMMARY` 一次。`COLUMNS` payload 是 column-name JSON array；`ROW` 是同顺序 value array；`SUMMARY` 使用第 13.1 节 summary object。callback 返回非零值时 query 被取消并返回 `SQLITE_INTERRUPT`；若 query 已写入但尚未 durable commit，则整个 write rollback。

`error_json` 只在失败时设置，使用第 13 节 tagged JSON/error contract，由 Extension 分配并必须通过 `lithograph_v1_free` 释放。callback payload 只在 callback 调用期间有效，不需要也不能由 caller 释放。

Native API 接收现有 `sqlite3*`、Cypher text、parameter JSON、option JSON 和 event callback。普通 `lithograph_v1_execute` 是单 execution surface；`lithograph_v1_tx_begin/execute/commit/abort` 在同一个 connection-local explicit transaction 上执行，`tx_execute` 复用同一 parser/planner/executor，不建立第二套 Cypher engine。SQL Bridge 是面向普通 SQLite 客户端的 adapter，并调用同一个 Engine，但不加入 Native explicit transaction lifecycle。

`CALL { ... } IN TRANSACTIONS` 需要 Query Engine 拥有真实 transaction boundary。Native API 在没有 caller-owned active transaction 或 Native explicit transaction 时执行该语义。SQL Bridge 本身运行在一个进行中的 SQLite statement 内，因此遇到需要独立 commit boundary 的 `IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 时返回 `TRANSACTION_BOUNDARY_REQUIRED`，要求调用普通 Native execution；explicit transaction 内同样拒绝这类会再拥有独立 transaction boundary 的 query。其它 Cypher 语义不因此分叉。

### 4.4 Query Options

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
- `graphView` 可以与 `branch` 或只读 `at` 组合；它们决定 base Snapshot，初始 visibility 按该 Snapshot 计算，read-write query 的后续 clause 再按第 7.7 节基于前序 staged writes 后的 graph state 重新计算；
- `graphView` 只约束 graph-data query / mutation / Search 的可见数据。Schema、Constraint、Index definition 和 Version Procedure 不属于 Graph View；这些 command/procedure 与 `graphView` 同时出现时返回 `INVALID_ARGUMENT`，避免把子图错误解释成独立 Schema 或 Version repository。

对 Version Procedure：`branch` query option 只为“对当前 Branch 操作”的 procedure 临时选择 target（`commit.create`、`patch.apply`、`merge.start`、`rebase`、`squash`、`reset`、`revert`）；它不永久改变 connection checkout。`merge.finalize` 的 target 已由 Session 固定，`merge.get/list/conflicts/resolve/abort` 也不重新选择 target；这些 procedure 与 query-level `branch` 同时出现时返回 `INVALID_ARGUMENT`。`merge.start/get/list/conflicts/resolve/abort` 不保存未来 Commit metadata，因此与 `author` / `message` 同时出现也返回 `INVALID_ARGUMENT`；只有 `merge.finalize` 接受 `author/message`，且仅 diverged `merged` 结果真正写入新 Commit。`branch.create/delete/checkout/list`、`tag.*` 与 `commit.data.*` 自己显式指定或管理 target，同样不接受 query-level `branch`。任何 version mutation 与 `at` 同时出现都返回 `READ_ONLY_SNAPSHOT`。

Parameters JSON 与 result JSON 共用第 13.1 节 tagged-value encoding。普通 JSON primitive/list/map 直接映射到对应 Cypher value；需要保留 INTEGER64 边界、Temporal、Point、Vector 或 UUID 类型时必须使用 `$type` tagged form。

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
- `LOAD CSV` 和未来其它拥有 file/network external-I/O authority 的 query 同样不能在 explicit transaction 内执行，返回 `TRANSACTION_BOUNDARY_REQUIRED`。External I/O 继续由普通 execution / 第 9.6 节 batching 管理，避免从 `tx_begin` 起长期占用 SQLite single-writer ownership。普通 current-graph read/write 与 Schema/Constraint/Index command 仍可在 explicit transaction 内执行。

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

- `mergeSession.id` 是第 10.5 节定义的 opaque Merge Session identity，不是 version descriptor；`revision` 必须与该 Session 当前 revision 精确相等，否则返回 `MERGE_SESSION_CHANGED`；
- `mergeSession` 与 `branch` / `at` / `author` / `message` 互斥，可以与 query-local `graphView` 组合；
- 只有当前没有 unresolved merge conflict 的 Session 才能读取 candidate；仍有 unresolved conflict 时返回 `MERGE_CONFLICT`；
- candidate execution 只允许普通 current-graph read、Search 与 Schema/Constraint/Index introspection。graph/schema/index mutation 返回 `READ_ONLY_SNAPSHOT`；Version Procedure、transaction-owning subquery 与 `LOAD CSV` 返回 `TRANSACTION_BOUNDARY_REQUIRED`；
- candidate query 绑定 `(session, revision)` 而不是 Commit，因此 `summary.commit = null`，并通过第 13.1 节的 `summary.mergeSession` 返回实际 session/revision。调用方可以用同一 revision 执行多次一致性检查，随后把该 revision 交给 `merge.finalize`；若期间 resolution 改变，finalize 必须以 `MERGE_SESSION_CHANGED` 拒绝旧验证结果。

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

Value comparison 固定遵循 `CY25-2026.08` current semantics，而不是旧 openCypher TCK 的历史 cross-type 规则：

- `=` / `<>` 对 `null` 传播 `null`；Integer/Float 按 numeric value 比较；Path 可以按交替 Node/Relationship sequence 与等价 List 比较；其余不具 equality-comparability 的不同 value family 必须返回稳定 `TYPE_ERROR`，不能静默降为 `false`；
- `<` / `<=` / `>` / `>=` 对 `null` 传播 `null`。同 family 使用该 family 的 comparison semantics；不同 family 使用 Cypher 25 value hierarchy，顺序为 `MAP < NODE < RELATIONSHIP < LIST < PATH < VECTOR < POINT < ZONED DATETIME < LOCAL DATETIME < DATE < ZONED TIME < LOCAL TIME < DURATION < STRING < BOOLEAN < UUID < NUMBER`；同 family 明确定义为 non-comparable 的 value（例如 Duration direct comparison，以及 Profile 中对应的 Point/Vector direct comparison）返回 `null`，不能借用 `ORDER BY` 的内部 total-order key 改变 predicate 结果；
- List direct comparison 使用 lexicographic semantics，并保留 `null` element 导致的 unknown result；`ORDER BY` 的 total ordering 与 direct comparison 是两个独立 contract；
- `STARTS WITH` / `ENDS WITH` / `CONTAINS` / regex 等 String predicate 对 `null` 或非-String input 返回 `null`；需要 String-only access path 时由 planner/type proof 决定是否安全下推，不能把非-String value 改成 query error。

持久化 Property 只接受 Cypher 25 允许的 property value types。Map、Node、Relationship、Path 等 constructed/structural runtime value 不作为 Property value 持久化。Property legality 在 Cypher type/mutation boundary 验证；LCE1 codec 负责 storage-format bytes 的 canonical encode/decode，不作为 Cypher Property 语义验证器，因此 format 1 已冻结的 tagged List bytes 不能因上层 property-type 规则而改变。

`SET n.key = null` 与对应 Relationship 操作表示删除该 Property，而不是持久化一个 `NULL` property slot。

Vector 按原始 coordinate type 与 dimension 保存，不用 JSON list 替代，因此整数宽度、浮点精度与 dimension 可被 Schema 和 Search 正确验证。

## 6. Engine Structure

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

统计属于 derived data，不进入 Commit / Layer identity，也不成为查询 correctness 的真源。Checkpoint 可以把与该 Snapshot 对应的 versioned statistics 放在 checkpoint metadata 中；Planner 读取最近 checkpoint 的统计，再只按 checkpoint -> target Commit 的 bounded overlay 变化做增量修正。缺失、损坏或版本未知的统计必须退化为保守的 unknown estimate，而不是为每个 query 重新扫描完整 Snapshot 计算 cardinality；estimate 缺失只能影响 plan quality，不能改变结果。

Planner 可以把 filter、projection、typed comparison 与部分 index seek 下推给 SQLite，但不能为了 SQL 转译便利改变 Cypher semantics。复杂 path、merge、version snapshot 与 semantic barriers 由 Lithograph executor 原生执行。

### 7.3 Streaming

Executor 使用 row pipeline。没有 `ORDER BY`、global aggregation、`DISTINCT` 或其它 semantic materialization 要求时，结果按 batch streaming，不随总结果行数线性占用内存。

`ORDER BY`、aggregation、eager write barrier 等需要物化时，可以使用内存并在超过内部预算后 spill 到 SQLite TEMP storage。

### 7.4 Snapshot Pinning

普通 query 在执行开始时解析 Branch / Commit 并 pin 到一个 immutable Commit。Branch head 在 query 执行期间发生变化不会改变该 query 的 base Snapshot；同一 query 内前序 write clause 产生的 staged changes 仍按 Cypher clause-composition semantics 对后序 clause 可见。

`CALL { ... } IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 是例外：它们按第 9.6 节让每个 inner batch transaction 各自 pin 对应的 Branch head，而不是让整个 outer query 共用一个 immutable Commit。query-level `graphView` selector 在这些 batch 间保持不变，但 visibility 必须基于各 batch 自己的 pinned Snapshot 与该 batch 内已经完成的 staged clause writes 计算。

### 7.5 Temporal Clock Boundary

普通 auto-commit execution 下，一次 top-level Lithograph execution 同时是 Cypher temporal clock 的 transaction boundary 和 statement boundary；`date/time/localtime/localdatetime/datetime.transaction()` 与 `.statement()` 都在 execution 开始时取值，因此两者相等且在所有 cursor batch 中稳定。Native explicit transaction 下，`.transaction()` 在 `tx_begin` 成功时固定，并在全部 `tx_execute` 中保持同一 instant；`.statement()` 则在每次 `tx_execute` 开始时重新取得并在该 execution 内稳定。`.realtime()` 每次求值读取 wall clock，不保证稳定。timezone 参数只改变同一 instant 的本地表示，不改变 clock identity。

caller-owned outer SQLite transaction 可以把多个普通 Lithograph execution 的 durability 合并到一次 `COMMIT` / `ROLLBACK`，但不把这些 execution 合并成一个 Cypher transaction 或一个 Lithograph Commit；每次 invocation 仍取得自己的 transaction/statement clock。第 9.6 节的 transaction subquery 由每个 inner batch transaction 各自取得 transaction clock。

### 7.6 Cancellation 与 Connection State

Executor 在 batch/operator boundary 与长路径/搜索循环中检查 SQLite interrupt state；host 调用 `sqlite3_interrupt()` 后 query 尽快停止并返回 `SQLITE_INTERRUPT`。Native event callback 返回非零是同一 cancellation 语义的另一入口。

Active Branch、Native explicit transaction state、temporary query options、prepared-plan/cache handle、current error/cancellation state 全部属于单个 `sqlite3*` connection 或单个 query。禁止使用 process-global mutable query/branch/parser/transaction state。跨线程使用同一 `sqlite3*` 是否允许完全遵循 host SQLite threading mode；Lithograph 不为一个不允许并发使用的 connection 增加第二套线程安全保证。

Native C ABI 在已完成 Lithograph registration 的 connection 上，使用 host SQLite 的 `sqlite3_db_mutex()` 覆盖一次 ABI invocation 的 connection-state check、client-data access、query/transaction execution 与 error cleanup。SQLite serialized mode 下该 mutex 是 recursive，因此同一个 `sqlite3*` 的 Native 调用与 SQLite 自身 connection 操作按同一 serialization boundary 排序；multi-thread / single-thread mode 如果 host 不提供同 connection serialization，Lithograph 不另建独立 mutex 去扩大 SQLite 自己的线程安全承诺。`sqlite3_get_clientdata()` / `sqlite3_set_clientdata()` 只负责 state ownership/lifetime，不单独承担 invocation serialization。

### 7.7 Graph View Execution Boundary

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

Planner 持有规范化的 selector / execution context，不把 Graph View 预展开成固定 element-ID allowlist；Physical operator 在对应 clause 的实际 graph state 上执行 visibility check。Graph View v1 使用第 4.4 节的 Label selector：Node 在当前 clause graph state 中满足全部 `requireAllLabels` 且不命中任何 `excludeAnyLabels` 时可见；Relationship 当且仅当两个端点都可见时可见。Graph View v1 是 element-level visibility，不提供 Property masking：一个 element 可见时，它在当前 graph state 中可见的 Label / Relationship Type / Property 仍按正常 Cypher 语义可见。NodeId / RelationshipId、Label、Type、Property 与 Schema 本身不因 view 改写或复制。

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
- `SET` / `REMOVE` Label 不允许一个 clause 在完成后把其修改且仍存在的 Node 留在当前 Graph View 外；这种 write 返回 `GRAPH_VIEW_VIOLATION`，整个 top-level mutating query 按第 9 节回滚；因此在要求 Label `A` 的 view 中，`CREATE (n) SET n:A` 会在 `CREATE` clause 完成时失败，而 `CREATE (n:A)` 可以成功并被后续 clause 观察；
- 删除可见 Node 时，如果保持 referential integrity 必须同时修改一个不可见 Relationship，则该 delete / detach delete 返回 `GRAPH_VIEW_VIOLATION`，不得跨 view 隐式删除；
- graph-data mutation 不因 Graph View 改变 Commit 粒度、Branch compare-and-move 或 caller-owned transaction 语义。

Graph View **不投影 Schema / Constraint / Index definition**。当前 Commit 的完整 versioned Schema 仍是该 execution 的唯一 Schema；Graph Type、property type、KEY / UNIQUE / existence 等 validation 按原 Cypher/Lithograph contract 对 mutation 的完整 candidate canonical graph state 执行，包括 Graph View 外的 element。`MERGE` 的 match 部分只看到 view 内 element；如果它因此尝试创建一个与 view 外 element 冲突的 UNIQUE / KEY value，最终返回正常的 `CONSTRAINT_ERROR` 并回滚，而不是把 constraint 解释成 view-local constraint。由这种 validation 产生的间接存在性信号不违反 Graph View contract，因为 Graph View 明确不是 authorization boundary。

Graph View 是执行语义，不是认证或权限系统。持有原始 Lithograph execution surface 的调用方可以省略 `graphView` 访问完整 graph；上层产品若把它用于租户或内部数据隔离，必须控制调用方能提交的 options。Lithograph 只保证在**已经选择的** Graph View 内没有 query/operator/Search/write bypass。

### 7.8 Query-scoped Resolved State（Phase 11）

**目标：一次只读 execution 的同一 graph state 只解析一次，不能按返回 batch 重放 lineage / Layer。** 这包含 prepare/planner 与 executor 的共享，不只是把每批重复解析改成两处独立解析。`EXPLAIN` 是另一 execution；不得让 benchmark 的预先 EXPLAIN 悄悄预热随后被计时的 execution。

实现把不借用 SQLite connection 的 resolved state（pinned Commit、checkpoint identity、immutable overlay、versioned Schema、statistics）与短生命周期 storage accessor 分开。QueryCursor 持有前者，各次访问只临时绑定原 connection；不得通过延长 Rust borrow 到 `'static`、悬挂 pointer 或复制整个 overlay 来绕过生命周期。内部 read context 可以调整，现有 SQL/C ABI、batch 返回格式和错误合同不变。

生命周期固定为：

```text
parse / classify
 -> establish main-database read guard
 -> resolve version + Snapshot + Schema once
 -> plan / execute / consume batches against that state
 -> EOF | LIMIT | cancel | error | drop
 -> release owned buffers / statements / read guard
```

Read guard 必须从 version resolution 前持续到 cursor 结束，保护 checkpoint、index generation 和 canonical rows 在查询期间的 SQLite read view；只保存 Commit ID 不足以抵抗其它 connection 的 GC/cache eviction。SQL Bridge 借用宿主 transaction，并由 adapter 保持一个真正读取 `main` 的 SQLite statement cursor，直到 execution 释放；Native 普通只读 execution 使用相同 guard 原则，不能靠各 batch 内独立 SELECT 的短 implicit transaction。Guard 仅持有 read view，不取得 writer、不建立第二 connection、不创建持久 pin registry；不依赖要求特殊编译选项的 `sqlite3_snapshot_*`。Standalone Core caller 同样必须提供跨 prepare/consume 的 read context，不能绕过此保护。

Guard 的 statement handle 由 owning adapter 的 RAII boundary 管理，不能被 Core 当作可跨 connection 使用的缓存。`xFilter` 重扫先释放旧 execution；SQL 外层提前停止、`xClose`、Native callback failure、interrupt、panic、连接 teardown 和 prepare failure 都必须释放对应资源。读结束只释放本次拥有的 statement/context，不 `COMMIT`、`ROLLBACK` 或 `RELEASE` caller-owned transaction。多个只读 cursor 可以共存；同 connection 有 active read cursor 时，重入 Lithograph mutation、GC 或 cache maintenance 在产生副作用前返回 `TRANSACTION_BOUNDARY_REQUIRED`，避免自己删除尚在使用的 rows。宿主不得在 active execution 中通过 raw SQLite 改写 internal tables、重置同一 connection 的 transaction 或执行 schema replacement；不通过安装全局 authorizer 扩大对宿主的控制。

WAL 下另一 connection 可以继续提交，读者保持旧 read view；rollback-journal 下沿用 SQLite 的 reader/writer 锁规则，不宣称所有模式都不阻塞 writer。长读可能延迟 SQLite WAL checkpoint，应测量 WAL 增长并通过及时关闭 cursor 释放，不偷偷切换到新 Snapshot。这里的 SQLite WAL checkpoint 与 Lithograph graph checkpoint 是两种不同资源。

Write / candidate state 不复用过期的 read membership：

- 普通 read-write execution 保留 immutable base，已完成 clause 的 staged mutation 以单调 state revision 更新；后续 clause 的 accessor、visibility 和 index overlay 必须读取新 revision。
- Native explicit transaction 每次 `tx_execute` 有自己的 statement state，观察前序成功 execution 的 staged changes；最终 commit/abort 清理所有 staged cache。不得把 transaction 起点的 membership 当成整个 transaction 不变的集合。
- Merge candidate 绑定 `(session, revision, candidate identity)`；一次 inspection 在同一 read view 验证 revision 并执行，不能跨 revision 复用。`IN TRANSACTIONS` 仍按第 9.6 节每个 inner transaction 独立 pin，不跨 batch transaction 复用旧 state。

Storage page 与输出 batch 分离：改变调用方的 `max_rows` 不应导致重复解析或重新执行已消费的查询。复用 prepared storage statements、按需读取 properties，并用 bounded ordered merge 代替每页不必要的 map/set 重建；不以增大 batch 隐藏每 batch 重复工作，也不把整图预装内存。

Graph View 优化仅消除有证据的重复检查：对同一合法 Snapshot access path 已证明存在且可见的起点复用 proof；终点按需/分批检查。空 selector 可以省略 Label predicate，但不能把任意外部 Node/Relationship reference 直接认作有效，也不能关闭 canonical integrity 或触及损坏记录时的 fail-closed 检查。可见性缓存限于 query，key 至少包含 state identity/revision、规范化 selector 与 NodeId，采用固定预算、可逐出；revision 改变必须失效，不能随遍历过的所有 Node 无界增长。

紧凑 row/slot 化只在前三项核心优化完成后的 profiling 仍证明 allocation、clone 或 string lookup 为热点时实施。优先复用/移动既有 binding，限制在被测 read pipeline；保留变量作用域、重复列名/行、OPTIONAL null、path relationship uniqueness、错误与时钟语义。不预先新增 JIT、并行 executor 或第二套 query engine。Planner 复用已有 `optimize_node_scan` 的最低 cardinality 选择；只修实测的路径覆盖缺口，不把已有能力重新实现一次。

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

Commit Data、Tag 与 Merge Session 不加入上述 canonical graph Snapshot source of truth。Commit Data 只解释某个 Commit，Tag 只命名某个 Commit；Merge Session 是尚未 finalize 的 mutable operational workspace。修改这些 mutable state 不能改变既有 Commit、Layer、Schema hash 或 Snapshot resolution。Merge Session 只有在 `merge.finalize` 成功时才通过正常 Commit/ref transaction 影响 canonical history。

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

Merge Session `id` 使用 `merge-session/<RFC-9562-uuid>` 的 lowercase text，但在所有 API 中视为 opaque token，不能当作 `branch/`、`tag/` 或 `commit/` version descriptor。`ours_commit` / `theirs_commit` 是 Session 创建时 pin 的 immutable Commit；`revision` 从 `1` 开始，只在 resolution set 发生有效变化时单调递增；`created_at` 使用 UTC Unix epoch microseconds，只用于 operational listing/diagnostics，不参与任何 Commit hash 或 merge correctness。`resolution_json` 使用第 10.5 节的 `ours | theirs | value` shape，其中 explicit value 使用第 13.1 节 Lithograph JSON typed-value encoding。Conflict 集合、merge candidate 与分页 materialization 不作为持久化真源，可从 pinned Commit + resolution set 确定性重算；实现可以使用 query-local / TEMP spill，但不能要求把全部 conflict 或 candidate Snapshot 永久 materialize 到 main schema。

Merge Session encoding/semantics 是 storage format `2` contract 的一部分；未来若 merge algorithm/session encoding 的不兼容变化会让同一 pinned inputs + resolution set 得到不同 candidate，必须通过显式 storage-format migration 处理 open Session，不能在升级后静默用新语义重新解释旧 Session。

Open Merge Session 是 GC reachability root：其 `ours_commit` / `theirs_commit` 及所需 ancestors 在 Session finalize/abort 前不能被 canonical GC 删除。Session finalize/abort 会原子删除 session + resolution rows；derived candidate/conflict spill 随时可以丢弃重建。

Storage format 1 的 canonical explicit index inventory 除 table primary key 外固定包含：dictionary name unique indexes、Layer hash unique index，以及第 8.3 节列出的 relationship outgoing/incoming/global-identity、label reverse lookup。不得依赖 SQLite 自动生成且名称/布局不受 Lithograph 控制的 secondary index 作为 canonical access path。

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

`COMMIT` fields 固定顺序为 `format_version, parent1, parent2, layer_hash, schema_hash, author, message, committed_at`。optional field 使用首字节 `0` 表示 absent、`1 + payload` 表示 present。Root Commit 使用 `parent1 = null`、`parent2 = null`、`author = null`、`message = null`、`committed_at = 0`；因此 empty Layer、empty Schema 与 Root Commit 都是 deterministic content-addressed object。普通 Commit 的 `committed_at` 仍遵循第 8.4 节 commit timestamp contract。

`LCE1` 属于 storage-format contract。修改上述编码必须提升 storage format，并通过 migration 保持旧 Commit ID 可验证。

### 8.3 Physical Access Indexes

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

### 8.3.1 Adjacency Keyset Cursor（Phase 11）

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

验证同时检查 Lithograph operator 与 SQLite 的实际 access plan/执行工作量；一个叫 `AdjacencySeek` 的 operator 或少量 logical `dbHits` 不足以证明没有扫描无关 Relationship，见第 17.1 节。

### 8.4 Content Addressing

Layer、Schema object 与 Commit 使用 256-bit BLAKE3 content hash。

- Layer hash 基于按 logical key canonical sort 后的 delta bytes；
- Schema hash 基于 canonical schema representation；
- Commit hash 基于 `format_version + parent IDs + layer hash + schema hash + metadata`；
- Commit ID 对外为 64 个 lowercase hex characters。

Commit Data 与 Tag name/ref 不进入 Layer / Schema / Commit hash。更新或删除 Commit Data、创建/移动/删除 Tag 都不能改变既有 Commit ID；这些 sidecar mutation 由 SQLite transaction 提供原子 durability。

`format_version` 是 hash input 的一部分。后续 storage migration 不允许静默重算既有 Commit ID。

`committed_at` 使用 UTC Unix epoch microseconds，在 Commit finalize 时读取 Engine wall clock。它属于 Commit metadata 和 hash input，但不作为 DAG ancestry 或 merge correctness 的依据，也不复用第 7.5 节的 query transaction/statement temporal clock。

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
    -> execute clauses against pinned snapshot + staged writes
       under the fixed graphView selector
    -> validate constraints
    -> canonicalize delta
    -> write immutable layer/commit
    -> compare-and-move branch head
    -> SQLite COMMIT
```

上图的 `SQLite COMMIT` 对 Native API 表示 Engine 自己拥有的 transaction commit；对 SQL Bridge 表示内部 SAVEPOINT 成功 release 后，由宿主 SQLite autocommit/outer transaction 决定最终 durability。SQL Bridge 不能从 function callback 提前 commit caller-owned transaction。

普通 auto-commit execution 中，每个成功的 **graph / Schema / Index mutating query** 都产生一个 Commit，即使 effective delta 为空；这样 graph history 与 write intent 一致。Native explicit transaction 改变的是多个 execution 的 Commit boundary，而不是这些 query 的 Cypher mutation semantics，规则见 9.2。Version ref/control procedure 不一概产生 Commit：`branch.create/delete`、`reset` 与 `merge.finalize` 的 fast-forward 结果只原子修改 ref，`branch.checkout` 只修改 connection-local context，`gc` 只做 reachability cleanup，Merge Session 的 start/resolve/abort 只修改 operational workspace；`patch.apply`、`merge.finalize` 的 diverged `merged` 结果与 `revert` 会产生 Commit。

### 9.2 Native Explicit Transaction

Native explicit transaction 解决的是 **version atomicity**：调用方可以把多个独立 Cypher execution 组织成一个逻辑写单元，整个单元成功时只产生一个 Layer、一个 Commit 和一次 Branch head move。它不是 Git-style staging area，不暴露 raw Layer/Structural Patch，也不把 Cypher 25 换成 Lithograph-specific mutation language。

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
- active explicit transaction 独占该 `sqlite3*` 上的 Lithograph graph/version execution lifecycle：除同一 connection 的 `tx_execute` / `tx_commit` / `tx_abort` 与纯信息 `lithograph_version()` 外，普通 `lithograph_v1_execute` / `lithograph_v1_validate`、SQL Bridge `lithograph()` / `lithograph_rows()`、`lithograph_init()`、`lithograph_integrity_check()` 以及其它会解析/读取/修改 graph/version state 的 operation 都返回 `TRANSACTION_BOUNDARY_REQUIRED`。这样既不能绕过 explicit transaction 形成独立 Commit，也不能通过另一个 surface 对 staged / committed state 得到含糊解释；
- `expectedHead` 提供时必须与取得 writer ownership 后观察到的 Branch head 相同，否则返回 `BRANCH_HEAD_MOVED` 且不创建 transaction。省略时以实际 pin 到的 head 为 base；
- 每个 `tx_execute` 使用同一 base + transaction-local staged graph/Schema/Index state。后续 execution 必须看到前序 execution 已成功完成的 staged writes；这些 staged state 在 `tx_commit` 前没有 public Commit identity、不会移动 Branch，也不会被其它 connection 读取；
- 每个 `tx_execute` 的 `graphView` 仍是 query-local selector，可以与前一个 execution 不同；visibility 必须针对**当前 transaction staged state**重新计算，因此会观察全部前序成功 `tx_execute` 的 staged writes，但不能看到后序 execution。不能在 `tx_begin` 时把 Graph View 预展开为固定 element-ID membership；
- `tx_execute` 继续执行普通 Cypher immediate semantics：parse/semantic/type、Graph View、Schema/Constraint 与 statement failure behavior 都针对当前 staged state 生效。Explicit transaction **不自动把全部 constraint 变成 deferred constraint**；如果一个 statement 按正常 Cypher/Lithograph semantics 已经非法，它立即失败；
- `tx_execute` 遇到 `LOAD CSV`、transaction-owning subquery、Version Procedure 或其它第 4.4 节禁止 surface 时在产生对应副作用前返回 `TRANSACTION_BOUNDARY_REQUIRED` 并使 explicit transaction fail-closed abort；
- 任一 `tx_execute` parse/semantic/type/schema/constraint/Graph View/I/O/callback/cancel failure 都使 transaction fail-closed：全部 staged state rollback，transaction 进入 terminal aborted state，不能继续 execute 或 commit；cleanup 自身失败沿用第 4.2 节 `INTERNAL_ERROR` + 丢弃 connection 的故障语义；
- transaction 内新分配的 Node/Relationship identity 可以由该 transaction 后续 execution 通过正常 query result 引用，但在 `tx_commit` 成功前只属于 provisional staged state；abort 后调用方不得把这些 identity 当作 durable element。既有“不复用已提交 identity”合同不因此扩大为“失败 transaction 也永久消耗 identity”；
- `tx_commit` 对 transaction 最终 candidate state 再执行 canonical integrity / Schema / Constraint validation，按 base -> final staged state 计算一个 canonical net delta。只要 transaction 内至少成功执行过一个 graph/Schema/Index mutating query，就创建**恰好一个** Commit；即使最终 net delta 为空，也创建一个 empty-delta Commit 以保留本 transaction 的 write intent。只有 read execution 的 transaction 不创建 Commit 并返回 base Commit；
- 最终 Commit 的 parent 是 `tx_begin` pin 的 base Commit，`author/message` 来自 begin options，`committed_at` 在 Commit finalize 时取得。`tx_commit` 返回最终 Commit identity 与基于最终 canonical net delta 的 counters；Branch 只在 Commit 与 Layer 已成功写入后移动一次；
- begin 已持有 single-writer ownership，正常情况下其它 writer 不能在 transaction 生命周期内移动 Branch；Commit 前仍保留最终 compare-and-move / integrity guard，任何不一致都整体 rollback，不产生 partial Commit；
- explicit transaction 会占用 SQLite 单文件 writer ownership，因此调用方必须保持 transaction 短小，不在其中等待用户输入、长时间网络交互或其它无界外部工作。

`tx_execute` 的 Native callback 仍使用 `COLUMNS -> ROW* -> SUMMARY`。因为 staged state 尚没有 durable Commit，transaction 内 statement 的 `SUMMARY.commit` 固定为 `null`；statement counters 描述该 execution 的 provisional effect。只有 `tx_commit` 返回最终 durable Commit identity 和 transaction-level final-delta counters。

逻辑结果 shape 固定为：`tx_begin -> {"baseCommit":"commit/<id>"}`，其中 `baseCommit` 是实际 pin 的 resolved Commit；`tx_commit -> {"commit":"commit/<id>","counters":{...}}`，纯 read transaction 的 `commit == baseCommit` 且 counters 全为 `0`，存在 mutating execution 时 `commit` 是唯一新建 Commit。`tx_execute` / `tx_commit` / `tx_abort` 在当前 connection 没有 active explicit transaction 时返回 `INVALID_ARGUMENT` + `SQLITE_MISUSE`。

`tx_abort` 显式 rollback Engine-owned SQLite transaction、清除全部 staged state 与 connection-local transaction state，然后返回 `SQLITE_OK`；abort cleanup 失败返回 `INTERNAL_ERROR`，该 connection 必须关闭并丢弃。`tx_commit` 成功或任何 fail-closed abort 后 transaction 都进入 terminal 状态并从 connection 清除，后续必须重新 `tx_begin`。如果宿主没有调用 terminal operation 就真正 teardown SQLite connection，SQLite rollback 是最后的 durability boundary：所有未提交 staged canonical write 必须被撤销，Lithograph 的 connection-registration destructor 同时丢弃 transaction state；reopen 后不得出现该 transaction 的 Commit、Layer 或 Branch move。

### 9.3 Caller-owned SQLite Transaction

如果调用方已经 `BEGIN` SQLite transaction，同一 connection 内每个 mutating Cypher query 仍产生独立逻辑 Commit 并连续移动 Branch head，但这些 Commit 与 ref move 只有在外层 SQLite `COMMIT` 后才对其它 connection 可见。外层 `ROLLBACK` 会移除这一 transaction 中创建的全部 graph Commits。

因此 caller-owned SQLite transaction 只提供 **durability atomicity**，不等价于 9.2 的 version atomicity。一个 outer SQLite transaction 可以整体回滚 Commit A/B/C，但只要最终 SQLite COMMIT 成功，历史中仍保留 A -> B -> C；需要一个逻辑版本节点时必须使用 Native explicit transaction，而不是事后隐式 squash。

### 9.4 Stale Branch Head

Write 在开始时记录 base Commit，在写 Branch ref 前再次 compare current head。若同一 Branch 已被其它 writer 移动，当前 write 失败为 `BRANCH_HEAD_MOVED`；Lithograph 不自动把两个并发写隐式 merge。

普通 auto-commit write 使用 query-level base；Native explicit transaction 使用 `tx_begin` pin 的 base / `expectedHead`，并由 9.2 的 writer ownership 把 compare-and-move boundary 扩展到整个 transaction。

### 9.5 Readers and Writers

Read query pin immutable Commit，因此不会读取半完成 Layer。SQLite 的 concurrency mode 决定物理锁与 WAL 行为；Lithograph 不增加第二套 lock manager。

不同 connection 可以同时 checkout 不同 Branch。SQLite 仍是单文件 write serialization 的最终仲裁者。

### 9.6 Cypher Transaction Subqueries

Native API 在 caller 没有 active transaction 时实现 `CALL { ... } IN TRANSACTIONS`：每个 batch 对应独立 SQLite transaction 与一个或多个按 query semantics 产生的 graph Commits。

精确规则：每个成功且发生 graph/schema/index mutation 的 batch 创建 **一个** graph Commit；read-only batch 不创建 Commit。某个 batch 失败时仅该 batch transaction rollback；后续 batch 是否继续、状态列和最终 query error 按冻结 Cypher Profile 的 `ON ERROR` / status semantics 执行，因此已经 durable commit 的成功 batch 不被后续独立 batch failure 回滚。

`IN CONCURRENT TRANSACTIONS` 可以并行执行不需要 SQLite write lock 的 parse/parameter/materialization preparation，但同一 active Branch 的真正 batch transaction 从“pin latest Branch head”开始进入 Lithograph branch commit coordinator，按获得 coordinator 的顺序执行并最终由 SQLite 串行 durable commit。内部 concurrent batches 因此不会互相触发 `BRANCH_HEAD_MOVED`；每个 batch 都从进入自身 transaction 时的最新 Branch head 开始。外部 writer 在某 batch pin head 后移动同一 Branch 时，该 batch 仍按 9.4 返回 `BRANCH_HEAD_MOVED`。`DISJOINT BY` 等 Cypher 25 semantics 由 executor 保证。

存在 `options.graphView` 时，outer execution 只解析/规范化一次 selector；每个 inner batch transaction 在 pin 自己的 base Commit 后，用同一个 selector 对该 batch 的 graph state 重新计算 visibility。顺序 batch 因而可以按 Cypher 语义观察前一成功 batch 已 durable 的 graph changes；concurrent batch 的 visibility 以其实际 pinned base 与 branch-commit coordinator 顺序为准，不允许复用 outer query 开始时预计算的 element membership。

## 10. Versioned Graph Model

### 10.1 Root、Commit、Branch、Tag 与 Commit Data

`lithograph_init()` 创建 Root Commit，`main` 指向 Root。

Commit 是 immutable database state：其 parents、graph Layer、Schema reference、author/message/committedAt 与 Commit ID 创建后不允许原地修改。普通 graph / Schema / Index mutating query 按第 9 节自动创建 Commit；此外 Version Procedure 可以显式创建一个 single-parent empty-delta Commit，用于调用方主动建立新的状态节点，而不引入 Git working tree 或 staging model。

Branch 与 Tag 都是可变的命名引用，但语义不同：

```text
main -> C3
feature -> F2
tag/release-baseline -> C2
```

创建 Branch 只新增一个 ref，初始指向指定 version descriptor 解析出的 Commit Snapshot，不复制图数据。

每个 connection 有一个 active Branch，默认 `main`。`checkout` 只改变 connection-local execution context，不改 graph data。

Branch name 使用 case-sensitive UTF-8 bytes，长度为 1..255 bytes；禁止 NUL、ASCII control characters、开头或结尾 `/`、空 path segment，以及 segment `.` / `..`。`/` 可以用于层级命名。`main` 是 init 创建的保留 Branch，不能删除或重命名。删除其它 Branch 不删除 Commit；如果另一 connection 仍 checkout 已删除 Branch，它的下一次依赖 active Branch 的 query 返回 `BRANCH_NOT_FOUND`，直到 checkout 一个存在的 Branch。

Tag 使用与 Branch name 相同的字节与 path validation，但位于独立 namespace，因此 `branch/foo` 与 `tag/foo` 可以同时存在。Tag 不参与 connection checkout，也不会因 graph write 自动移动；只有显式 `tag.move` 才能改变其 target。Tag create / move / delete 都不创建 Commit。Tag 是 GC reachability root：只要任意 Tag 仍指向某 Commit，该 Commit 及其 canonical ancestors 不能因 Branch 不可达而被 GC。

每个 Commit 最多有一份可选 **Commit Data**。它是调用方提供的 mutable JSON annotation，可以是 object、array、string、number、boolean 或 `null`；Lithograph 不预定义 `title`、`time`、`stage` 等业务字段。Commit Data 可以随时通过显式 sidecar mutation set / replace / clear，且这些操作不创建 Commit、不移动 Branch/Tag，也不改变目标 Snapshot。需要 immutable/versioned 的业务事实必须放入正常 graph / Schema state。

### 10.2 Version Descriptor

所有 version-aware API 使用无歧义 descriptor：

```text
branch/<name>
commit/<64-hex-id>
tag/<name>
```

`branch/<name>` 与 `tag/<name>` 在每次 operation 开始时解析为当时指向的 Commit；operation 后续使用 pinned Commit，不受并发 ref move 影响。历史 `commit/<id>` Snapshot 永远只读。要从任意历史 Commit / Tag 状态继续写入，先从解析出的 Commit 创建 Branch。

### 10.3 Version Procedures

版本管理通过 Cypher procedure 提供，不增加自定义 grammar：

```text
CALL lithograph.branch.create(name [, from])
CALL lithograph.branch.checkout(name)
CALL lithograph.branch.list()
CALL lithograph.branch.delete(name)
CALL lithograph.commit.get(version)
CALL lithograph.commit.create([data])
CALL lithograph.commit.data.set(version, data)
CALL lithograph.commit.data.clear(version)
CALL lithograph.tag.create(name, target)
CALL lithograph.tag.list()
CALL lithograph.tag.move(name, target)
CALL lithograph.tag.delete(name)
CALL lithograph.log([version [, limit [, cursor]]])
CALL lithograph.diff(before, after)
CALL lithograph.patch.apply(patch)
CALL lithograph.merge.start(source [, expectedHead])
CALL lithograph.merge.get(session)
CALL lithograph.merge.list([limit [, cursor]])
CALL lithograph.merge.conflicts(session [, limit [, cursor]])
CALL lithograph.merge.resolve(session, expectedRevision, resolutions)
CALL lithograph.merge.finalize(session, expectedRevision)
CALL lithograph.merge.abort(session, expectedRevision)
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
- `commit.get(version)`：接受任意 version descriptor，返回解析后的 immutable Commit metadata 与当前 Commit Data；读取本身不修改任何 ref/data；
- `commit.create(data)`：在 query-level `branch` 或 active Branch 上创建一个 parent=当前 head、empty Layer、相同 Schema 的新 Commit；`author/message` 使用 execution-level query options。可选 `data` 与新 Commit Data 在同一 SQLite transaction 内原子写入；不使用 working tree / staging；
- `commit.data.set(version, data)`：解析 target Commit 后 set / replace 其 Commit Data；显式 JSON `null` 是合法 value；不创建 Commit；
- `commit.data.clear(version)`：删除目标 Commit 的 Data sidecar；目标 Commit 本身保持不变；
- `tag.create(name, target)`：创建新 Tag 并指向 target version descriptor 当前解析出的 Commit；同名 Tag 已存在时返回 `INVALID_ARGUMENT`；
- `tag.move(name, target)`：显式移动已存在 Tag；不存在时返回 `TAG_NOT_FOUND`；
- `tag.delete(name)`：删除 Tag ref，不删除其目标 Commit；
- `tag.list()`：枚举全部 Tag，按 name binary ascending 返回；
- `log(version, limit, cursor)`：`version` 省略时使用 active Branch；第一次调用把 version 解析并 pin 成 immutable start Commit。`limit` 省略时默认 `100`，必须为正整数；`cursor` 是 opaque continuation，包含 start Commit 与 DAG traversal frontier，只能用于同一 pinned traversal，Branch / Tag 后续移动不改变已开始的分页；
- `diff(before, after)`：两个参数都必须是 version descriptor；
- `patch.apply(patch)`：Commit author/message 使用 execution-level query options；
- `merge.start(source, expectedHead)`：`source` 接受任意 version descriptor；target 使用 query-level `branch` 或 active Branch。可选 `expectedHead` 只接受 resolved `commit/<id>`，用于要求 Session 的 pinned `ours` 必须精确等于调用方已验证的 target head；省略时使用调用开始实际 pin 到的 target head。成功创建 durable Merge Session 并计算初始 candidate/conflict 状态；**不创建 Commit、不移动 Branch，包括 fast-forward 情况**；
- `merge.get(session)`：在一个 SQLite read snapshot 内读取 Session 当前 pinned inputs/revision，并基于同一 revision 的 resolution set 计算 status 与 unresolved conflict count，不能返回 revision/status 来自不同瞬间的混合结果；
- `merge.list(limit, cursor)`：分页枚举**调用期间当前存在**的 open Merge Session；`limit` 默认 `100`，按 session id binary ascending，cursor opaque。它只读取持久化 session metadata，不为了 listing 重算 candidate/conflict；需要 `status/unresolved` 时对具体 Session 调用 `merge.get`。list 是 operational inventory，不 pin 一个跨多页不可变的 Session 集合；并发 start/finalize/abort 可以改变后续 page，调用方需要最新完整 inventory 时从首屏重新枚举；
- `merge.conflicts(session, limit, cursor)`：分页返回该 Session 当前 revision 的 conflict；cursor 绑定 session + revision，resolution 改变后旧 cursor 返回 `MERGE_SESSION_CHANGED`；
- `merge.resolve(session, expectedRevision, resolutions)`：`expectedRevision` 必须等于当前 revision；同一调用原子 set/replace 一组 conflict resolution，unknown conflictId / duplicate conflictId / 非法 choice/value 返回 `INVALID_ARGUMENT`。在**当前 revision** 上，如果整批 resolution 与已保存值完全相同则是 no-op、revision 不变；只要 resolution set 有有效变化，revision 就只递增一次。使用 stale `expectedRevision` 的重试仍返回 `MERGE_SESSION_CHANGED`，即使 payload 恰好与当前值相同，v1 不另外维护 request-id 幂等日志；
- `merge.finalize(session, expectedRevision)`：只在 expected revision 精确匹配且 unresolved conflict 为 `0` 时运行；Commit author/message 使用 finalize execution-level query options。取得 target Branch writer ownership 后必须再次确认 head 仍等于 Session 的 pinned `ours`；不一致返回 `BRANCH_HEAD_MOVED` 且 Session 保留；
- `merge.abort(session, expectedRevision)`：进入短 writer boundary 后要求 Session revision 仍等于 `expectedRevision`，匹配时原子删除 Session/resolution，不创建 Commit、不移动 Branch；stale caller 返回 `MERGE_SESSION_CHANGED`，避免用旧状态误删别人刚更新的 conflict resolution；
- `rebase(onto, options)`：把 active Branch 在 merge-base 之后的 first-parent commit sequence 逐个 replay 到 `onto`；procedure options 只支持 `resolutions`；replayed Commit 默认保留各自旧 author/message，不使用 execution-level author/message 覆盖历史 intent；
- `squash(since)`：`since` 必须是 active Branch head 的 ancestor descriptor，把 `since..HEAD` 的最终结构化变化压成一个新 Commit；新 Commit author/message 使用 execution-level query options；
- `reset(target)`：把 active Branch ref 移到 target descriptor 当前解析出的 Commit；
- `revert(commit, options)`：commit 必须是 `commit/<id>`；procedure options 只支持 `mainline`；Commit author/message 使用 execution-level query options；
- `gc()`：没有参数。

`merge.resolve(..., resolutions)` 与 `rebase.options.resolutions` 共用同一 resolution item shape：

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
| `commit.get` | `commit, parents, author, message, committedAt, hasData, data` 一行 |
| `commit.create` | `commit` 一行 |
| `commit.data.set` | `commit, data` 一行 |
| `commit.data.clear` | `commit` 一行 |
| `tag.create` | `name, commit` 一行 |
| `tag.list` | 每个 Tag 一行 `name, commit` |
| `tag.move` | `name, previousCommit, commit` 一行 |
| `tag.delete` | `name, previousCommit` 一行 |
| `log` | 每个 Commit 一行 `commit, parents, author, message, committedAt, cursor`；`cursor` 可从该 row 之后继续，遍历结束时为 `null` |
| `diff` | 一行 `patch` map |
| `patch.apply` | 一行 `commit` |
| `merge.start` | 一行 `session, targetBranch, ours, theirs, revision, status, unresolved` |
| `merge.get` | 一行 `session, targetBranch, ours, theirs, revision, status, unresolved` |
| `merge.list` | 每个 open Session 一行 `session, targetBranch, ours, theirs, revision, createdAt, cursor` |
| `merge.conflicts` | 每个 conflict 一行 `session, revision, conflictId, slot, base, ours, theirs, resolution, cursor` |
| `merge.resolve` | 一行 `session, revision, status, unresolved` |
| `merge.finalize` | 一行 `status, commit`；`up_to_date | fast_forward | merged` |
| `merge.abort` | 一行 `session` |
| `rebase` | 一行 `status, commit, rewritten, conflicts`；冲突时 `commit = null` |
| `squash` | 一行 `from, previousHead, commit` |
| `reset` | 一行 `from, to` |
| `revert` | 一行 `commit` |
| `gc` | 一行 deleted Commit/Layer/cache counters |

`log` 遍历 pinned start Commit 可达的 DAG，按 reverse-topological order 返回；同一 topology level 先按 `committed_at` descending，再按 Commit ID ascending，保证结果确定。Continuation cursor 只编码 traversal state，不是新的 version identity，也不能被调用方解析或修改。`log` 默认不展开 Commit Data；需要某个状态的业务 annotation 时使用 `commit.get`，避免大型 history 把任意 JSON 一次性塞入结果。

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
SetIndex
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

每个 operation 都包含 `op`、stable logical slot，以及该 operation 所需的 typed `before` / `after`。一个 canonical Patch 对同一 logical slot **最多包含一个 operation**；duplicate slot 属于非法 Patch 并在应用任何 operation 前返回 `INVALID_ARGUMENT`。这样全部 `before` conditions 都解释为对输入 Snapshot 的并列前置条件，而不是依赖 Patch 内 operation 顺序形成第二套 imperative mutation language。Index 从一个定义替换为同名的另一个定义时使用单一 `SetIndex`，不能编码成同一 `index/<name>` slot 上的 `DropIndex` + `CreateIndex` 顺序对。`patch.apply` 只接受 `databaseId` 与当前 database 相同的 patch；跨 database patch/import 不属于当前合同。`from/to` 用于 provenance，不要求 active Branch 当前 head 等于 `from`，真正 applicability 由所有 `before` conditions 决定。

每个 operation 使用稳定 `elementId`、label/type/property name 和 before/after value 表示。`DETACH DELETE` 产生显式 Relationship deletions 与 Node deletion，因此 patch 可独立验证和重放。

`lithograph.diff(A, B)` 对任意 version descriptor 产生从 A 变为 B 的 canonical ordered patch。若 A 是 B 的 ancestor，可以直接 compose Layers；否则使用 Snapshot / merge-base 优化，但输出语义相同。

`lithograph.patch.apply` 在 active Branch 上验证 patch 的 `before` condition，全部成立后以一个新 Commit 原子应用；任一 condition 不成立则整体失败。

Diff / Patch 只描述 canonical graph / Schema / Index Snapshot change。Commit Data、Tag、Branch ref 不进入 patch。显式 empty-delta Commit 与其 parent 的 `diff` 可以合法返回空 `operations`；这不表示两个 Commit identity 相同。

### 10.5 Three-way Merge 与 Merge Session

Merge 使用 Git 风格 three-way model：

```text
merge-base
   /   \
 ours  theirs
   \   /
  merge commit
```

目标 Branch 当前 head 是 `ours` / first parent；source 是 `theirs` / second parent。Merge result layer 相对 `ours` 保存。

Merge 使用**可恢复的 Merge Session**，把“计算/解决冲突/检查 candidate”与“最终 Commit + Branch move”分开。`merge.start` 同时解析并 pin `ours` 与 `source` Commit，并持久化 Session；Source Branch 后续移动不改变本次 merge 已 pin 的 `theirs`。Session 创建后不长期持有 SQLite writer ownership，用户或上层系统可以跨多个调用、connection reopen 甚至 process restart 分页查看和逐步解决大量冲突。

`merge.start` 的 merge-base / candidate/conflict 计算可以在 read path 上完成，但 Session row 真正建立前 `ours/theirs` 还不是 GC root。开始持久化时必须进入一个短 SQLite write transaction，在该边界内重新确认两个 pinned Commit 仍存在，再原子写入 Session；如果其间某个 Commit 已被 GC，返回 `VERSION_NOT_FOUND` 且不创建 partial Session。调用方提供 `expectedHead` 时，还必须在**同一个 writer boundary** 内确认 target Branch 当前 head 仍精确等于 `expectedHead`，否则返回 `BRANCH_HEAD_MOVED` 且不创建 Session；成功 Session 的 `ours=expectedHead`。省略 `expectedHead` 时，target Branch 在 read-path 计算后移动不要求 start 失败，因为 Session 明确保存计算时 pin 的原 `ours`，最终是否还能提交由 finalize 的 target-head CAS 决定。

Merge Session 是 operational workspace，不是 Commit、Branch、Tag 或 Version Descriptor。Session 的 authoritative state 只有 pinned `ours/theirs`、resolution set 与单调 `revision`；candidate/conflict 都从这些 immutable inputs 确定性计算。多个 Session 可以并存，也可以针对同一 target Branch 并行准备；只有 finalize 时的 target-head CAS 决定谁能够提交。

`merge.start` 计算出的初始 status 使用：

- `theirs == ours` 或 `theirs` 是 `ours` ancestor -> `status = up_to_date`；
- `ours` 是 `theirs` ancestor -> `status = fast_forward`；
- 其它 divergence 且存在 unresolved conflict -> `status = conflicted`；
- 其它 divergence 且 conflict 已全部解决/不存在 -> `status = ready`。

这些 status 在 `merge.start/get/resolve` 阶段都**不会**修改 canonical history。即使是 fast-forward，也要等 `merge.finalize` 才能移动 target Branch，使调用方可以在 ref move 前对 pinned candidate 做额外只读验证。

用于 candidate inspection 的逻辑 Snapshot 固定为：`up_to_date` 读取 pinned `ours`；`fast_forward` 读取 pinned `theirs`；`ready` 读取基于 pinned `ours/theirs` + 当前 resolution set 计算出的 merged candidate。`conflicted` 没有完整 candidate，不能进入 candidate query context。

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

存在 conflict 时 Session **不写任何 Commit/ref 部分结果**。`merge.conflicts` 以 bounded page 返回当前 revision 的 conflict inventory，包括已经有 resolution 与仍 unresolved 的项；结果按 canonical `slot` UTF-8 bytes、再按 `conflictId` bytes 升序，cursor 绑定 session + revision。大量 conflict 不要求一次 materialize 到 result 或 caller memory。Conflict ID 仍由 pinned merge inputs + logical slot/value 确定性生成。

`merge.resolve` 可以多次调用，每次只提交一批 `ours` / `theirs` / explicit replacement value，并允许后续用同一个 conflictId 替换之前选择。为避免在大量 conflict 校验时长期占用 writer，Engine 先在 `expectedRevision=R` 的 SQLite read snapshot 上 pin Session/resolution set，应用本次 proposed resolution 到 operation-local state，并在这个只读阶段完成 conflictId / explicit-value type validation、candidate/conflict 重算以及**本次成功 operation 应返回的 resulting revision/status/unresolved**：整批与已有 resolution 完全相同则 resulting revision 仍为 `R`，存在有效变化则为 `R+1`。随后进入短 write transaction / writer boundary，**重新**读取 Session 并要求 revision 仍为 `R`，否则返回 `MERGE_SESSION_CHANGED`。只有 CAS 成功才原子 set/replace resolution 并把 revision 至多增加一次；返回的 status/unresolved 使用前述 deterministic proposed state，因此不需要在 writer lock 内重新扫描大型 conflict set。一次 resolution 可能使旧 conflict 消失，也可能暴露新的 constraint conflict。任何 unresolved conflict 时都不能 finalize。

如果某个已经保存 resolution 的 conflict 因其它 resolution 改变而暂时不再出现在当前 conflict inventory，该 resolution 作为 **dormant resolution** 保留：它当前不作用于 candidate、不计入 unresolved，也不出现在 `merge.conflicts` 当前页；若同一 pinned merge inputs 下完全相同的 deterministic conflictId 后续重新出现，则自动重新应用原 resolution。这样逐步解决不会因为 conflict dependency 的出现/消失丢失已完成工作，同时 conflictId 绑定的 slot/value hash 又保证旧 resolution 不会被套到另一个不同 conflict。

当 unresolved conflict 为 `0` 时，调用方可以通过 `options.mergeSession={id,revision}` 使用普通只读 Cypher / Search / Schema introspection 检查**这一版精确 candidate**。每次 candidate execution 在自己的 SQLite read snapshot 内同时读取 Session、校验 requested revision 并 pin 对应 resolution set；如果 operation 开始时当前 revision 已不同则返回 `MERGE_SESSION_CHANGED`，operation 开始后的并发 resolution 不会改变该 execution 已 pin 的 candidate。这提供通用的上层 candidate-validation boundary，而 Lithograph 不需要知道调用方的业务规则。一个调用方可以在 revision `R` 上执行多次检查，随后调用 `merge.finalize(session,R)`；如果检查期间任何 resolution 被修改，revision 会变化，旧 finalize 以 `MERGE_SESSION_CHANGED` 失败。因此“被验证的 candidate”和“准备提交的 candidate”不会静默漂移。

`merge.finalize` 是唯一会影响 canonical history 的 Session operation。它分成 preparation 与短 writer finalize 两段，不能把大型 merge 重算放进 writer lock：

1. 在 `expectedRevision=R` 的 SQLite read snapshot 上 pin Session/resolution set，确认 unresolved=`0`，确定性构造 exact candidate、canonical net delta / schema result，并完成可在只读阶段证明的完整 graph/Schema/Constraint/canonical-integrity validation；这些 prepared result 只是本次 operation-local state，不创建新的持久 workspace；
2. 开启 Engine-owned SQLite write transaction / 取得 writer ownership，使并发 `merge.resolve/abort/finalize`、GC 与 Branch write 被 SQLite 串行化；
3. 在该 writer boundary **内部**重新读取 Session，验证其仍存在、revision 仍为 `R` 且 unresolved=`0`；随后读取 target Branch：Branch 已不存在返回 `BRANCH_NOT_FOUND`，存在但 head 不等于 pinned `ours` 返回 `BRANCH_HEAD_MOVED`。这些失败都保留 Session；不能先 check revision/head 再取得 writer；
4. 如果上述 CAS 成立，prepared candidate 仍由同一 immutable `ours/theirs` + revision `R` resolution set 唯一决定，不需要在 writer 内再次执行完整 merge/conflict scan；只执行依赖实际写入边界的 final storage/integrity recheck，并安装必要的 canonical/derived write result；
5. `up_to_date`：不写 Commit、不移动 Branch；`fast_forward`：只把 Branch 移到 pinned `theirs`；`ready`：创建 parent1=`ours`、parent2=`theirs` 的 Merge Commit，Layer 相对 `ours` 保存；
6. Branch move / Commit write（若有）与 Session/resolution 删除在**同一个 SQLite transaction**完成。

因此用户可以解决 1 个、100 个或 100,000 个冲突而不产生 intermediate Commit；Branch 也不会因为逐步 resolution 移动。只有最终 finalize 成功时历史才出现一次结果。

`merge.finalize` 成功时 `commit` 总是非空 resolved identity：`up_to_date` 返回 pinned `ours`，`fast_forward` 返回 pinned `theirs`，`merged` 返回新建 Merge Commit。这样调用方不需要根据 status 再执行一次 Branch read 才知道最终 Snapshot。

Merge Session operation 的稳定失败优先级固定为：Session 不存在先返回 `MERGE_SESSION_NOT_FOUND`；需要 expected revision 的 operation 在 Session 存在但 revision 不匹配时返回 `MERGE_SESSION_CHANGED`；candidate inspection / finalize 在当前 revision 仍有 unresolved conflict 时返回 `MERGE_CONFLICT`；finalize 再检查 target Branch 的 `BRANCH_NOT_FOUND` / `BRANCH_HEAD_MOVED`；通过这些 concurrency/boundary checks 后才报告 candidate 的 Schema/Constraint/storage validation error。这样 stale caller 不会因为后续 candidate 内容变化得到误导性的业务错误。

Merge 不解释或合并 Commit Data，也不移动 Tag。Diverged finalize 新建的 Merge Commit 默认没有 Commit Data；调用方需要时在 finalize 成功后显式 set。Fast-forward finalize 只移动目标 Branch 到已有 source Commit，因此该 Commit 原有 Data 保持可见。

Conflict ID 是对 `merge-base identity + ours commit + theirs commit + slot + base/ours/theirs canonical values` 的 BLAKE3 hash；同一 pinned merge inputs 得到相同 conflict ID。Session resolution 可以长期保存，但不会绕过 target Branch concurrency：target head 在 Session 生命周期内发生变化时，已有 conflict resolution 仍可查看，`merge.finalize` 必须返回 `BRANCH_HEAD_MOVED`，调用方重新 start 新 Session 后不能把旧 resolution 静默套到新 merge inputs。

Session 自身是持久 operational state：connection/process crash 后可以通过 `merge.get/list` 恢复进度；`merge.abort(session, expectedRevision)` 以 revision CAS 显式放弃。Lithograph v1 不自动按 TTL 删除 open Session，避免在长时间人工/AI conflict resolution 中丢失工作；调用方负责 finalize/abort，`merge.list` 提供可发现的清理入口。Open Session 同时保护 pinned history 免受 GC。

### 10.6 Rebase

`rebase(onto)` 使用 Git-style commit replay，但以结构化 graph patch 为单位：

1. pin active Branch head 为 `oldHead`，pin `onto` Commit；
2. 沿 `oldHead` 的 **first-parent chain** 向历史方向查找，选择离 `oldHead` 最近且同时是 `onto` ancestor 的 Commit 作为 **replay boundary**；这一定义与本节 first-parent replay 模型绑定，不从多个 criss-cross best merge bases 中按 Commit ID 任取一个；
3. 取 replay boundary（exclusive）到 `oldHead` 的 first-parent Commit sequence，按旧到新顺序 replay；
4. 对每个旧 Commit `C`，使用 `diff(parent1(C), C)` 作为该 Commit 的 intent；在当前 replay head 上以 `parent1(C)` / current replay head / `C` 做 logical-slot three-way application；
5. 无冲突时创建一个新的 single-parent Commit，保留旧 Commit 的 `author` / `message`，使用新的 `committed_at`；旧 Commit 如果是 Merge Commit，其 second-parent topology 默认被 flatten，replay 的是它相对 first parent 的实际 graph/schema change；
6. 全部旧 Commit replay 成功后才把 active Branch 原子移动到最后一个新 Commit。

整个 rebase 在一个 SQLite transaction 内 staged。任何 replay conflict、constraint violation、resource failure 或目标 Branch stale-head 都 rollback **全部新 Commit** 并保持 Branch 不变，不产生半完成 rebase。

Rebase conflict 使用与 merge 相同的 logical slot 和 `base/ours/theirs` shape，并额外包含 `sourceCommit`。`options.resolutions` 使用相同 conflictId resolution format。没有需要 replay 的 Commit 时返回 `status = up_to_date`；成功重写时 `status = rebased`，`rewritten` 按旧到新顺序返回 `{from,to}` pairs。

Rebase conflictId 必须在 **相同 `onto`、相同待 replay source sequence、相同前序 resolution 选择** 下跨整个 operation retry 保持稳定。它不能依赖本次尝试中新建 rewritten Commit 的 ID 或 `committed_at`，因为 conflict rollback 会删除这些临时 Commit，而下一次调用按本节规则使用新的 `committed_at`。对每个 source Commit `C`，Rebase 因此使用 `parent1(C)` 的 stable base identity、当前 replay state 的 canonical logical-state identity、`C` 的 immutable Commit identity、slot 与 `base/ours/theirs` canonical values 生成 deterministic conflictId。当前 replay state identity 只描述 graph / Schema / Index logical slots，不包含 rewritten Commit metadata；前序 resolution 真正改变 replay state 时，后续 conflictId 可以相应改变。这样调用方可以跨多次 `rebase(..., {resolutions:[...]})` 逐步解决位于多个 source Commit 的 conflict，同时不会把旧 resolution 静默套到不同 candidate state。

10.5 的 best-common-ancestor / virtual-base 规则仍定义 **Merge** 的三方 base。Rebase 的 replay boundary 解决的是“active Branch 哪些 first-parent Commit 属于待重放序列”这一不同问题；在存在多个 best merge bases 的 criss-cross DAG 中，必须由 active first-parent chain 决定该边界，不能让 hash/Commit-ID 排序偶然改变 rebase 是否可执行。每个待重放 Commit 的冲突判定仍复用 10.5 的 logical-slot、dependency 与 post-merge Constraint validation 规则。

Rebase 不自动把旧 Commit Data 复制到 rewritten Commit，也不移动任何 Tag。旧 Commit Data 继续绑定旧 Commit；调用方可以根据 `rewritten` mapping 自行决定是否复制/重建 annotation。Lithograph 不猜测任意业务 JSON 在新 base 上是否仍然成立。

### 10.7 Squash

`squash(since)` 要求 `since` 是 active Branch head 的 ancestor。若 `since == HEAD`，返回 `INVALID_ARGUMENT`，因为没有 Commit 可 squash。

Squash 计算 `diff(since, HEAD)`，创建一个 parent=`since` 的新 single-parent Commit，Layer 表示该完整结构化变化，然后原子把 active Branch 移到新 Commit。原历史 Commit 保持 immutable；如果没有其它 Branch 引用，它们只是变成 unreachable，直到 explicit GC。

Squash 不把原 Commit author/message 列表嵌入新 Commit。新 Commit metadata 使用 execution-level query options 的 `author/message`；省略时为 `null`。Squash 后 graph/schema/index Snapshot 必须与原 HEAD 完全相同。

Squash 不聚合被压缩 Commit 的 Commit Data，也不移动 Tag。新 Commit 默认没有 Data；旧 annotation 仍绑定旧 Commit，直到这些 Commit 后续真正被 GC。

### 10.8 Reset、Revert 与 History

- `reset(target)` 原子移动 active Branch ref 到已有 Commit，不删除 Commit；
- `revert(commit)` 计算该 Commit 相对 parent 的 inverse patch，并在 active Branch 创建一个新 Commit；普通 Commit 固定使用 parent1；Merge Commit 必须通过 options 指定 `mainline: 1|2`，否则返回 `INVALID_ARGUMENT`；Root Commit 不能 revert；
- `log` 沿 pinned Commit DAG 分页返回 id、parents、author、message、timestamp 与 opaque continuation；
- 使用 `options.at = commit/<id>|branch/<name>|tag/<name>` 可 time-travel / named-snapshot 查询，Branch / Tag 都在 query 开始时解析到 immutable Commit。

Reset / Revert 不修改既有 Commit Data 或 Tag；Revert 创建的新 Commit 默认没有 Data。

Canonical history 不自动 GC。`lithograph.gc()` 只删除从任何 Branch、Tag **或 open Merge Session** 都不可达的 Commit / Layer；derived checkpoint/index/cache 可以自动回收，因为可重建。

GC 对 canonical objects 按 reachability 删除：Commit 不可达后，其 Commit Data sidecar 一起删除；其 Layer 只有在没有其它 reachable Commit 引用时才删除；Schema object 同理。Tag 与 open Merge Session 都提供 root；Tag 不由 GC 自动删除，Merge Session 只由 finalize/abort 删除。Dictionary identity/name 是 database-global append-only metadata，即使当前没有 reachable Snapshot 使用也不回收，避免 ID 重用和历史/patch 解释变化。

Lithograph 的 versioned-state contract 是**单个 SQLite database 内的本地状态演进机制**；Git / TerminusDB 只提供 Commit DAG、Branch、Diff/Merge 等机制参考，不规定调用方把 Commit 解释成软件版本、时间点、场景还是其它业务状态。Commit/Branch/Tag/Diff/Merge/Rebase/Squash 等全部在同一个 `databaseId` 内工作。跨 SQLite database 或跨网络的 clone/fetch/push/pull 属于复制/传输层，不是 Lithograph Extension v1 的 Version Procedure contract；SQLite backup/file replication 可以复制整个 repository，但两个独立 `databaseId` 不通过 Version API 隐式合并 identity space。

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

Full-text 按第 11.5.4 节在此边界内验证 tokenizer 的构造能力；完整 FTS corpus 仍按需构建，不把一次配置验证扩大为全图全文索引构建。Canonical write 与 Branch move 的对外成功仍以整个 invocation/transaction 成功为准。

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

Derived index 不能通过“只缓存可索引类型”改变 Cypher 的 error / `null` / cross-type ordering semantics。Exact equality / `IN` 如果目标 property 可能同时存在与 probe value 不具 equality-comparability 的其它 value family，则直接 exact seek 会错误跳过本应产生的 `TYPE_ERROR`；这种情况下只有目标 Commit 的 versioned Property Type Constraint 能证明全部 present value 与 probe equality-compatible，Planner 才能使用 exact seek，否则必须 scan/filter。Ordered comparison 对不同 value family 按第 5.3 节的 Cypher value hierarchy 比较，因此只覆盖单一 physical key family 的 range seek 也必须有足够 Property Type proof，或由访问路径显式覆盖 hierarchy 中全部相关 family，不能把其它 family 静默过滤。String predicate 对非-String value 返回 `null`；Text/Range seek 若会预先丢弃这些 value，只能用于与 filter semantics 等价的上下文，并继续遵守 Graph View / null semantics。Point spatial predicate 同样不得因 derived cache 的类型筛选改变 observable behavior。Property Type proof 与 index definition 一样按目标 Commit 解析，不能使用 current-head Schema 或 derived cache 内容替代。

### 11.5 Full-text

#### 11.5.1 职责与边界

SQLite FTS5 是唯一 Full-text backend。Lithograph 负责 Node/Relationship Full-text DDL、versioned IndexDefinition、multi-label/type、multi-property、query procedures、score、Graph View、历史 Snapshot 和 derived cache 生命周期；真正的分词算法与 tokenizer 参数解释由当前 SQLite connection 上注册的 FTS5 tokenizer 实现负责。

应用可以在同一 connection 上加载 Lithograph 与第三方 SQLite tokenizer extension，或通过 SQLite FTS5 API 注册 tokenizer。只要求相关执行发生前已经注册，不规定两个 extension 的加载先后。Lithograph 不安装、查找或自动加载第三方动态库，不启用宿主的 extension-loading 权限；不预先调用 Jieba 把文本改造成词序列后再写入 FTS5，也不建立通用 tokenizer/backend plugin system。

本节只开放 FTS5 原生 tokenizer contract。独立全文 virtual-table engine、FTS3/4 tokenizer、普通 SQL 分词函数，以及 FTS5 的自定义 rank/auxiliary function、`content`、`detail`、`prefix`、`locale` 等建表选项，不因此成为 Lithograph 新的公共配置。Vector/HNSW、`SEARCH` 与 KG OS 不在本次范围。

#### 11.5.2 Cypher 配置与 FTS5 specification

保留既有 Cypher 25 入口，不增加 `TOKENIZER` clause、`fulltext.tokenizer` 或独立 arguments key：

```cypher
CREATE FULLTEXT INDEX doc_text
FOR (d:Doc) ON EACH [d.title, d.text]
OPTIONS {
  indexConfig: {
    `fulltext.analyzer`: 'unicode61',
    `fulltext.eventually_consistent`: false
  }
}
```

创建索引后，单独执行查询；两者不是一条复合 DDL：

```cypher
CALL db.index.fulltext.queryNodes('doc_text', $query, {skip: 0, limit: 10})
YIELD node, score
RETURN node, score
```

Relationship 使用 `CREATE FULLTEXT INDEX ... FOR ()-[r:TYPE]-() ON EACH [...]` 与 `db.index.fulltext.queryRelationships`，返回 `relationship, score`。多 Label/Type 和 Property 的含义不变。

| 配置 | 类型 / 省略时默认 | 目标含义 |
| --- | --- | --- |
| `fulltext.analyzer` | STRING / `'unicode61'` | 完整 FTS5 tokenizer specification；按 SQLite 的规则读取 name 和有序参数 |
| `fulltext.eventually_consistent` | BOOLEAN / `false` | 继续版本化保存；不放松目标 Snapshot correctness，也不新增异步 worker |

`eventually_consistent` 的两个值都沿用按需构建 cache、按选定 Snapshot 同步取得正确查询结果的机制；目前它只是 versioned metadata，不选择另一条刷新路径。它不允许返回其它 Commit 或过期 cache 的内容，也不是 Neo4j 后台刷新时序的实现承诺。

以下是 FTS5 specification 的例子，不是新增 Cypher 语法：

| analyzer 的字符串值 | FTS5 含义 |
| --- | --- |
| `unicode61` | 使用 SQLite Unicode tokenizer |
| `porter unicode61` | 使用 Porter wrapper，并把 `unicode61` 作为参数交给它 |
| `unicode61 remove_diacritics 0` | 把有序参数交给 `unicode61` |
| `jieba` | 仅当宿主实际注册了这个名字时，使用对应第三方 tokenizer |

第三方名字、支持的参数和含义以该 extension 的实际注册和文档为准。`jieba search` 仅在某个插件明确接受 `search` 参数时成立，不是 Lithograph 或 SQLite 定义的 Jieba 通用配置。内置 tokenizer 的可用范围同样取决于受支持的宿主 SQLite runtime。

STRING 的类型检查、NUL 拒绝和非空检查由 Lithograph 完成；FTS5 specification 语法、名字解析、参数接收和构造失败由 SQLite/tokenizer 验证。拒绝空字符串和只含 FTS5 分隔空白的 specification，不允许把它们当成省略配置，也不以 Unicode trim 擅自改变合法名称或参数。未知 Index OPTIONS/indexConfig key、错误类型和 `null` 继续明确拒绝；不把整个 options map 作为任意 FTS5 建表参数透传。DDL 继续使用当前支持的 literal OPTIONS 值，不顺带扩展参数化 DDL。

**安全编码规则**：先完成 Cypher 字符串解码，得到 specification 原文；再把这整个值编码为一个 SQL text literal，内部单引号必须成对转义。不得将未转义输入拼入 `execute_batch`，不得使用 shell quoting、简单 `split_whitespace` 或自行剥离 FTS5 内层引号。Property text 与 MATCH query 使用绑定参数；表名和内部列名由 Engine 生成，与 tokenizer 输入隔离。

例如 Cypher 使用双引号包围带 FTS5 内层单引号的值：

```cypher
OPTIONS {indexConfig: {
  `fulltext.analyzer`: "unicode61 tokenchars '-_'"
}}
```

得到的内部建表片段是 `tokenize='unicode61 tokenchars ''-_'''`。引号、空格、重复参数、Unicode 和参数顺序必须按 FTS5 contract 保留；它们既不是 SQL 指令，也不是 Lithograph 要理解的业务参数。需要瞬态解析以调用原生 API 时，解析规则必须与 FTS5 specification grammar 一致，并以直接 FTS5 建表作差分验证。

#### 11.5.3 Versioned definition 与破坏升级

Canonical Full-text configuration 只保留 `analyzer: String`（完整 specification）和 `eventually_consistent: bool`。默认值在新建 definition 时显式写入；`SHOW FULLTEXT INDEXES` / `SHOW INDEXES` 的 options 返回目标 Snapshot 中保存的字符串。无需再保存第二份 name/arguments、插件路径、运行时 handle 或 tokenizer registry。

保存的是 Cypher 解码后的原始字符串，不做大小写折叠、参数排序或等价拼写归一化。即使两种拼写在某个 tokenizer 下等价，不同字符串仍是不同 definition；这允许保守地重建 cache，避免错误合并第三方配置。参数值、参数顺序或 tokenizer 名称变化，都必须被 definition equality、Schema hash、Diff/Patch 与 Merge 的既有 Index slot 识别。修改配置通过标准 DROP/recreate 或既有版本操作完成，不新增 ALTER FULLTEXT INDEX 方言。

本次允许破坏升级：删除 Lithograph 的 `standard-no-stop-words → unicode61`、`english → porter unicode61` 映射，不保留 legacy variant、双模型或 alias fallback。普通分词改写为 `unicode61`，原有 Porter 行为改写为 `porter unicode61`；后者并不等于 Neo4j/Lucene 的完整 `english` analyzer。旧名称只有作为真实 FTS5 tokenizer 注册名、并使用合法 specification quoting 时才可能可用；例如带连字符的名字需要 FTS5 内层引号，不能靠旧 Cypher STRING 的外层引号替代。Lithograph 不再赋予它们特殊含义。

已有 STRING 编码可容纳新 specification，本次不要求改变物理 storage format、重算既有 Commit/Schema ID 或增加迁移层。旧版本 analyzer 定义的可继续检索性不属于本次兼容承诺；不原地重写其历史配置、不自动删库或修补旧 Commit。应用可以使用新配置重建当前索引；这不会让旧 Commit 中的旧配置自动变成新配置。v0.1.0 Release Notes 和固定版本使用文档继续描述当时事实。

#### 11.5.4 Schema 执行验证与原子失败

普通 CREATE、Native staged DDL，以及 Patch/Merge/Rebase/Revert 等创建新 canonical 状态并引入或更改 Full-text definition 的路径，都必须在发布新 Schema/Branch 状态之前，在实际执行的宿主 SQLite connection 上验证对应 specification。只验证相对本次目标 Branch base 新增/改变的定义，不为无关 Schema/graph 写入重新构造所有 tokenizer。纯 ref 移动（包括仅移动到已有 Commit 的 fast-forward）、历史 Schema inspection、DROP 和真正没有改变 definition 的 `IF NOT EXISTS` 不以 tokenizer 可用为前提；配置类型/结构验证仍不能被 `IF NOT EXISTS` 绕过。

最低成本的原生验证是：在当前 invocation/savepoint 内创建只有一个文本列的临时 FTS5 table，使用安全编码后的 specification，成功后删除探针。它检查实际 FTS5 解析、注册名与 tokenizer `xCreate`，不构建整个 graph corpus。缺失 tokenizer、无效 specification 或 tokenizer 报告的参数错误使本次 Schema mutation 失败，不能留下新的 durable Commit、Schema 引用、Branch move 或不完整探针。Native explicit transaction 继续遵守任一 execution 失败整体 abort 的既有合同。

`xCreate` 成功只证明构造成功，不证明该插件会拒绝所有不认识的参数，也不证明任意文本的 `xTokenize` 永不失败。后续构建/查询遇到 tokenizer、I/O、资源或中断错误必须传播；不可把之前的探针成功当作允许返回部分结果或 fallback 的理由。

`EXPLAIN`、纯 prepare/validation 和 `SHOW` 不创建 FTS table、不执行 tokenizer 构造器、不隐式加载动态库；它们可以完成静态配置检查，但不宣称证明运行时可用性。执行时的探针不能被移入无副作用的 planning 路径。只做历史/reset 等 ref 操作允许指向当前不能检索的索引定义；真正检索时再按第 11.5.7 节失败。

#### 11.5.5 Cache 与历史 Snapshot

FTS physical content 继续放在 connection-local TEMP derived cache，不新增持久化全文 backend 或第二套 canonical storage。Cache key 覆盖完整 Snapshot state identity、完整 IndexDefinition 和 FTS provider cache encoding version；staged Snapshot 必须包含自身 revision，不能与已提交状态混用。定义同名但 tokenizer/参数不同，不得复用旧 cache；不以 current Branch head 的 definition 重建历史 Snapshot。

缓存只有完整构建成功并写入 connection-local readiness marker 后才可复用。构建中断或 tokenizer 文本处理失败必须先撤销 readiness；若当前宿主 SQLite statement 允许清理，则立即清空或删除本次 TEMP 中间态。真实 SQL scalar 执行中，SQLite 可能因为外层 statement 仍持有该 FTS virtual table 而对 DROP/DELETE 返回 `SQLITE_LOCKED`；此时允许保留**没有 readiness marker 的 quarantined root**，但它不属于可读 cache，后续任何使用都必须先成功 reset/rebuild，绝不能读取其中的部分内容。这样 cleanup 的物理回收可以延后，但 correctness 不依赖回收成功。Cache 被删除、connection 关闭或数据版本改变后，从目标 Snapshot 的 canonical graph 和 definition 重建。重建不创建 Commit、不移动 Branch、不写 `main`，也不使用额外 connection 来绕开注册或只读限制。宿主不允许所需 TEMP 操作时明确失败，不以忽略 tokenizer 的扫描代替。

节点/关系和多 Property 的既有文本抽取行为继续保留。创建 cache 直接把原文本交给 FTS5，分词由 FTS5 调用 tokenizer；不能通过存储预分词文本替换 canonical Property。读 cache 之前解析目标版本是否存在该 Index，DROP 后当前版本不再可用，但保留该 definition 的历史 Snapshot 仍按历史配置尝试重建。

#### 11.5.6 Query-time analyzer 与结果语义

`db.index.fulltext.queryNodes` / `queryRelationships` 继续支持 `{skip, limit, analyzer}`。省略 query-time analyzer 时使用目标 IndexDefinition 的完整 specification；显式 analyzer 同样是 FTS5 specification STRING，仅影响本次查询文本的分词，不能改写 definition、产生 Commit 或用查询 tokenizer 重新解释已索引文档的 token。

`skip` 默认 0，省略 `limit` 不额外施加结果数限制；二者显式提供时必须为非负 INTEGER，拒绝 `null` 与错误类型。显式 analyzer 遵守第 11.5.2 节的 STRING/非空/NUL 规则，不将空值当作“使用默认”。对实际执行的 procedure 调用，空图、空查询字符串或 `limit:0` 不能跳过配置有效性/可用性检查；配置有效的空查询与零 limit 返回空结果。没有输入行而根本没有执行 procedure 的情况，仍遵守既有 Cypher clause 执行规则。

FTS5 正常 MATCH 路径必须保留 tokenizer 的 DOCUMENT、QUERY、QUERY|PREFIX 调用区别、token 次序、byte offsets 与 colocated synonym；不能把 query 当作一篇文档插入临时索引，再通过 `DISTINCT term` / vocab 交集近似执行。`queryString` 仍是既有全文查询表达式，而非 tokenizer specification；短语、布尔组合、Property 限定和前缀等已支持语义不能因更换 tokenizer 或 analyzer override 而丢失。

当 query specification 与 index specification 相同时直接使用普通 FTS5 cache。当它们不同时，采用限定在 Full-text provider 内的 FTS5 原生委托适配：FTS5 发起 DOCUMENT/AUX tokenization 时委托 index tokenizer，发起 QUERY/QUERY|PREFIX 时委托本次 query tokenizer；token、位置、flags、callback 返回码原样转交，分词算法仍由外部 tokenizer 实现。Adapter instance 固定绑定 index specification 与 connection-local handle；query child tokenizer 只在一次 MATCH 的作用域内替换，作用域结束 exactly-once 恢复并释放。嵌套/重入 override 必须按 LIFO 保存和恢复上一层 child，不得因为另一个 analyzer 已在作用域中就串配置或使用 process/connection 全局“当前 analyzer”。适配器只有内部固定身份，不提供用户注册/安装 API，不可覆盖宿主同名 tokenizer，也不能递归选择自身。

不同 analyzer 的执行可从同一 Snapshot 原文本构建一份额外 TEMP FTS corpus，文档始终由 index tokenizer 分词，MATCH 期间由 scoped query child 分词。由于真实 SQL scalar 外层 statement 可能在内部 MATCH 返回后仍锁住参与执行的 virtual table，不能要求每次 procedure 调用结束时立即 DROP 该表；因此 corpus identity 只覆盖 `(Snapshot state identity, IndexDefinition, provider encoding version)`，**不包含 query analyzer 或 query string**。同一 Snapshot/IndexDefinition 的后续 override 复用这份 connection-local derived corpus，只重建本次 query child tokenizer；不同 analyzer 不产生无限增长的 TEMP table。corpus 删除、Snapshot/definition 改变或 connection 关闭后按 canonical graph 重建，不能跨 Snapshot/definition 复用，也不变成持久化 backend。普通无 override 的 warm query 不依赖该额外 corpus，也不承担其重建成本。

原生 API 使用宿主 FTS5 API，保留 SQLite 3.45.0 最低 runtime；v2 方法只能在 `fts5_api.iVersion >= 3` 时访问，不假设宿主和编译 headers 同版本。本次不暴露 locale 配置；可使用满足本次无 locale 合同的原有 API。需要的 FFI 只封装在小范围 provider adapter，覆盖构造部分失败、exactly-once 释放、callback error/panic containment 与 connection 生命周期，不放宽 workspace-wide unsafe 规则。

普通与 override 路径都通过 FTS5 MATCH/`bm25` 产生候选，以现有非负 score 映射按相关性降序排列，同分按 element identity 稳定排序。Graph View 的 Node/Relationship（包括 endpoint）visibility 必须先于 `skip`/`limit` 计数；不得让 hidden result 消耗分页名额。分数保持 FLOAT，不承诺与 Lucene 数值一致或可跨 query/不同 tokenizer 比较；修复旧 override 的词集合近似，不保留其错误评分算法作为兼容层。

#### 11.5.7 运行环境、失败与可复现性

Tokenizer 注册是 connection-local runtime capability，不在数据库文件中持久化。连接池的每个实际 connection、reopen 后的 connection、读取历史 Snapshot 的 connection，都由宿主负责注册所需 tokenizer。另一个 connection 注册成功不构成当前 connection 可用的证据。

缺少 tokenizer 或必要外部资源时，使用该配置的 CREATE、cache build/rebuild、实际 query 明确失败，绝不静默改用 `unicode61`；成功的零结果也不能掩盖已执行调用的配置错误。Schema 读取、`SHOW INDEXES` 和不检索全文的普通 graph read 不要求 tokenizer 当前可用。

| 失败位置 | 对外错误和状态要求 |
| --- | --- |
| DDL 配置形状/未知 key、FTS5 specification/名称或构造参数错误 | `SCHEMA_ERROR`；不发布该次 Schema/Commit/Branch 变化 |
| 查询 options 形状/未知 key、不可用 analyzer 或 FTS5 query 表达式错误 | `SEMANTIC_ERROR`；不退化为空结果或另一种 tokenizer |
| SQLite busy、I/O、只读、资源耗尽或中断 | 保留现有 `BUSY` / `IO_ERROR` / `RESOURCE_ERROR` 与 SQLite code；不统一伪装成“未注册” |
| 清理失败、adapter invariant 失败 | `INTERNAL_ERROR` 或现有更具体错误；遵守 fail-closed cleanup，不继续使用不确定的中间态 |

消息至少说明 Full-text Index 和失败阶段；不得为了诊断把完整第三方 arguments、私有词典路径或源文本无条件返回。第三方 extension 本身是宿主信任的 native code，Lithograph 不能 sandbox 它的文件访问、外部副作用或算法错误；SQLite rollback 只覆盖本次数据库状态。

Specification 本身会进入可由 SHOW/历史读取的 Schema，应用不应把 secret 放进 tokenizer arguments；错误脱敏不会把已主动写入历史的配置变成秘密。

Versioned specification 固定的是**名称与参数**，不是第三方二进制、词典文件或实现版本。要跨机器/reopen 重现同一 Commit 的全文结果，应用必须固定兼容的 SQLite runtime、tokenizer 实现及其外部资源。同一名字下热替换 tokenizer 或原地修改词典不在运行中 cache 的支持合同内；应用需结束相关 cursor、关闭并重新建立 connection。需要同时保留两种分词行为时，由应用使用不同注册名或插件支持的稳定参数表达，并保留历史所需资源。Lithograph 不伪造不存在的插件版本指纹/注册代次 API，也不承诺检测宿主背后的所有实现变化。

#### 11.5.8 设计取舍与证据

采用一个 versioned specification STRING，而非新 Cypher keys、持久化 name/arguments 双模型或 analyzer 白名单：解决任意已注册 FTS5 tokenizer 和其参数可达性，代价是 analyzer 值不具 Neo4j 跨 backend 可移植性，且应用负责依赖版本。保留 query-time override 的原生委托边界，是为修复现有词集合近似不能保留查询结构的问题；不是给尚不存在的插件需求增加框架。

SQLite specification quoting、connection-local API、tokenizer flags、v1/v2 边界与 Neo4j analyzer 对照见 [FTS5 tokenizer 研究证据](research/fts5-tokenizer-contract.md)。实施与全部验收见 [Phase 12](development/phases/12-fulltext-tokenizer.md)；本节不以设计完成代替 runtime/test 证据。

### 11.6 Vector

Vector property 保留 dimension 与 coordinate type。Vector index 使用 HNSW ANN architecture，并支持 Cypher 25 vector index metadata、similarity、additional filtering properties 与 `SEARCH` subclause。

HNSW physical graph 是 derived cache，可以按 `(index definition, commit)` 重建。Vector cache 的缺失不能改变语义；没有 cache 时可以使用 exact scan 作为 correctness fallback。

### 11.7 Persistent Standard Index Base + Delta（Phase 11）

本节覆盖 Node/Relationship 的 Range、Text、Point 和 Relationship Lookup 物理内容；Node Lookup 继续使用既有 Label access path。Full-text/HNSW 暂不换存储架构，Phase 11 先扩大其测量与回归覆盖。目标是**已有物理索引跨 connection/reopen 可复用，少量图变化不触发整域重建**，不是承诺删除全部缓存后的首次 query 无构建成本。

#### 11.7.1 Generation identity 与持久布局

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

范围/编码合法性、manifest 与 entries 的对应关系由 storage primitives 验证，不依赖宿主 `foreign_keys` 开关。Key encoding 沿用第 5.3、11.4 节的语义；`value_blob` 保留必要的精确 recheck 值。索引前缀固定是 generation + owner kind + property ordinal，后接 equality、typed range、text 或 spatial key，并以 owner identity 处理同值重复。Relationship Lookup 使用 generation + owner kind + token + owner identity。采用固定、按非空 key family 过滤的 partial secondary indexes，避免给不适用 family 填入大量全 NULL key；相关 SQL 必须包含匹配 predicate，并经真实 plan 验证。构建一个 generation 不得 DROP 或重建其它 generation 正在使用的全部 secondary indexes。

Persistent generation 只覆盖 committed canonical state，不缓存某个 Graph View，也不持久化 Native staged state 或 Merge candidate。数据库文件本身隔离 database identity；内存 handle 还必须绑定原 connection/database，不能仅凭一个 generation number 跨库复用。

#### 11.7.2 Read path 与 delta overlay

```text
target Commit + target IndexDefinition
 -> compatible first-parent anchor generation
 -> collect relevant changed owners from anchor..target Layers
 -> indexed base candidates minus all changed owners
 -> union matching final-state values of changed owners
 -> Cypher predicate recheck / Graph View / semantic LIMIT or aggregation
```

相关 owner 包含 Node add/delete、Label membership change、被索引 property set/remove，以及 Relationship add/delete/type-domain/property change；即使 owner 的新值不再匹配，也必须屏蔽旧 base entry。Composite index 任一组成 property 或 membership 改变，都重新获取该 owner 的完整最终 tuple。Staged clause 与 Merge candidate 使用相同规则，但 cache key 额外绑定 state revision/candidate identity；不能把只有 committed target 身份的 cache 用于 staged state。

Base 与 delta 的候选合并必须 bounded/streamed，按实际 predicate key 分页；不能先把全部 matching owner 收集为无界 `BTreeSet`，也不能每一输出页重新计算同一 changed-owner set。结果需要重排时按第 7.3 节 spill，不能靠全域 scan 或大 OFFSET 模拟 indexed pagination。删除/添加 Label、property remove、相同值、复合键缺项、跨类型 ordering、`null`、并行 Branch 与历史 query 都必须与 canonical scan oracle 一致。

小变化只产生与**相关 Layer delta / changed owners**有关的解析和 point read；不复制整个 base generation 为每个新 Commit 建一份 index。Empty-delta Commit 与无关 Label/property write 不应触发 full index build。跨越较长 lineage 的 anchor 查找允许 query/connection-local 有预算的 immutable metadata cache，不能为每次查询扫描完整 Commit DAG。Delta 超过内部内存预算时允许 TEMP spill 或正确的 bounded scan fallback，并在诊断中明确标记；不能因缓存预算不足截断结果或悄悄使用过期 base。

#### 11.7.3 Build、publish、read-only 与 maintenance

以下是不同状态，不能在 benchmark 中混为一个“cold”数字：

| 状态 | 行为 |
| --- | --- |
| generation 完整且兼容 | 直接从持久 B-tree seek；新 connection 不重新构建 |
| 小 delta | 复用 ancestor base + delta，生成 bounded query-local overlay |
| generation 不存在、未完成或 encoding 不兼容 | 正确的 canonical scan / TEMP materialization fallback；不得返回不完整结果 |
| 明确重建或新 Index DDL | 在拥有 write authority 的 boundary 构建、原子发布 generation |

普通 read、`lithograph_rows()`、只读 SQLite connection、`EXPLAIN`、validation 和 Merge candidate inspection **不得**为 cache miss 隐式写 `main`、升级 storage format 或打开辅助 write connection。可用的 TEMP 仍只是 query-local 可重建数据；宿主连 TEMP write 也禁止时使用流式 canonical fallback。`EXPLAIN`/validation 不执行 index rebuild。Cache miss 的慢路径必须被测量，而不是从延迟报告删除。

新 Index DDL 在既有第 11.2 节 write boundary 内生成与最终 Schema/Commit 对应的物理 generation；Native transaction 中未提交的 index 可使用 staged/TEMP 内容，只能在最终 commit 时发布到 committed identity，abort 不得留下可见 generation。既有普通 graph mutation 不逐次重建所有 index；后续 read 使用 delta，maintenance 可以在当前目标 Commit re-anchor。

为使持久 cache 删除/失效后的恢复无需 DROP/CREATE 逻辑 index、无需伪造 Commit，Phase 11 增加一个独立维护 procedure（不是新 Cypher grammar）：

```text
CALL lithograph.index.rebuild(name, version)
YIELD name, commit, indexedEntities
```

两个参数均为非空 STRING；`version` 使用第 10.2 节已有 `commit/`、`branch/`、`tag/` descriptor，取得 writer 后解析并 pin。目标 Schema 中必须存在该 name，且属于本节支持的 Standard Index family；缺失 name、不支持的 kind/Node Lookup 或非法参数返回 `INVALID_ARGUMENT`，version 解析沿用已有稳定错误。只重建该 target 的完整 canonical index，不解释为 view-local index，不创建 Commit、不移动 ref。结果固定一行 `name: STRING, commit: STRING, indexedEntities: INTEGER`；`summary.queryType = "version"`、`summary.commit = null`、graph/schema mutation counters为0。结果中的 `commit` 是实际 anchor，`indexedEntities` 为本 generation 索引的 owner 数，不是 property-entry 数。

该 procedure 只能独立调用并后接 `YIELD`/`RETURN` 等只读结果处理，不与 graph mutation 或其它维护 operation 混在一个 execution。通过 `lithograph()` 或普通 Native execution 调用；`lithograph_rows()` 返回 `READ_ONLY_ADAPTER`；Native explicit transaction、transaction-owning subquery 或 Merge candidate 中返回 `TRANSACTION_BOUNDARY_REQUIRED`。`at` 返回 `READ_ONLY_SNAPSHOT`，`branch`、`author`、`message`、`graphView` 等不适用 options 返回 `INVALID_ARGUMENT`。`SHOW PROCEDURES`/validate/EXPLAIN 必须认识该 procedure，但 validate/EXPLAIN 无写副作用。只读文件的真实执行返回既有 I/O/read-only 错误，不吞掉用户明确请求的 rebuild failure。

Rebuild 和 DDL 使用现有 invocation SAVEPOINT/transaction discipline：从 pin 到 publish 在同一 SQLite transaction，边扫描边分批编码/写 entries，最后置 `complete=1`，成功后才让其它 reader 看到。重建已存在 generation 时旧内容的移除与替换同样原子；failure、cancel、disk-full 或 crash 只留下旧完整 generation 或无 generation，不留下可被使用的半成品。不会引入异步 worker、server、持久 build job 或 request-id 系统。此最小方案的全量重建可能长时间持有 writer，必须单独报告 build、writer-wait/hold、临时空间与峰值内存，不能把“最后设置 complete 很快”描述为整个重建的 writer 很短；使用者应在维护窗口进行全量 rebuild。

完整构建Relationship Lookup本来就需要枚举全部Relationships。Relationship property generation有可复用的兼容Lookup/type访问路径时优先使用；不存在时，显式首次全量build允许一次有界canonical枚举并记录其全部成本，不能称为type seek。已经ready的property/Lookup读取不再重复这一过程。除非新的工作量证据证明必要，不为构建捷径追加一套覆盖所有Relationships的重复永久索引。

Generation 成为 complete 后视为不可原地改写的内容。Read guard 保护正在使用的 generation；maintenance 负责成组删除 manifest/entries，不能通过读路径清理数据库。默认每个 definition 保留至多两个 complete anchors（保留本次目标并逐出最旧的其它 anchor），这是可调整但不公开配置化的性能策略；被逐出版本继续使用 ancestor/fallback。显式 canonical GC 同时清理 anchor 已不可达的 generation；generation **不是** canonical GC root，不延长用户已删除历史的生命周期。Manifest/entry count、encoding 和被访问 payload 的可检测异常使整个 generation 失效并回退，不允许跳过坏 entry 返回少量“正常”结果。完整 cache-vs-canonical 检查属于显式 integrity gate，普通 query 不全量重算 index checksum。真正的 canonical corruption、内部 schema/trigger 篡改继续 fail closed，而不是伪装成 cache miss。

#### 11.7.4 Adoption boundary

Cache失效在首行输出前被发现时，可以切换canonical fallback重新执行。已经向调用方发出结果后才发现损坏，不得从头fallback造成重复行或返回成功SUMMARY；除非能证明continuation的等价性，否则使用既有 `STORAGE_ERROR` 终止该execution、清理read资源，要求显式完整性检查/重建后重试。常规maintenance删除generation必须同时使manifest失效并原子清理entries；离线任意修改单个entry而保留完整marker属于篡改，不能承诺每次point query在不全量核验的情况下都能检测。

可删除的 derived data 在这里指 generation manifest/entries，而不是任意修改内部 table definition。Format 3 的结构仍由第 4.1 节 exact inventory 校验；缺表、错列、额外 trigger/index 不能被静默当作可用缓存。完整重建入口解决 payload 层缺失；结构损坏沿用明确的恢复/迁移检查。`lithograph_init()` 的 format migration 只建立空 cache 结构，不在迁移期间扫描所有图数据建立每个索引。

本节不改变 Graph Type/Constraint 的校验域与第 11.4 节 type proof。不能仅因为 persistent index 看起来完整，就把 derived entries 当成所有 canonical owner/value 的可信替身；任何约束验证加速必须另有针对相同 target state 的等价性证明及失败回归。

## 12. LOAD CSV 与 External I/O

`LOAD CSV` 支持 `file://`、`http://` 与 `https://` source。读取权限继承宿主进程的 OS / network authority；Lithograph 不注入隐藏 credentials。

I/O error、malformed CSV、type/constraint error 按 Cypher query failure 传播。普通 `LOAD CSV` 位于当前 query transaction；`CALL ... IN TRANSACTIONS` 使用第 9.6 节 transaction semantics。

Native explicit transaction 已从 `tx_begin` 起持有 single-writer ownership，因此不接受 `LOAD CSV`；`tx_execute` 在 external I/O 开始前返回 `TRANSACTION_BOUNDARY_REQUIRED` 并按第 9.2 节 fail-closed abort。需要批量导入时使用普通 `LOAD CSV` 或 Cypher `IN TRANSACTIONS`，不把网络/文件等待时间包进 multi-execution version-atomicity boundary。

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

## 14. Integrity、Recovery 与 Migration

### 14.1 Integrity Invariants

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

上述完整检查是显式 maintenance/integrity surface，不是每个普通 query、graph/version API、Native `tx_begin` / `tx_execute` 或 validation call 的隐式前置全库扫描。普通 initialized gate 只执行第 4.1 节定义、可在 bounded metadata/schema cost 内完成的结构性校验，包括 metadata marker、storage format、reserved internal-schema inventory、TEMP internal trigger 与 canonical table/index shape；Commit/Layer/Schema hash 重算、完整 DAG/ref/referential/checkpoint consistency 属于显式 `lithograph_integrity_check()`、初始化/迁移验证和 recovery/maintenance gate。需要证明完整 immutable history 未被离线篡改时，调用方必须显式运行 `lithograph_integrity_check()`。该边界不降低 corruption detection：显式 integrity surface 仍执行完整检查，运行时访问自身触及的 canonical object 也继续 fail closed；它只禁止普通 API 每次 invocation 重复扫描全部 canonical history，否则 read/write latency 会随全图/全历史线性放大并违反第 17 节 large-scale invariant。

### 14.2 Crash Recovery

Canonical write 与 Branch move 共用 SQLite transaction，因此 crash 后只允许出现 commit 前状态或 commit 后状态，不存在 Branch 指向半写 Layer 的合法状态。SQLite recovery 完成后 Lithograph 再执行自身 metadata/integrity checks。

Native explicit transaction 在 `tx_commit` 前没有 public intermediate Commit；process crash 或实际 SQLite connection teardown 会由 SQLite rollback 未提交 transaction，恢复后只能看到 `tx_begin` 前的 base State。`tx_commit` finalize 期间仍服从同一 canonical write + Branch move crash boundary，只允许完整旧 State 或完整新 Commit。Explicit transaction 的 connection-local staged state 不进入 storage format，也不能在 reopen 后恢复为“悬挂 transaction”。

Commit Data set/clear 与 Tag create/move/delete 同样必须是单 SQLite transaction 的原子 sidecar/ref mutation；crash/reopen 后只允许看到操作前或操作后状态，不允许出现半写 JSON、Tag 指向不存在 Commit 或 ref/data 与返回成功状态不一致。

Merge Session start/resolve/abort 每次都是短 SQLite transaction，crash 后只能观察到该 operation 完整发生前或后的一版 session/revision/resolution state。`merge.finalize` 把最终 Commit/ref move（若有）与 Session 删除放在同一 SQLite transaction，因此 crash/reopen 不允许出现“Branch 已移动但 Session 仍可重复 finalize”或“Session 已删除但 Merge Commit/ref move 没有发生”的合法状态。Open Session 本身允许跨 restart 恢复，不需要保持原 connection。

### 14.3 Storage Migration

Storage format version 记录在 `_lithograph_meta`。升级迁移必须：

- 在 SQLite transaction 中执行；
- 保持已存在 Commit ID 和 history semantics；
- 迁移失败整体 rollback；
- 新 Engine 继续读取历史 `format_version`；
- 旧 Engine 遇到更高 format version 直接拒绝写入和读取需要新格式语义的 graph。

首个正式 migration path 是 `1 -> 2`：增加 Commit Data / Tag sidecar 与 Merge Session operational storage，以及对应 integrity / GC semantics；不改写任何既有 Commit、Layer、Schema object 或 hash input。Migration 在一个 SQLite transaction 内创建新 internal objects、把 `storageFormat` 提升到 `2`，失败时整体 rollback。Format `1` database 不存在 Commit Data / Tag / Merge Session，因此迁移不需要为历史 Commit 合成 annotation、ref 或 workspace；升级后三者从空集合开始。

### 14.3.1 Performance Storage Format 3（Phase 11）

Phase 10 的 format `2` 保持已实现历史基线。Phase 11 已完成实现，current format 为 `3`，仅为第 11.7 节持久 derived index 增加固定 table/index inventory，不重写 Node/Relationship/Layer 的 canonical 编码。

- Fresh database 的显式 `lithograph_init()` 创建 format `3`；format `2` 的显式 init 原子执行 `2 -> 3`，format `1` 的 init 在同一外层 migration transaction 完成 `1 -> 2 -> 3`。任何一步失败回到原格式及原 schema/metadata，而非留下半升级的 format `2`。
- 保留 `databaseId`、全部旧 Commit ID/各自 `format_version`、Layer hash、Schema hash、parents、Branch/Tag、Commit Data 与 open Merge Session/resolutions。新 Commit 使用新 engine 的 format `3` hash input；旧 Commit 仍按原1/2编码验证，不能全库 rehash。
- 新 Engine 对尚未 init 升级的 format `1/2` 保留既有 legacy read 能力及 TEMP/canonical fallback；任何会持久化 graph/schema/ref/session/cache 的 operation 都先返回 `STORAGE_ERROR` 并提示显式 `lithograph_init()`，不得在旧 schema 写 format `3` Commit。`lithograph_version()` 仍报告实际旧格式与支持范围，`EXPLAIN`/validation 不隐式迁移。所有 legacy 检查使用该版本自己的 exact inventory。
- 旧的 maximum-format-2 Engine 遇到 format `3`，按既有 `FORMAT_TOO_NEW` 合同拒绝 graph read/write，不尝试忽略新 reserved objects 继续工作。没有自动 downgrade；回退需要迁移前的完整数据库备份，或使用新 Engine，不能删 cache 表再修改版本号。
- Migration 只增加空 derived structures 和更新 metadata；已有必须执行的 integrity validation 不被省略，但不把全量 cache build 混入 migration。后续新 index DDL 或显式 rebuild 填充内容；普通只读查询可先走 fallback。
- 初始化/迁移、exact-schema collision/TEMP trigger、reopen、crash rollback、mixed-format Commit DAG、GC 与六平台 interoperability fixtures 全部需要扩展到 format `3`。不能因数据可重建就免除 storage-format 变更的测试。

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

`graphView` 不是 authorization boundary：它只约束一次 execution 的可见 Property Subgraph。能够直接调用 Lithograph 且自行选择 options 的主体可以省略该 option 访问完整 graph；需要强制隔离的上层必须控制 execution surface 与 option construction。

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
- Graph View 不能通过预先 materialize 整个子图实现；scan/seek/expand/search 必须在现有 Snapshot access path 上按需执行 visibility check，且不得因 view 导致本可 seek 的查询退化为无条件全图扫描；
- 10M Node / 100M Relationship benchmark tier 必须作为 release hardening 的真实规模验证，覆盖 traversal、indexed lookup、write、history、diff 与 search；通过条件是正确完成、无 OOM、无意外全图扫描，并建立可持续 regression baseline。
- Merge conflict enumeration 必须 bounded/pageable；大量 conflict 的 start/list/resolve/finalize 不能要求一次把全部 conflict 或完整 candidate materialize 到 caller memory。Open Session 只持久化 pinned inputs + resolution set，candidate/conflict 可以重算或临时 spill。

### 17.1 Performance Evidence Contract（Phase 11）

Phase 10 的成功记录保留为功能/规模验收历史；Phase 11 必须在当前代码上重新证明物理访问路径及资源边界。单次 wall time、少量结果行、`AdjacencySeek` / `IndexSeek` 名称或 logical `dbHits` 都不能代替该证明。旧 baseline 及其限制见 [性能证据](research/phase11-performance-evidence.md)。

Benchmark 报告至少保存：Git commit 与 dirty-tree digest、fixture seed/version/真实 cardinality、pinned Commit、数据/索引量与分布、CPU/RAM/OS、Rust/build profile、实际 SQLite version/compile options、journal/synchronous/cache/temp 参数、读取 adapter、并发数、batch 大小、缓存状态与重复次数。不得通过关闭 durability、安全检查或减少 fixture cardinality 获得未标注的“优化”。Fixture 构造与完整 integrity check 单独计时，不混入或静默从被测 query 中移除工作。

每次查询统一从参数/options 解码或 Core prepare 入口计到全部结果被消费/释放，分别报告 prepare、Snapshot resolve、cache lookup/build/overlay、first-row、完整消费和 serialization/callback 成本。Core、Native callback 与 SQL `lithograph_rows` 分别测量，不直接比较不同 adapter 的数字；`MATCH ... RETURN 1` 必须真正消费每行，不能用 `count(*)` 或预知 cardinality 替换。旧 runner 对 streaming query 在 prepare 后开始计时，对 `execute` 则包含 prepare，新的报告必须消除这种口径差异。

缓存实验固定分为四类：同 connection 的 warm read；新 connection/进程但 persistent generation 存在；generation 缺失/被删除后的 fallback 与显式 rebuild；小 delta 后的 ancestor-generation read。它们分别统计，不汇总成一个 P95。OS page cache cold 只有明确完成并记录隔离方法时才能如此命名；仅重开连接仍可能是 OS warm。统计、checkpoint、index readiness 与测试顺序都要记录，不能用 preceding EXPLAIN/查询隐藏预热成本。

性能诊断保存底层 SQL template/绑定条件、`EXPLAIN QUERY PLAN`、公开 `sqlite3_stmt_status` 的 VM_STEP / FULLSCAN_STEP / SORT 等以及 Engine 的 resolved-state 构造次数、Layer 加载数、base/delta owner 检查数、generation build 次数。后者是 internal/test instrumentation，不增加公开 query options 或承诺新的 ABI metrics 字段。FULLSCAN_STEP 为零不能排除错误的宽 index-range scan；必须结合 VM work 和无关数据规模增长实验。SQLite 32-bit statement counters 在溢出前分段采样/reset 并聚合到宽计数，溢出/不可用标为无效证据，不能报告小值通过门禁；可选 scanstatus 不成为 stock runtime 的必需编译选项。

固定 degree/result/相关 delta，将无关图数据增至原来的10倍时，邻接与 ready index path 的实际读取工作不得近似线性增长。工程门禁为 `work(10×unrelated) ≤ 2×work(unrelated) + 1000 VM steps`，汇总一次query的全部相关SQLite statement，配合正确endpoint/type plan和不必要SORT检查；允许B-tree的对数变化，不允许宽主键范围扫描。大结果集 memory 包含 resolver、overlay、visibility、index candidates、row buffers、SQLite cache 与 TEMP spill；报告 process peak RSS、增量 RSS、TEMP/WAL/disk bytes。不能只证明输出256行一批，就声称整个 executor 的内存有界。

### 17.2 Phase 11 性能验收目标

以下是针对 **Apple M2 / 8 CPU / 16 GiB、Release build、同一固定 SQLite runtime 与 10M Node / 100M Relationship fixture** 的工程目标，不是已测结果或跨硬件 SLA。Phase 11.1 在优化前固定可重复的 before 基线；Phase 11 完成时须达到下表与结构性门禁。更换机器/runtime 要重建同机 before/after，不能混比；不能在观察优化结果后通过放宽目标或减少数据把失败改成通过。

| 工作负载/状态 | 完成目标 | 计量边界 |
| --- | --- | --- |
| 单起点、固定小 degree 的 typed/untyped one-hop，含0结果 | warm P95 ≤ 100 ms；新 connection P95 ≤ 500 ms | Core 和 Native 各测；结果完整消费，persistent state 已存在 |
| 10M Label rows streaming | 中位数 ≤ 60 s，最慢一轮 ≤ 90 s | Core 与 Native 分开满足；不得以 count 替代 |
| 单 hub 的1M outgoing rows streaming | 中位数 ≤ 30 s，最慢一轮 ≤ 45 s | Core 与 Native；包含页间推进、row/visibility工作 |
| 已构建 Range Index 的 equality，命中1行或0行 | warm P95 ≤ 50 ms，新 connection P95 ≤ 500 ms | 包含 prepare/metadata，full generation build count = 0 |
| 同一 index 返回1000行的 range read | warm P95 ≤ 250 ms，新 connection P95 ≤ 1 s | 完整范围结果；不能从 offset0重复扫描到本页 |
| 相同 indexed domain 上1000 owner 变化后的 equality/range | P95 ≤ 1 s，full generation build count = 0 | ancestor base + delta；包括旧值移除/新值命中，不预先重建 |

新 connection 项不包含 shared library 编译、显式 init/migration 或 OS cache purge，但必须分别报告 open/load 成本。上述长 streaming workloads 至少独立运行3次，报告每次、中位数和最大值，不用3个样本声称 P95。短查询warm集合至少5个connection、每个20次以上；新connection集合至少100次独立重开，每次只采第一条被测query。两类各自报告 P50/P95/max 和全部失败，不能将warm样本充作reopen样本。`0 ms` 不能写成零成本，计时使用微秒或更高精度。

10M/100M 核心 read workload 的总 process peak RSS 目标 ≤ 1 GiB；在固定 graph state / batch / cache budget 下，将同一 streaming query 的消费行数从1M增加到10M，额外 peak RSS ≤ 128 MiB，且无与输出行数同阶增长的 retained collection。全量 persistent index build 必须分批/可取消并单独报告内存、writer hold、磁盘体积和耗时，不能把其成本移到未计时 setup 后宣称 cold-build 已优化；它不适用 ready-index 的毫秒级延迟目标。

1000 Node batch+Commit、History、Diff、Native explicit transaction、10K-conflict Merge 各阶段作为非回退集：相同条件下新中位数不得超过 before 的 `max(1.20 × before, before + 10 ms)`。重复3轮仍出现超标时按 finding 处理，不靠删掉慢样本通过。Merge 的 read preparation、writer wait、writer hold 与 total finalize 分开测量；total finalize 时间不能当成 writer hold。

### 17.3 扩展压力场景与范围控制

Phase 11 必须增加相互独立的100K与1M Search corpus，不能把“大图里1000个 sample documents”写成百万向量压测。固定并记录 document 长度、vector dimensions/coordinate type、seed、similarity、HNSW build/search 参数、top-k、过滤选择性及历史/staged状态。至少有100K×1536和1M×128维向量场景；不同维度不比较成同一个延迟曲线。另有1M全文文档场景。10M向量可作为容量探索，不是本轮强制范围，也不得在未测前宣传支持该规模的低延迟。

Vector 用确定的 query sample 对 exact top-k oracle 报告 recall@k、分数/排序与 visible filtering；oracle construction 单独计时。若 exact top-k 的第 `k` 名与更多候选在实际 coordinate type / similarity 计算后具有完全相同的 cutoff score，则这些 cutoff tie candidate 在 recall@k 中等价，不能仅因 deterministic ID tie-break 选择了另一组同分结果而判为 miss；仍必须逐项验证返回 score、去重以及稳定的 score/identity 排序。ANN参数/数据相同条件下 recall@10 不得低于优化前，验收最低均值为0.95；不能降低 recall 换延迟。全文结果与相同语义 oracle 比较。报告 index build/加载/查询、cold/warm/new connection、peak RSS/磁盘以及历史 correctness；若这些新增场景暴露架构或 OOM 问题，完成最小必要设计修订后修复，不预先重写 FTS/HNSW。

Mixed-workload 验证覆盖1/4/8个 reader与一个 writer、不同 Graph View、Branch/Tag移动、GC、cache eviction/rebuild；每种并发配置持续压力至少30分钟，记录吞吐、P95、BUSY/retry、失败数、writer持锁、WAL与资源回收。WAL reader pin 不得让返回值跨 Snapshot 漂移；不能为提高吞吐忽略冲突/CAS。10K-conflict Merge 仍分40轮并保持 revision/candidate/finalize语义，先测热点再优化 resolution，不改变公众冲突协议。

性能改动按证据优先：邻接键序 → query-scoped resolved state → persistent Standard Index base/delta → 剩余实测热点。Row slot 化、Search算法重写、更多索引或并行调度不是默认任务；没有 profiling/acceptance 驱动就不增加。Phase 11 以固定验收集合闭合，不以“所有可能优化都做完”作为无限任务。

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

### D9 Graph View 是 Execution Context，不扩展 Cypher

- 决定：通过现有 SQL Bridge / Native ABI 的 `options.graphView` 选择 query-local Property Subgraph；selector 在一次 execution 内固定，visibility 随 Cypher 当前 clause graph state 计算；完整 versioned Schema / Constraint 不被 view 投影。Cypher text、AST 与 `CY25-2026.08` grammar 不增加 Lithograph-specific syntax。
- 依据：调用方需要让完整 MATCH/path/aggregation/Search/write 在同一个逻辑子图内执行；query rewrite 或结果后过滤无法保证 `count()`、path、top-k 和 mutation correctness。Cypher 25 的 `USE` 面向 graph reference / composite-database selection，不等价于同一 Property Graph 内的子图过滤。
- 备选：重新解释 `USE`、给 Cypher 增加自定义 clause、由调用方给每条 query 注入 `WHERE`、或为每个逻辑范围复制独立 database。
- 取舍：Planner/Executor/Storage/Search 都必须继承同一个 visibility contract，并在 read-write clause 与 transaction batch 边界重新依据实际 graph state 判断 membership；但不改变 storage format、Phase 03 frontend、Cypher compatibility Profile，也不建立持久化 named-view registry 或 per-view Schema。

### D10 Native Explicit Transaction 提供多 execution 的单 Commit boundary

- 决定：Native ABI 以 `sqlite3*` connection 作为 transaction identity，提供 `begin -> execute* -> commit | abort` 的 explicit transaction，不增加 opaque transaction handle。多个标准 Cypher current-graph execution 共享 transaction-local staged graph/Schema/Index state；成功 transaction 只创建一个最终 Layer/Commit 并只移动 Branch 一次。caller-owned SQLite transaction 继续只负责 durability atomicity，Cypher `IN TRANSACTIONS` 继续按 batch 独立 Commit，二者都不替代 explicit transaction。
- 依据：通用数据库调用方存在一个逻辑变化需要跨多个 query 读取中间结果、分配 identity、修改 graph + Schema/Constraint/Index，但版本历史只应出现最终一致 Snapshot 的需求。把每个 query 自动提交后再 squash 会产生真实 intermediate history；把 raw Structural Patch 变成普通 CRUD language 又会复制 Cypher mutation semantics。
- 备选：只允许“一条 Cypher query -> 一个 Commit”；用 caller-owned SQLite transaction 包裹多个 Commit；要求调用方构造 Structural Patch；完成后自动 squash/rewrite history；建立独立 server/session transaction layer。
- 取舍：Engine 必须维护跨 execution 的 staged snapshot、transaction-level temporal clock、final net-delta canonicalization 与 fail-closed cleanup；active transaction 持有 SQLite single-writer ownership，长事务会阻塞其它写入。换取的是标准 Cypher 25 仍为正常 mutation language，同时获得明确的 version atomicity 和 one logical change -> one Commit 语义。

### D11 Merge 使用持久 Merge Session 分离冲突解决与最终 Commit

- 决定：Three-way merge 先创建 durable、非历史的 Merge Session，pin `ours/theirs`，通过分页 conflict + incremental resolution 逐步得到 candidate；Session 以单调 revision 标识 resolution state，candidate 通过 `options.mergeSession={id,revision}` 只读检查。只有 `merge.finalize(session, expectedRevision)` 可以创建 Merge Commit/fast-forward 并移动 Branch，同时删除 Session。
- 依据：大型 merge 可能存在大量冲突，需要 AI/用户跨多个调用逐步解决；在整个交互期间持有 SQLite writer 会阻塞数据库，而一次性 `merge(source,resolutions)` 又要求 caller 把所有 conflict 放入一次上下文。上层系统还需要在最终 ref move 前检查自己的业务不变量。Pinned Commit + session revision + finalize target-head CAS 可以在不嵌入上层 callback、不保持长 writer transaction 的前提下保证“检查的 candidate == 最终准备提交的 candidate”。
- 备选：一次性 merge + 全量 resolutions；长生命周期 SQLite/Native transaction；merge prepare/finalize 但 candidate 只存内存；finalize-time application callback；让上层先 merge 再 revert invalid result。
- 取舍：format 2 增加 mutable Merge Session/resolution operational storage，GC 需要把 open Session 当 root，Version API 增加 session lifecycle 与 revision concurrency；换取 resumable conflict resolution、bounded conflict pagination、crash recovery、上层 pre-commit candidate validation，以及历史中始终只有最终一次 merge/fast-forward 结果。

### D12 性能优化保持版本语义，先修物理访问与生命周期

- 决定：第 8.3.1 节让邻接 cursor 匹配现有 B-tree；第 7.8 节让同一 query 共享 owned resolved state 与 read guard；第 17 节用实际物理工作量和统一计时验收。
- 依据：当前 scale baseline 与代码复查发现错误的邻接 access plan、逐 batch resolution 及混合计时口径；证据强度和限制见性能研究记录。
- 备选：添加重复巨型索引、单纯增大 batch、关闭 Graph View/constraint 检查、直接重写 executor。
- 取舍：需要迁移内部 cursor、resource ownership 和关键失败测试，但不改变 Cypher 语义、Commit identity 或公开 ABI；长 read guard 仍可能造成 WAL 增长。

### D13 持久 derived index 复用 anchor，不为每个 Snapshot 重建全域

- 决定：第 11.7 节采用 committed anchor generation + query-local delta；format 3 显式容纳其 exact schema；新 DDL 与单独 rebuild procedure 负责发布，只读路径不隐式写 `main`。
- 依据：TEMP Standard Index 在 connection 关闭后丢失，且 cache identity 按 Snapshot 分裂；持久化必须同时尊重 reserved-schema、只读 adapter、staged state 与版本类型语义。
- 备选：继续只用 TEMP、每个 Commit 复制完整 index、把 physical cache 纳入 canonical history、引入后台 server。
- 取舍：付出 derived disk space、generation cleanup 与一次显式2→3迁移；换取 reopen可复用和小delta不全量重建。缓存确实缺失时仍有 fallback/build 成本，显式全量 rebuild 仍可能持有长 writer，不能隐瞒。

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

Phase 11 的当前实现/性能观察、SQLite row-value pagination、INDEXED BY、read-transaction 与 statement-counter 依据及不采用边界见 [性能证据](research/phase11-performance-evidence.md)。
