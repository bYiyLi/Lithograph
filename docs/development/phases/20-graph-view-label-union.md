# Phase 20：Graph View 多 Label 并集筛选

**状态：`done`（源码实现、本机完整 quality / CI、专项测试与 Phase-level review 已通过；尚未提交、推送或发布）**

## 1. 目标、范围与依赖

在既有 Graph View execution context 中增加 `requireAnyLabels`，使调用方可以一次指定多个 Node Label 的 OR 范围，并在相同范围内执行 Cypher、Full-text、Raw Vector 与 Managed Semantic；保持此前 AND / NOT 语义、读写与历史能力，不通过上层拆分查询、Top-K 后过滤或新 Cypher 语法实现。

前置：Phase 04–08、13、15、18–19 均为 `done`，在当前已交付的 v0.3.3 代码基线上开发。唯一行为合同是 [Query Options](../../design/interfaces.md#query-options) 与 [Graph View Execution Boundary](../../design/query-engine.md#graph-view)；搜索细则沿用 [Full-text](../../design/full-text.md)、[Vector](../../design/vector.md)，资源及性能边界沿用 [Runtime](../../design/runtime.md#large-scale-invariants)。

本 Phase 只修改 Lithograph 的 Graph View selector、执行和受影响的回归/用户手册。它不定义上层 Domain、实例归属、任意 Relationship / NodeId 子图 selector、访问权限或新的租户模型；不修改 `CY25-2026.08` grammar / coverage 分母、storage format、Commit identity、SQLite SQL surface 或 Provider SPI。不预设发布日期，也不把“计划就绪”记为代码交付。

## 2. Features 与实施顺序

### Feature 20.1 Selector、解析与 visibility

- 在 `ExecutionOptions` / `GraphViewSelector` 增加 `requireAnyLabels` 并维持空 selector 等于完整图、既有 `requireAllLabels` / `excludeAnyLabels` 的兼容语义；解析时严格校验成员、类型、`null`、重复项及与排除条件的组合。
- 在 `ResolvedGraphView` 的可见性判定中加入 OR，并核对 `is_full_graph`、`is_empty`、扫描起点与 Unknown Label 处理；未知 OR 候选不创建 dictionary entry、不提供匹配，所有 OR 候选均不存在时没有可见 Node；其它 AND / NOT 条件仍可独立使结果为空。
- 覆盖 AND + OR + NOT 的独立及组合语义；`requireAllLabels` / exclude 冲突仍拒绝，OR / exclude 重叠按排除优先而不报错。后续合法写入使 Label 变为可用时，不冻结查询起始时的 element allowlist。
- 核对全部 `GraphViewSelector` 构造位置及非空 View 判断：仅设置 `requireAnyLabels` 时，Schema / Index / Version operation 仍必须按现有规则拒绝与非空 Graph View 的组合，不能被误判成完整图。

### Feature 20.2 Read、Planner 与物理访问

- 复用 NodeScan、LabelScan、bound element lookup、IndexSeek 和已有遍历/路径执行。OR-only 的扫描必须涵盖全部候选 Label 并去重；有必需 pattern/AND Label 的查询仍可利用已有 seek 并做剩余可见性检查。
- 覆盖 Node/Relationship scan、Expand、OPTIONAL、variable/shortest path、子查询、`count()` / aggregation / `DISTINCT` / pagination，以及 streaming 与 `EXPLAIN` / `PROFILE`。
- 用多 Label、混合 Label、非候选 Label 大集合 fixture 验证不漏结果、不重复计数，也不因 OR 条件把已有可用 seek 变为无条件全图扫描；有界 memory / TEMP 工作状态遵守现有资源合同。

### Feature 20.3 Search 的范围内排名与缓存隔离

- 覆盖 Full-text `queryNodes` / `queryRelationships`，Raw Vector `SEARCH`（含其结构化过滤），Managed Semantic `queryNodes` / `queryRelationships`；所有通道在分页 / top-k 前判定三组 selector 与 Relationship 端点可见性。
- 构造“全局 Top-K 被其它 Label 占满、OR 范围内仍有有效候选”的 fixture，验证在指定范围内能返回目标候选，而不是全局 Top-K 后过滤；按原有 ANN 召回及评分合同验收，不制造超出原有保证的新精确性承诺。
- 验证 Semantic Provider 只对当前 view 允许的 source text 发起必要调用；跨不同 OR selector、相同 selector 的不同历史 Snapshot、staged revision 及 warm/cold TEMP/HNSW 路径不能复用错误的 owner 候选或泄漏范围外数据。

### Feature 20.4 写入、历史与 SQL 集成

- 在 CREATE / MERGE / SET / REMOVE / DELETE / DETACH DELETE 的原有 clause boundary 验证 OR 范围：合法新建及 Label 变更可被后续 clause 观察；跨 view 修改或删除回滚为既有 `GRAPH_VIEW_VIOLATION`，不改变 canonical constraint 的全图检查。
- 覆盖 branch 当前图、`options.at` 的 pinned historical Commit、staged graph state 与 SQL explicit transaction；跨版本 Label membership、Relationship 两端的可见性及 transaction rollback 一致。
- 通过真实 SQLite extension 的 `lithograph()` 与 `lithograph_rows()` 进行相同结果与错误回归；不增加第二个 application-facing API，也不改变 `readAccess` / `writeAccess` 的既有交集语义。

### Feature 20.5 回归、Review 与使用文档

- 增补 Core targeted unit/integration fixtures、SQL real-load / rows integration、Search/Provider smoke 与适用的 History / Mutation 回归，使用合成数据且不写入用户真实数据库。
- 实现后更新 [Graph View 使用指南](../../guide/graph-views.md)、[Execution Options Reference](../../reference/execution-options.md) 和需要的可执行示例；在实现之前不把尚未交付的参数写进 v0.3.3 用户参考文档。
- 按仓库规范执行定向测试、`cargo make quality`、适用的 `scripts/ci.sh` 真实 SQLite / Provider / Cypher gate 与最终 diff review，并核对 [兼容清单](../cypher25-compatibility.md) 的冻结覆盖状态是否保持不变。

## 3. Acceptance

- [x] 无 `graphView`、`{}`、三个数组都为空与完整图读写表现一致；旧 `requireAllLabels` / `excludeAnyLabels` 正反例回归无变化。
- [x] `requireAnyLabels: ["A", "B"]` 返回 A 或 B 节点；同时拥有 A 和 B 的 Node 不被重复；与 AND/NOT 组合后严格符合设计合同，OR/exclude 重叠可执行且被排除的候选不可见，全部 OR 候选都被排除时返回空结果而非配置错误。
- [x] 重复项、未知 Label、大小写/Unicode 精确名称、空数组、显式 `null`、错误类型、未知成员和 requireAll/exclude 冲突均有正反/错误码验证；读取不创建 Label dictionary 条目。
- [x] OR-only 是非空 Graph View，Schema / Index / Version operation 继续按既有 `INVALID_ARGUMENT` 规则拒绝，不因为新增 selector 漏过既有边界；空 OR 数组仍等同未设置该条件。
- [x] Node/Relationship scan、索引和 direct lookup、路径/子查询/聚合、`EXPLAIN` / `PROFILE` 与 rows streaming 在 OR 范围内正确；非候选节点不影响 `count`、`DISTINCT` 或分页；选择性 OR 查询有物理访问证据，不凭返回行数假装避免全图扫描。
- [x] Full-text、Raw Vector、Managed Semantic 的 Node / Relationship 搜索使用范围内 top-k / skip / limit；可构造全局 Top-K 后过滤的反例证明修复，不改变既有评分/ANN 召回定义。
- [x] Managed Semantic 的 Provider 输入、TEMP/HNSW owner 候选、跨 view 与历史版本的缓存边界通过真实测试，非可见 source 不触发不必要外部调用。
- [x] 合法 staged Label/CREATE/MERGE 与非法跨 view SET/REMOVE/DELETE/DETACH 按原有 clause、constraint、rollback 语义执行；Historical Commit 查询不借用当前 Branch 的 Label membership。
- [x] `lithograph()` / `lithograph_rows()` / SQL explicit transaction 与既有访问控制组合回归通过；没有 query rewrite、跨 view 绕过或对存储格式、Commit identity、Cypher Profile、Provider SPI 的改动。
- [x] 实现、定向与 real-load / Provider 测试、`cargo make quality`、适用 `scripts/ci.sh`、文档与链接校验、Phase-level review / 最终 diff 全部有本次实际通过的证据后，才将 Phase 标记 `done`。

## 4. Review 重点与状态边界

审查 selector 的组合真值表、unknown Label 与 staged-state 生命周期；`scan_label` 单 Label 快捷路径是否在 OR-only 时漏掉其它 Label；集合去重是否改变同一 Node 的 Cypher 多行结果；Search 是否先全局 Top-K 再过滤；Semantic cache / Provider 是否泄漏不同 view 或 revision 的候选；写入/历史是否绕过 Node 和 Relationship 端点 visibility。

本轮审查覆盖 selector 真值表、未知及历史 Label、staged visibility、OR 扫描去重和内存计账、搜索范围内 Top-K、Provider 缓存与失败回滚；没有剩余的本 Phase correctness、compatibility、resource 或安全 finding。已完成 Phase 04–19 的历史验收记录保持不变。

## 5. 本机实现与验收证据

- **实现：** `query/options.rs`、`query/graph.rs`、stream MATCH 及 completeness path 共用 Graph View 的多 Label OR 语义和有界扫描；新增 `crates/lithograph-core/tests/phase20_graph_view.rs`，扩展 `lithograph-phase13` 真实 SQLite / synthetic Provider 测试。
- **专项回归：** Phase 20 Core **11/11**，原有 Phase 04 **30/30**、Phase 05 **71/71**；Node / Relationship 范围内 Top-K、OR/AND/NOT、未知 Label、historical/staged 查询、写入拒绝回滚、Provider 输入和 SQL 双入口均有实际测试。
- **完整 quality：** `cargo make quality` **exit 0**；行覆盖率 **88.14%**，适用 TCK **3,777/3,777** 通过。日志：`target/phase20-check/quality-python314.log`。
- **本机 CI：** `sh scripts/ci.sh` **exit 0**；覆盖 SQLite 3.45 / 3.51 / 3.53.4 真实扩展加载、Provider SPI、SQL / 资源 / 事务、文档可执行示例与兼容清单。日志：`target/phase20-check/ci-after-reboot.log`。
- **门禁环境与安全：** 使用 Homebrew Python 3.14（内置 SQLite 3.51）替代测试环境中自带旧 SQLite 3.41 的 Conda Python；另以最小 `Cargo.lock` 补丁升级 `rustls-webpki` 到 `0.103.16`、`webpki-roots` 到 `1.0.10`，`cargo deny` 通过。未放宽验收阈值。

`done` 仅代表**本地源码实现与验收**。本轮没有 commit、push、远端 CI、Release Matrix 或正式发布；已发布的 v0.3.3 二进制不包含 `requireAnyLabels`。日志和编译制品位于 Git 忽略的 `target/`，不是发行物。
