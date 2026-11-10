# Lithograph 查询优化器演进：外部算法与当前实现差距

**研究截点：2026-11-10。** 本文只保存公开设计、算法证据与适配边界；正式 Lithograph 行为由 [Query Engine](../design/query-engine.md)、[Storage](../design/storage.md)、[Schema / Index](../design/schema-and-indexes.md) 和 [Runtime](../design/runtime.md) 各自拥有，开发与验收见 [单一 Phase 22：高性能查询优化器与可预算执行引擎](../development/phases/22-high-performance-query-optimizer.md)。公开 URL 未固定到源码 Commit 时仅作为版本/文档参考，实施前需重新核对内容及许可证，不直接复制代码。

## Lithograph 已有基础与真实缺口

- 本地仓库当前为 HEAD `e245555` 加尚未提交的 Phase 21 工作区修改；Phase 21 的 `done` 仅代表本地验收，不等于 v0.3.4 已发行功能。
- 已有 AST / LogicalPlan / PhysicalPlan、Node/Relationship scan、Standard Index、NodeIdSeek、AdjacencySeek、流式 MatchCursor、query-scoped resource budget、TEMP spill、Checkpoint + Layer Overlay 与统计。不能将它们描述为缺失并重复实现。
- `query/plan/build.rs` 对分离 Pattern Part 仍存在简单 Cartesian 串行组合；没有完整的 DPhyp Join Enumerator、选择性 Join 算法集合或通用多路交集。
- `storage/snapshot.rs` 在 Snapshot resolve 时按 lineage 重建 Overlay；`storage/checkpoint.rs` 有 `create_checkpoint`，但当前常规写入路径未找到自动 Tiered 维护调用。Phase 21 的 100k Node 固定 ID 点查在无 Checkpoint 时比有 Checkpoint 慢得多，需分开治理版本解析和算子执行。
- `query/stats.rs` 已包含 Node/Relationship/Label/Type 统计，Phase 21 对非必要统计收集做了收敛；但索引 distinct、联合选择性、degree skew 与多 Join 候选计划所需统计仍不完整。

## 外部算法证据及引入边界

| 参考及来源 | 可借鉴机制 | Lithograph 的目标实现与限制 |
| --- | --- | --- |
| [DuckDB Internals](https://duckdb.org/docs/current/internals/overview)；[Join Operations](https://duckdb.org/docs/current/guides/performance/join_operations)（访问 2026-11-10） | Binder → Logical Planner → Rewrite → Join Order；DPhyp 对较小 Join Graph 做 DP 搜索，大图退化 Greedy 并有搜索预算 | 在现有 AST/Plan 中标注不可跨越的 Cypher scope、OPTIONAL、Apply、写 barrier；仅对证明可重排的 inner Join 组件枚举，不整套迁移 DuckDB Planner |
| [PostgreSQL Planner Statistics](https://www.postgresql.org/docs/current/planner-stats-details.html)；[Extended Statistics](https://www.postgresql.org/docs/current/planner-stats.html)（访问 2026-11-10） | 基数、NDistinct、MCV、correlation / functional dependency 改善过滤估计 | Derived statistics 按 Snapshot / index definition 选取；样本和数据偏斜有界，缺失回退 unknown；不把估算当作正确性真源或暴露用户属性值 |
| [SQLite Query Optimizer](https://www.sqlite.org/optoverview.html)；[ANALYZE](https://www.sqlite.org/lang_analyze.html)（访问 2026-11-10） | 访问路径选择、统计驱动、B-tree seek 和 `sqlite3_stmt_status` 访问度量 | 尽量复用现有 SQLite keyed access 和 `EXPLAIN QUERY PLAN`；SQLite `stat1/stat4` 不直接等同 Lithograph 历史 Graph Snapshot 的完整统计，不要求宿主额外编译选项 |
| [PostgreSQL Hash Join 执行源码](https://github.com/postgres/postgres/blob/master/src/backend/executor/nodeHashjoin.c)；[EXPLAIN](https://www.postgresql.org/docs/current/using-explain.html)（访问 2026-11-10） | 小 build 侧、分批 Hash Join、磁盘 partition spill、peak/batches 诊断 | 使用现有 query budget、SQLite TEMP / cancel / rollback；不新增第二套 `work_mem` 或不受控的进程级 Hash Table |
| [TerminusDB Delta Rollup](https://terminusdb.com/docs/delta-rollup/)（访问 2026-11-10） | Immutable delta 累积时自动合并成可重建层；减少深层 ancestry 查询 | 采用自动策略和有限维护预算，但保留 Lithograph 的 Commit/Layer 不变性；只在可写维护边界构建 derived Checkpoint，不能在普通 read / EXPLAIN 暗写主库 |
| [LadybugDB Semi-mask](https://github.com/LadybugDB/ladybug/blob/main/docs/semi_mask_in_scan.md)（访问 2026-11-10） | 查询内按候选 Node ID 建 Semi-mask / Bitmap，用于减少扫描 I/O，可同 Hash Join 配合 | 先在 NodeId/Label/Adjacency keyed access 证明能减少实际 SQLite VM/页访问；query-scoped、state/revision/Graph View 隔离，不能只在全表读完后过滤 |
| [Kùzu Factorization](https://kuzudb.github.io/blog/post/factorization/)（2023-01-20；访问 2026-11-10） | 在多对多 Join 之间保持压缩的 factorized intermediate，延迟扁平展开，优化重复计算与复制 | 仅给能证明语义等价的中间子计划提供 factorized representation；输出到 `lithograph_rows()` 必须按真实 Cypher multiplicity 逐行展开，不能改公开结果 |
| [Kùzu WCOJ](https://kuzudb.github.io/blog/post/wcoj/)（2023-02-22；访问 2026-11-10） | 环形/三角形图模式使用多路有序邻接交集，必要时和二元 Join 混用 | 将有限循环图 Pattern 编译成 multiway intersect，保留 typed/directed relationship、parallel relationship identity 与 Cypher path uniqueness；无适用索引或不等价时回退 |
| [Apache DataFusion Dynamic Filters](https://datafusion.apache.org/blog/2025/09/10/dynamic-filters/)；[Configurations](https://datafusion.apache.org/user-guide/configs.html)（访问 2026-11-10） | Build 侧的 min/max 等信息向 Scan 侧传递；TopK cutoff 反向约束读取 | 只在有物理 skip/seek 能力时启用，统计实时访问降幅；`ORDER BY/LIMIT`、NULL/ties、Graph View 与 Search 的范围内 TopK 保持原语义，不插入提前终止的错误优化 |

## 选型与实施顺序

1. 在 [Phase 22 Stage S22.1](../development/phases/22-high-performance-query-optimizer.md#stage-s22-1) 完善现有 Planner/Executor 的共享成本与预算机制，使后续算法是可比较的候选 physical operator 而非独立执行分支；此 Stage 必须产生真实的 access-path 决策和资源边界证据，不能仅写空抽象。
2. [Stage S22.2](../development/phases/22-high-performance-query-optimizer.md#stage-s22-2) 先控制 Snapshot 成本，再在 [Stage S22.3](../development/phases/22-high-performance-query-optimizer.md#stage-s22-3) 建立版本化统计、Relationship ID Seek、无索引扫描改善和安全谓词范围。
3. 统计可用且访问路径正确后，在 [Stage S22.4](../development/phases/22-high-performance-query-optimizer.md#stage-s22-4) 实现有界 DPhyp/Greedy + Index Nested Loop，在 [Stage S22.5](../development/phases/22-high-performance-query-optimizer.md#stage-s22-5) 实现 Grace Hash Join + TEMP spill；只有此时才能严肃比较图专有算子是否获益。
4. **全部实施** [Stage S22.6](../development/phases/22-high-performance-query-optimizer.md#stage-s22-6) 的 Semi-mask/SIP、Join Dynamic Filter/TopK Pushdown、[Stage S22.7](../development/phases/22-high-performance-query-optimizer.md#stage-s22-7) 的 Factorized/ASP-Join、[Stage S22.8](../development/phases/22-high-performance-query-optimizer.md#stage-s22-8) 的 WCOJ/Multiway Intersection；每项都有正例、适用边界、保守回退和 I/O/CPU/内存对照，不把“性能收益需证据”解释成可删除交付。

## 测量、许可和不确定性

- 各来源的原始性能数字针对其各自存储和数据分布，不作为 Lithograph 的目标值。使用同一 Lithograph 代码/SQLite/fixture 建立 Before/After，分离 prepare、Snapshot、physical runtime、完整结果消费、RSS/TEMP、写入放大、索引/Checkpoint 维护与 CPU。
- 强制覆盖 1k/10k/100k 逐级增长，并在相关 Phase 验证 1M 和既有 10M Node / 100M Relationship release tier；仅比较同一真实结果/相同语义的计划，不能通过减少输出、禁用约束或预先秘密物化获得提升。
- Graph View、pinned historical Commit、staged revision、mixed types / NULL、错误时序、OPTIONAL/Apply、关系重复/自环/路径唯一性、资源拒绝、取消和重启均属于适用性判定，不因优化跳过。
- 第三方项目许可证/具体文件许可证可能不同；只借鉴公开算法与观察结果并在 Lithograph 独立实现，除非未来另行逐文件审核许可证、NOTICE、兼容性。仓库不新增 DuckDB / LadybugDB / PostgreSQL / TerminusDB 数据库依赖。
