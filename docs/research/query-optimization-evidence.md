# Lithograph 查询谓词与身份访问外部实现证据

**研究截点：2026-11-09。** 本文是研发证据，不是另一份设计真源，也不授权复制代码。目标合同见 [Query Engine](../design/query-engine.md#predicate-routing-and-identity-seek)，实施/验收见 [Phase 21](../development/phases/21-predicate-routing-identity-seek.md)。以下源码链接指向已核对的 Commit。

## 事故与本地证据的边界

- 2026-11-09 21:54:25（UTC+8），KG OS Prod v0.3.20 所在 1 GiB 容器被宿主内核 OOM Killer 终止（systemd 和 Docker exit 137，peak 1 GiB），object.patch 和 lithograph.execute 在持久请求跟踪中为 incomplete；生产 main Head 随后未变。该证据仅能确认异常退出和未形成 Commit。
- 近似原查询的 EXPLAIN 包含 NodeScan → Cartesian → NodeScan → Filter；751×751=564001 只是理论组合规模。不能由 EXPLAIN 单独断言实际驻留 564001 行或唯一 OOM 诱因。
- 在 Lithograph v0.3.4 源码中，query/plan/build.rs 对多 Part 插入 Cartesian；query/stream/match_pipeline.rs 在全部 Part 绑定后统一计算 step.predicate；query/mutation/execute.rs 的写入 MATCH 进入同一匹配管线；storage/snapshot.rs 已有按 NodeId 读取 Snapshot 存在性的基础能力。

## GitHub 项目算法对照

| 研究源码 | 实际处理机制 | 对 Lithograph 的适用边界 |
| --- | --- | --- |
| [DuckDB PushdownCrossProduct](https://github.com/duckdb/duckdb/blob/1f3fa9f0fc95c21c35894003b82eb8bd3857bb2d/src/optimizer/pushdown/pushdown_cross_product.cpp) | 依据条件绑定集合，左/右侧独立过滤下推；跨侧条件考虑 Join | 借鉴变量依赖判断；不复制整个 SQL Join Optimizer |
| [Neo4j idSeekLeafPlanner](https://github.com/neo4j/neo4j/blob/2994b43b44f35bf6520bd2667ab181bb76a45585/community/cypher/cypher-planner/src/main/scala/org/neo4j/cypher/internal/compiler/planner/logical/steps/leafplanner/idSeekLeafPlanner.scala) | 对 id/elementId 查找识别参数依赖并建立专用叶子计划 | 借鉴 Element ID Seek；保持 Lithograph 版本化身份和访问规则 |
| [FalkorDB push_filters_down](https://github.com/FalkorDB/FalkorDB/blob/56d00c3d830c23ea3f8a204727dc4ac3ac975a76/graph/src/planner/optimizer/push_filters_down.rs) | Rust IR 的 AND 分拆、变量覆盖判断和 Apply 边界处理 | 借鉴小规则 Pass；不引入不必要的独立 IR 树 |
| [FalkorDB utilize_node_by_id](https://github.com/FalkorDB/FalkorDB/blob/56d00c3d830c23ea3f8a204727dc4ac3ac975a76/graph/src/planner/optimizer/utilize_node_by_id.rs) | 把 Scan + id() Filter 换为直接 ID 查找 | 借鉴判别规则；本项目使用 elementId 的自身解析 |
| [FalkorDB Cartesian → Hash Join](https://github.com/FalkorDB/FalkorDB/blob/56d00c3d830c23ea3f8a204727dc4ac3ac975a76/graph/src/planner/optimizer/replace_cartesian_with_hash_join.rs) | 跨侧等值谓词变 Hash Join | 与两个独立 ID 过滤不同，本次不新增 Hash Join |
| [LadybugDB Filter Pushdown](https://github.com/LadybugDB/ladybug/blob/5668d9955a208acce4c6000b149f142e2dcb97df/src/optimizer/filter_push_down_optimizer.cpp) | 跨子计划分析可求值 Schema；部分等值过滤转 Join | 借鉴安全分发；SIP/SemiMask 留待独立规模证据 |
| [Memgraph Index Lookup](https://github.com/memgraph/memgraph/blob/caa636d2de7c120f9baf9f1b8cf96da1ee3d3cdf/src/query/plan/rewrite/index_lookup.hpp) | 把 Filter + ScanAll 转为可选索引访问 | 仅借鉴 Rewrite 判别；许可与实现结构不直接移植 |
| [GraphQLite Performance](https://github.com/colliery-io/graphqlite/blob/857b178ef48c1e901aa80e589f74bc89e9a4f066/docs/src/content/docs/explanation/performance.md) | SQLite 索引访问、批量节点/边的身份映射减轻重复查询 | 参考 SQLite 经验；不改 Lithograph 原生 Cypher / immutable 版本引擎 |

## 推荐选择及许可

先用安全的单变量 AND 下推、再用版本化 Element ID Seek；同时修复真实 Read/Write MATCH 运行路径与诊断覆盖。这两项直接针对已观察到的浪费；不扩大为全局 Join 重排、SIP、第二套索引系统、SQLite fork 或专用 KG OS 逻辑。

第三方源码许可证与发行条件不尽相同，参考行为和算法不等于可复制文件；若未来确需复用原代码，必须重新核对实际文件许可证与 AGPL/商业双许可证兼容及 NOTICE/attribution。本期独立实现，不复制上述第三方源码。
