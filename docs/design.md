# Lithograph 技术设计

`docs/design.md` 与下表列出的 `docs/design/` 专题共同构成 Lithograph 的唯一设计真源。本文保留产品定义、设计驱动与全局边界；每项详细合同由一个专题拥有，其余文档通过链接引用。

设计描述已确认的目标行为，不等于全部能力已经交付。当前实现、Phase 状态与验收证据见 [开发计划](development/README.md)，已发布能力见 [发布记录](releases/)；不能把目标 storage format 或 Managed Semantic 设计当作发布状态。

<a id="design-ownership"></a>

## 设计职责与阅读入口

| 设计范围 | 唯一拥有文档 |
| --- | --- |
| 产品定位、全局边界与设计驱动 | 本文的 [产品定义](#product-definition) 与 [设计驱动](#design-drivers) |
| Cypher 25 Profile、兼容范围、oracle | [Compatibility](design/compatibility.md) |
| SQLite SQL execution surface、options、外部 I/O、结果与错误 | [Interfaces](design/interfaces.md) |
| 图元素与值语义、Frontend、Planner、Executor、Graph View | [Query Engine](design/query-engine.md) |
| Canonical storage、编码、Snapshot、事务、恢复与格式迁移 | [Storage](design/storage.md) |
| Commit、Branch、Tag、Diff/Patch、Merge 及版本操作 | [Versioning](design/versioning.md) |
| Schema、Constraint、Index 公共规则与 Standard Index | [Schema and Indexes](design/schema-and-indexes.md) |
| FTS5 specification、全文查询与 cache | [Full-text](design/full-text.md) |
| Raw Vector、Managed Semantic、Embedding Provider 与 Provider-owned cache | [Vector](design/vector.md) |
| 部署、SQLite ABI、安全、规模与性能目标 | [Runtime](design/runtime.md) |
| 已采用架构决策的依据、备选与取舍；外部参考入口 | [Decisions](design/decisions.md) |

先读本文，再按任务读取拥有该合同的专题；跨域任务继续跟随专题内的依赖链接。例如 Provider/config 改动从 Vector 开始，只有真正改变 Lithograph canonical/derived storage 时才继续读 Storage；Merge 改动从 Versioning 开始，涉及事务或结果时再读 Storage / Interfaces。

<a id="source-discipline"></a>

## 文档职责

专题负责完整合同，入口只提供导航与全局边界，不重复参数表或状态机。跨文件引用使用具体主题链接和稳定锚点，不再依赖整篇文档的章节编号。发现矛盾时先定位拥有该合同的专题并修正受影响引用，不通过复制正文建立第二真源。

[开发计划](development/README.md) 只维护实施依赖、Phase 状态和验收，不重新定义产品行为；[用户手册](guide/README.md) 与 [API Reference](reference/README.md) 描述其标明发布基线的实际用法；[研究证据](research/reference-baseline.md) 保存外部来源及采用限制。历史开发日志保留当时事实，不能覆盖当前设计。

<a id="product-definition"></a>

## 产品定义

Lithograph 是一个运行在标准 SQLite 上的、可加载的 Property Graph 数据库扩展。它在同一个 SQLite 数据库文件内提供三项一体化能力：

1. 完整的 Cypher 25 当前图查询与数据库语义；
2. 面向大规模单机图的 Property Graph 存储、执行、Schema、Index、Full-text、Raw Vector Search 与 Managed Semantic Search；
3. 基于 immutable Commit DAG 的版本化状态图：每个 graph / Schema / Index **逻辑写单元**形成 Commit；普通 auto-commit query 是一个写单元，SQL explicit transaction 可以把多个独立 execution 组合成一个写单元。系统同时提供 Branch、Tag、可修改的 Commit Data、显式 empty-delta Commit、可分页 History、Time-travel、Diff、Patch、Merge、Rebase、Squash、Reset 与 Revert。Git / TerminusDB 是机制参考，不限定调用方如何解释这些状态。

```text
Application / SQLite client
            |
            | SQLite loadable extension API
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

<a id="design-drivers"></a>

## 设计驱动

<a id="sqlite-native-deployment"></a>

### SQLite 原生部署

Lithograph 必须是标准 SQLite loadable extension，不维护 SQLite fork，不要求独立 Server 或 Daemon。扩展通过 `sqlite3_lithograph_init` 注册公开接口，并使用 SQLite 自身事务、WAL、B-tree、文件格式与并发控制。

<a id="sql-only-execution"></a>

### Application-facing execution 只通过 SQLite SQL

Application / Database Client 只通过标准 SQLite SQL surface 调用 Lithograph，不同时维护另一套 application-facing Native query ABI。完整结果由 `lithograph()` 返回，增量 execution event stream 由 `lithograph_rows()` 返回；两者必须共享同一个 Cypher execution core 和事务语义，只允许结果消费方式不同。Embedding Provider 的 `EmbeddingProviderV1` 是 Lithograph extension 与 Provider extension 之间的插件 SPI，不属于 application-facing query API，也不受本边界删除。

<a id="cypher-semantics"></a>

### Cypher 25 语义兼容

Lithograph 不创建“类似 Cypher”的查询语言。公开图查询语言就是 Cypher 25。兼容要求以可观察语义为准，包括结果、类型、`null`、重复行、路径、更新、Schema、Constraint、Index、Search、错误和事务行为；仅解析相同语法但产生不同语义不算兼容。

<a id="versioned-foundation"></a>

### 版本化是存储基础，不是后加功能

版本历史从第一条图数据开始存在。图数据、Schema 与 Index 定义共同进入 Commit 历史。Branch 只移动引用，不复制完整数据库。历史 Commit 不被后续写入修改。

Commit boundary 属于版本模型的一部分：普通 mutating Cypher execution 默认各自产生 Commit；需要把多个 execution 视为一次逻辑状态变化时，调用方使用 SQL explicit transaction，在同一 staged Snapshot 上执行并只 finalize 一个 Commit。caller-owned SQLite transaction 只改变 durability visibility，Cypher `IN TRANSACTIONS` 只改变 batch transaction boundary，二者都不隐式重写 Commit 粒度。

Commit 的 graph / Schema / Index Snapshot 与 DAG lineage 是 immutable canonical history。调用方可以另外给已有 Commit 保存可修改的 **Commit Data**，也可以用 **Tag** 给 Commit 建立显式命名引用；Commit Data 与 Tag 都是 version-control sidecar state，不进入 Snapshot、Commit hash、Diff / Patch 或 Merge correctness。若某项业务数据本身必须随 Snapshot versioning、Cypher query、Constraint、Diff 或 Merge 一起演进，它必须保存为正常 graph / Schema 数据，而不是 Commit Data。

<a id="single-machine-scale"></a>

### 大规模单机图

遍历、索引、历史查询和结果返回不能依赖把完整图或完整结果集加载到内存。邻接访问的成本必须与命中的邻接数据相关，而不是与全部 Relationship 数量相关。`lithograph_rows()` 是真正的 pull-based execution stream：除 `ORDER BY`、`DISTINCT`、aggregation 等 Cypher operator 自身要求的 semantic barrier 外，Engine 不得为了 adapter 结果返回而先物化完整最终 row set；需要 barrier 时使用 bounded memory 与必要的 TEMP/disk spill。
