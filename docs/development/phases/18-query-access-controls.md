# Phase 18：查询、返回与写入访问控制

**状态：`done`（本机实施与验收完成；v0.3.2 发布交付进行中）**

**审计重新打开与闭环（2026-10-18）：** 前轮 quality/CI 的 green 结果保留为当时工作树的真实证据；后续审计复现两个漏测问题：signed-zero 场景中 Squash 可能改变受保护属性，含 NaN 的无关 Revert 可能错误失败。因此前轮 done 结论已撤回，受影响的版本重组、canonical state reconstruction 与组合验收补齐后已闭环；新接口仍为 unreleased。访问控制、canonical/插值修复、永久回归、独立 review、最终 quality/CI 和当时的本机候选包验收均已完成，并曾恢复为 done。

**用户追加 scope（2026-10-18）：** 用户随后明确要求修复 OpenAI-compatible Provider 可选 cache 的首次并发初始化竞争、补入永久回归并持续 review，之后重新准备 v0.3.2。这项有界追加工作继续记在本 Phase，不新建 Phase，也不回写 Phase 15 的历史完成记录。Provider 修复、真实多进程回归、独立 review、最终完整 quality/CI 与新候选包验收均已通过，因此 Phase 恢复 done。前述 112 项专项、完整 quality/CI 和旧候选包的 green 结果保留为追加前证据；追加完成结论使用 `target/cache-init-032/` 的新日志与新包。用户现已授权提交、推送、远端 CI/六平台验收通过后正式发布 v0.3.2，当前进度见本文末尾 [发布交付记录](#v032-release-delivery)。


## 目标、范围与输入

在现有 SQL execution surface 中交付 query-local `readAccess` / `writeAccess`，覆盖查询计算、公开结果、普通图写入、Schema、完整版本操作与维护入口。正式字段/错误合同由 [Interfaces](../../design/interfaces.md#query-access-controls) 拥有，执行语义由 [Query Engine](../../design/query-engine.md#query-access-controls) 拥有，宿主边界见 [Runtime](../../design/runtime.md#security-boundary)。本计划仅维护实现顺序、入口清单、验收与真实证据，不形成第二份设计合同。

依赖已完成的 [Phase 15](15-sql-execution-provider-cache.md)、[Phase 16](16-safe-execution-option.md) 与 [Phase 17](17-developer-documentation-release.md)。保持 `CY25-2026.08`、storage format 3、既有 Commit identity 与 `EmbeddingProviderV1`。只修改 Lithograph；不新增 parser bridge、账号系统、application query ABI。本 Phase 原本机实施阶段不包含提交、推送、发布和下游集成；后续发布交付按新增授权单独记录，v0.3.1 tag/资产保持不动。

追加修复沿用 [Provider-owned cache 合同](../../design/vector.md#cache-policy-and-maintenance) 的 SQLite 并发仲裁、有界锁等待、取消与失败语义，仅修正空 cache 初始化窗口，不增加配置 key、ABI、cache schema version 或新的 storage abstraction。

## 工作与验收矩阵

| ID | 交付与验收 | 状态 |
| --- | --- | --- |
| AC18-01 | 正式设计、Phase 入口/验收清单、路线与 Runtime 边界保存；默认/空配置、严格配置类型、未知 key、非法 regex、空项的正反用例 | 已通过 |
| AC18-02 | PreparedQuery 共享规范化策略/编译 matcher；完整 AST（含嵌套和插值）检查函数及 capability 禁令；无全局策略状态 | 已通过 |
| AC18-03 | 静态/动态属性、pattern/过滤/计算/聚合/子查询、keys/properties/Map projection、Node/Relationship/Path 的 query masking；普通 Map/scalar 不误伤 | 已通过 |
| AC18-04 | Planner/index seek/ordering 不绕过 query masking；依赖隐藏属性的 Search 明确拒绝；未受限索引路径与历史 Snapshot 正常 | 已通过 |
| AC18-05 | CREATE/INSERT/MERGE、SET/REMOVE/null、动态 key、Map 合并/替换与 no-op 显式写入检查；删除清理区别于属性修改；拒绝无部分写入/Commit | 已通过 |
| AC18-06 | 下表全部 38 个 Procedure 的适用允许/拒绝与副作用证据；所有版本/Schema/维护入口按真实效果分类 | 已通过 |
| AC18-07 | Patch/Reset/Revert/Merge fast-forward/conflict/finalize、Rebase/Squash 保护；历史重组最终字段不变成功，实际变化拒绝；finalize 当前策略重验 | 已通过 |
| AC18-08 | scalar/rows 共用对外编码递归 result 过滤；显式 scalar/properties/Map projection 与普通 Map 保持值；内部排序/spill/DISTINCT 不变 | 已通过 |
| AC18-09 | explicit transaction 每次策略、staged 基线、caller-owned transaction、独立/并发 batch、ON ERROR、取消与提前关闭；失败保持既有 rollback/已提交 batch 语义 | 已通过 |
| AC18-10 | 文件/HTTP/Provider/cache-hit/tokenizer 禁令在调用前失败；零外部调用证据；旧只读/Snapshot/其它规则始终取交集 | 已通过 |
| AC18-11 | `ACCESS_DENIED`/`SQLITE_AUTH`、配置错误、不可见 null、错误不泄漏保护值；完整性/Schema 使用真实 canonical state | 已通过 |
| AC18-12 | Guide/Reference/事务/错误/可执行示例与兼容记录同步；标明 unreleased；不增加语言 coverage 分母 | 已通过 |
| AC18-13 | 定向回归、format/Clippy/Core/Extension、quality/CI、真实 SQLite SQL load、Provider 双扩展、适用 TCK/文档门禁全部通过 | 已通过 |
| AC18-14 | Phase 级 review findings 闭环、受影响重验、最终 diff/链接/空白检查；真实状态同步 | 已通过 |

## 全部注册 Procedure 入口清单

以下清单来自当前 `query/registry.rs` 的 38 项注册入口。每项必须有既有允许行为的正向回归和对应新增规则的拒绝/无副作用证据；复合权限不能只通过 named Procedure 总开关测试替代。验收前再次核对运行时 `SHOW PROCEDURES`，新增/删除项必须同步。

| Procedure | 适用效果/边界 | 必须覆盖 | 正向/专项证据 | 状态 |
| --- | --- | --- | --- | --- |
| `db.labels` | query 可见图；named Procedure | 正向枚举、named 禁止 | DOC18 `proc-graph-labels`；R18；PROC18 | 已通过 |
| `db.propertyKeys` | query 可见属性；named Procedure | 隐藏属性不出现在枚举、named 禁止 | DOC18 `proc-graph-propertykeys`；R18.enumeration；PROC18 | 已通过 |
| `db.relationshipTypes` | query 可见图；named Procedure | 正向枚举、named 禁止 | DOC18 `proc-graph-relationshiptypes`；R18；PROC18 | 已通过 |
| `db.index.fulltext.queryNodes` | query 属性依赖；external I/O；named Procedure | 正常检索、隐藏 source/禁止 tokenizer 拒绝且零调用 | DOC18 `proc-ft-phrase`；IDX18.fulltext；IO18；PROC18 | 已通过 |
| `db.index.fulltext.queryRelationships` | query 属性依赖；external I/O；named Procedure | 正常检索、隐藏 source/禁止 tokenizer 拒绝且零调用 | DOC18 `proc-ft-rel-query`；IDX18.fulltext；PROC18 | 已通过 |
| `db.index.semantic.createNodeIndex` | Schema；external I/O；named Procedure | 正常创建、Schema/Provider 拒绝且无新定义/Commit | DOC18 `proc-sem-node-create`；EXT18.semantic_args；IO18；PROC18 | 已通过 |
| `db.index.semantic.createRelationshipIndex` | Schema；external I/O；named Procedure | 正常创建、Schema/Provider 拒绝且无新定义/Commit | DOC18 `proc-sem-rel-create`；IO18；PROC18 | 已通过 |
| `db.index.semantic.queryNodes` | query 属性依赖；external I/O；named Procedure | 正常检索、query/Provider 拒绝且零调用 | DOC18 `proc-sem-node-query`；IO18；CACHE18；PROC18 | 已通过 |
| `db.index.semantic.queryRelationships` | query 属性依赖；external I/O；named Procedure | 正常检索、query/Provider 拒绝且零调用 | DOC18 `proc-sem-rel-query`；IO18；PROC18 | 已通过 |
| `db.index.semantic.rebuild` | maintenance；external I/O；named Procedure | 正常重建、maintenance/Provider 拒绝且零调用 | DOC18 `proc-sem-rebuild`；IO18；PROC18 | 已通过 |
| `lithograph.branch.create` | version；named Procedure | 允许创建 ref、version 拒绝无 ref | DOC18 `vp-branch-create`；VR18.references/version_boolean；PROC18 | 已通过 |
| `lithograph.branch.checkout` | version/connection state；named Procedure | 允许 checkout、version 拒绝 checkout 不变 | DOC18 `vp-branch-checkout`；VR18.references/version_boolean；PROC18 | 已通过 |
| `lithograph.branch.list` | metadata read；named Procedure | 允许读取、named 拒绝 | DOC18 `vp-branch-list`；O18.independent；VR18.version_boolean；PROC18 | 已通过 |
| `lithograph.branch.delete` | version；named Procedure | 允许删除 ref、version 拒绝 ref 保留 | DOC18 `vp-branch-delete`；VR18.references/version_boolean；PROC18 | 已通过 |
| `lithograph.commit.get` | metadata read；named Procedure | 允许读取，普通 metadata Map 不被属性名误伤；named 拒绝 | DOC18 `vp-commit-get`；VR18.references；PROC18 | 已通过 |
| `lithograph.commit.create` | version；named Procedure | 允许 empty Commit、version 拒绝无新 Commit | DOC18 `vp-commit-create`；VR18.references/version_boolean；PROC18 | 已通过 |
| `lithograph.commit.data.set` | version metadata；named Procedure | 允许 sidecar 修改、version 拒绝且 metadata 不变 | DOC18 `vp-data-set`；VR18.references/version_boolean；PROC18 | 已通过 |
| `lithograph.commit.data.clear` | version metadata；named Procedure | 允许 sidecar 清理、version 拒绝且 metadata 不变 | DOC18 `vp-data-clear`；VR18.references/version_boolean；PROC18 | 已通过 |
| `lithograph.tag.create` | version；named Procedure | 允许创建 Tag、version 拒绝无 ref | DOC18 `vp-tag-create`；VR18.references/version_boolean；PROC18 | 已通过 |
| `lithograph.tag.list` | metadata read；named Procedure | 允许读取、named 拒绝 | DOC18 `vp-tag-list`；VR18.version_boolean；PROC18 | 已通过 |
| `lithograph.tag.move` | version；named Procedure | 允许移动、version 拒绝 ref 保持 | DOC18 `vp-tag-move`；VR18.references/version_boolean；PROC18 | 已通过 |
| `lithograph.tag.delete` | version；named Procedure | 允许删除、version 拒绝 ref 保留 | DOC18 `vp-tag-delete`；VR18.references/version_boolean；PROC18 | 已通过 |
| `lithograph.log` | metadata read；named Procedure | 允许历史分页、named 拒绝 | DOC18 `vp-log`；DOC18；PROC18 | 已通过 |
| `lithograph.diff` | 完整版本资料的 query 属性检查；named Procedure | 未涉及隐藏属性成功、含隐藏属性拒绝且不伪造 Patch | DOC18 `vp-diff`；VP18.opaque_diff；PROC18 | 已通过 |
| `lithograph.patch.apply` | version；有效 graph/Schema 变化；named Procedure | 允许非受限 Patch、保护属性/创建/删除/Label/Schema 拒绝原子回滚 | DOC18 `vp-patch-graph`；VP18 全组；IO18.Schema restore；PROC18 | 已通过 |
| `lithograph.merge.start` | version Session；named Procedure | 允许建立 Session、version 拒绝无残留 Session | DOC18 `mp-start`；VR18.version_boolean；VM18；PROC18 | 已通过 |
| `lithograph.merge.get` | Session metadata read；named Procedure | 允许状态/计数/引用，query 属性规则不误伤 metadata；named 拒绝 | DOC18 `mp-get`；VM18.revision/session-preservation；PROC18 | 已通过 |
| `lithograph.merge.list` | metadata read；named Procedure | 允许 Session 列表、named 拒绝 | DOC18 `mp-list`；DOC18；PROC18 | 已通过 |
| `lithograph.merge.conflicts` | 完整冲突资料的 query 属性检查；named Procedure | 可见冲突成功、含隐藏属性拒绝而不伪造冲突 | DOC18 `mp-conflicts`；VM18.conflict_records；PROC18 | 已通过 |
| `lithograph.merge.resolve` | version；显式 candidate 属性修改；named Procedure | 允许非受限 resolution、受保护显式修改拒绝且 revision/candidate 不变 | DOC18 `mp-resolve-value`；VM18.resolution/session_permission；PROC18 | 已通过 |
| `lithograph.merge.finalize` | version；有效 graph/Schema 变化；named Procedure | fast-forward/冲突后 finalize 正向；按本次权限重验、拒绝 Session 保留 | DOC18 `mp-finalize`；VM18.fast_forward/ready_merge/effects；PROC18 | 已通过 |
| `lithograph.merge.abort` | version Session；named Procedure | 允许清理 Session、version 拒绝 Session 保留 | DOC18 `mp-abort`；VM18.session_permission；PROC18 | 已通过 |
| `lithograph.rebase` | version；最终有效 graph/Schema 变化；named Procedure | 最终保护属性未变成功、真实改变拒绝且无中间 Commit/ref move | DOC18 `vp-rebase`；VH18.rebase 全组；PROC18 | 已通过 |
| `lithograph.squash` | version；最终有效 graph/Schema 变化；named Procedure | 重放曾触及保护属性但最终相同成功；其它显式禁令仍生效 | DOC18 `vp-squash`；VH18.squash；DOC18；PROC18 | 已通过 |
| `lithograph.reset` | version；最终有效 graph/Schema 变化；named Procedure | 允许未受限目标、受限字段真实变化拒绝 Branch 不变 | DOC18 `vp-reset`；VH18.reset 全组；IO18；PROC18 | 已通过 |
| `lithograph.revert` | version；最终有效 graph/Schema 变化；named Procedure | 允许未受限反向变化、受限字段真实变化拒绝且无新 Commit | DOC18 `vp-revert`；VH18.reset_and_revert；PROC18 | 已通过 |
| `lithograph.index.rebuild` | Standard Index maintenance；named Procedure | 正常重建、maintenance 禁令拒绝；保持不支持 FTS/Vector/Semantic 的既有错误 | DOC18 `proc-maint-rebuild`；VR18.maintenance；IO18；PROC18 | 已通过 |
| `lithograph.gc` | maintenance；named Procedure | 正常清理不可达历史、maintenance 拒绝无清理 | DOC18 `proc-maint-gc`；VR18.maintenance；PROC18 | 已通过 |


## 非 Procedure 写入与执行入口

| 入口 | 验收范围 | 状态 |
| --- | --- | --- |
| `CREATE` / `INSERT` | Node/Relationship 实际创建、初始属性、Label 与 permission 交集；零行无效果允许 | M18.initial/creation/labels；MR18.new_properties；已通过 |
| `MERGE` | 匹配/创建分支、ON MATCH/ON CREATE、动态与嵌套写入；allowCreate=false 时仅匹配成功 | M18.merge/creation；MR18.merge；已通过 |
| `SET` / `REMOVE` / `FOREACH` | 静态/动态 key、null、相同值、Map +=、Map 整组替换隐含删除、Label 变化 | M18.node/relationship/property_rules；MR18.maps；B18；已通过 |
| `DELETE` / `DETACH DELETE` | 删除权限与关系完整性；允许删除不被保护属性清理误伤 | M18.deletion；VP18.entity_deletion；已通过 |
| Graph Type / Constraint / Index DDL | Schema 权限，Provider/tokenizer 外部调用交集，失败无定义/Commit | M18.schema/constraints；EXT18；IO18；已通过 |
| `SHOW` / `EXPLAIN` / `PROFILE` / 函数调用 | 全 AST capability 与 denyFunctions，嵌套/未执行路径/插值 | O18.capabilities/keyword_functions；IDX18.plan；已通过 |
| `LOAD CSV` | 参数/字面量 URI、文件/HTTP、零行分支、嵌套与 batch 的调用前拒绝 | O18.capabilities；B18；DOC18；已通过 |
| `lithograph()` / `lithograph_rows()` | 同一规则和错误；编码/length/resource 失败在既有成功边界前 | SQL18；ERR18；DOC18；已通过 |
| SQL explicit staged execution | 新选项转发；每次 staged 基线；任一 execution 拒绝整体 abort | SQL18.explicit_policy/explicit_denial；ERR18.forwarding；已通过 |
| `IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` | 同策略传递、独立批次/ON ERROR、已 durable batch 不倒退 | B18 全组；DOC18.streaming；已通过 |
| 初始化、原始 SQLite SQL、数据库文件、`tx_begin/commit/abort` | 保留可信宿主边界，不增加新 policy 入口 | SQL18.caller_owned；ERR18.forwarding；DOC18.lifecycle；已通过 |


## 审计修复与漏测补齐验收

本轮复验补足原 AC18-06/07/11/13/14 的证据，不修改用户已确认权限行为，不把旧 green 当作缺失场景已通过。已核对 [Storage canonical encoding](../../design/storage.md#canonical-value-encoding)、[Diff/Patch 与 logical-slot](../../design/versioning.md#diff-and-patch)、[Squash 完整 Snapshot 保持](../../design/versioning.md#squash) 的既有合同；此次只补稳定锚点与跨域引用，不新建数值/存储语义。精确测试名、数量与最终门禁在实际执行后追加。

| ID | 本轮修复/补验范围 | 必须证明的结果 | 状态 |
| --- | --- | --- | --- |
| AC18-R01 | signed-zero Squash；canonical reconstruction | Squash 保留真实 +0/-0 canonical 属性状态；受保护状态发生真实变化时拒绝；失败无 Commit/ref 副作用 | 已通过：Core 版本 41/41、公开 SQL 与包内 canonical 5/5 |
| AC18-R02 | NaN 无关 Revert；Patch applicability | 原 canonical NaN 与自身匹配；无关字段 Revert 正常执行，保护属性保持原 canonical 值，不因 Cypher/宿主浮点比较误报冲突 | 已通过：Core 版本 41/41、公开 SQL 与包内 canonical 5/5 |
| AC18-R03 | Node/Relationship、组合值及 Schema 的 canonical state/delta | canonical reconstruction、net Layer、Diff、Patch before/after、Merge candidate/resolution、Rebase/Squash/Revert 使用同一既有状态含义；涵盖 signed zero、NaN 及相邻 property value 类型，不只证明 formatter/JSON 相同 | 已通过：canonical 属性/Schema、相关 147 项、最终 debug/release 112 项与包内验收 |
| AC18-R04 | 版本操作权限交集与回滚 | Diff/Patch/Merge fast-forward/ready/conflict/finalize/Rebase/Squash/Reset/Revert 的允许/拒绝路径；当前策略复验，保护字段最终状态不变成功，真实变化拒绝；Session/revision/Branch/Schema 正确 | 已通过：effects/canonical/Schema、SQL/C 双 surface、完整门禁与包内验收 |
| AC18-R05 | scalar/rows、explicit staged、独立/并发 batch 与取消 | query/result/write 策略一致，过滤不改内部排序/去重；每次 staged 基线、失败自动 abort、ON ERROR 与 earlier durable batches 保持原语义 | 已通过：真实库及包内 SQL 28/28、batch 7/7、完整门禁 |
| AC18-R06 | Graph View、索引/Search 与外联组合 | 受限属性不经 index/Search/graph procedures 绕过；未受限查询继续正常；Graph View 与写入交集、Provider/tokenizer/file/HTTP 调用前拒绝及 warm-cache 行为完整 | 已通过：真实库及包内 CSV 28/28、外联 22/22、cache 8 checks、完整门禁 |
| AC18-R07 | 全部新增/遗漏用例与独立 review loop | 新发现先复现、修复、定向重验、再次 review；所有 scope 内 finding 闭环后运行最终 full quality/CI、TCK、真实 SQLite/双扩展/文档门禁；检查最终 diff | 已通过：独立 canonical/runtime/test/docs review 无 open finding，最终 quality/CI 与 diff 检查闭环 |
| AC18-R08 | 发布准备材料 | 用户指定 v0.3.2；准备具体可审阅的发布说明/检查结果，不把准备等同于已提交/推送/tag/上传/发布 | 追加修复前的本机发布准备已完成；新 Provider 对应的候选包与验收按下表重做，未发布 |

## 追加 Provider cache 初始化修复验收

本节只维护追加工作的实施与证据。缓存身份、事务、失败和数据边界继续由 [Vector](../../design/vector.md#cache-policy-and-maintenance) 拥有；已完成的 AC18 / AC18-R 结果保留，不把 warm-cache 权限验证写成空 cache 首次初始化验证。

| ID | 追加范围 | 必须证明的结果 | 当前状态 |
| --- | --- | --- | --- |
| AC18-C01 | 空 cache 首次并发初始化修复 | 多个真实进程竞争同一未初始化 cache 时，初始化在 writer 仲裁后重新确认状态；全部有效请求成功，只有一份合法 Schema，向量与 accounting 正确 | 已通过 debug 真实多进程 missing/empty 两组，各 4 进程 |
| AC18-C02 | 永久回归与旧红→新绿 | 使用真实 Provider/宿主 SQLite 与同一 empty-schema barrier 机制，修复前复现竞争、修复后通过；覆盖初始化后 reopen/hit，并保留原 cache checks | 已通过：before.log 旧红、after-debug.log 新 15 checks |
| AC18-C03 | 失败、取消与配置边界 | 有界 BUSY/LOCKED 等待与取消仍生效；main identity、非 Provider DB、marker/schema 不匹配、disabled 无文件副作用仍按既有合同失败或成功，不静默绕过 enabled cache | 已通过：原 cache 回归、新初始化取消/超时/foreign races、24 项 Provider unit 与独立 review |
| AC18-C04 | 追加 scope review 与最终源码门禁 | Provider/cache/test/gate 独立 review findings 闭环；最终源码的 quality、CI、适用 SQLite/Provider 双扩展与文档门禁通过 | 已通过：最终 quality/CI 均 exit 0，112 项 Core、3-runtime 各 15 cache checks；独立 review 无剩余 finding |
| AC18-C05 | 新候选包与版本适用文档 | 从包含修复的最终源码重新打包，核对新校验和、包内真实版本、首次并发初始化和完整 release smoke；保留 v0.3.1 历史限制及适用规避 | 已通过：新包校验和/真实 0.3.2、完整 smoke、canonical SQL 5/5 与 integrity；v0.3.1 规避保留 |

追加证据统一使用本机 `target/cache-init-032/`。旧 `target/phase18-recheck/` 的日志与 SHA-256 `a615b5d7a2ff5aef8a6c1f47e8be2f736d732c8d8525e300ae069fcf5f15e649` 只证明追加前的候选包；它未包含此次 Provider 修复，不能继续作为新候选的验收证据。

## 专项证据索引

下表的短名只用于将上面每项验收定位到实际测试，不是新的产品接口。DOC18 的 example ID 来自 Markdown 的 `<!-- example: ... -->` 与 [执行清单](../../guide/examples/cypher/)，已在当前开发扩展的 scalar/rows 两入口逐项运行；因此正向能力不以 effects classification unit test 代替。PROC18 的统一注册入口拒绝测试是额外覆盖，已通过 38 个 named 禁令与 30 次独立 effects 禁令；不把分类 unit 当作实际正向执行。

| 证据 | 测试来源与精确用例 |
| --- | --- |
| O18 | [phase18_access_options.rs](../../../crates/lithograph-core/tests/phase18_access_options.rs)：`omitted_empty_and_explicit_true_access_preserve_defaults`、`property_rules_are_exact_literal_union_and_full_regex_matches`、`invalid_access_configuration_is_an_argument_error`、`capabilities_are_checked_on_the_complete_ast_before_execution`、`operation_permissions_do_not_block_other_read_or_write_capabilities`、`graph_filter_recurses_without_matching_map_keys_or_structure`、`keyword_function_syntax_and_interpolation_do_not_bypass_function_denials`、`qualified_and_special_function_forms_are_denied_after_valid_baselines`、`nested_function_denials_cover_union_subqueries_and_list_expressions`、`keyword_functions_keep_their_identity_with_nested_function_arguments`、`simultaneous_prepared_queries_keep_separate_access_policies`；加下列 PROC18 后当前共 12 项 |
| PROC18 | 同一 O18 文件的 `every_registered_procedure_checks_named_and_effect_permissions_before_invocation`：从真实 SHOW PROCEDURES 取得 38 名，分别验证 named 禁令，以及各自 external-I/O/Schema/version/maintenance 固定 effects；前轮 options 9/9 通过，审计补齐后的 12/12 已在最终 debug/coverage/release 三组通过。另有 [registry effects unit](../../../crates/lithograph-core/src/query/registry/effects.rs) `every_registered_procedure_has_an_explicit_effect_classification`，只作为分类完整性证据 |
| R18 | [phase18_interpreter_access.rs](../../../crates/lithograph-core/tests/phase18_interpreter_access.rs)：`interpreter_uses_hidden_values_for_grouping_subqueries_and_interpolation`、`path_predicates_and_grouped_relationships_never_observe_hidden_properties`、`graph_property_enumeration_is_filtered_but_ordinary_maps_keep_their_keys`、`search_rejects_hidden_sources_and_only_filter_properties_it_uses`、`endpoint_functions_and_projection_barriers_keep_query_objects_restricted`、`simple_stream_projection_and_count_fast_paths_apply_the_same_rules`、`optional_hidden_relationship_predicates_preserve_the_left_row_and_null_extend`、`nested_graph_aliases_cannot_restore_hidden_node_relationship_or_path_properties`；当前 8 项 |
| IDX18 | [phase18_index_access.rs](../../../crates/lithograph-core/tests/phase18_index_access.rs)：`standard_indexes_skip_hidden_predicates_and_preserve_public_seeks`、`composite_range_and_text_indexes_mask_parameter_predicates_for_nodes_and_relationships`、`point_indexes_cannot_match_hidden_bounding_box_or_distance_sources`、`hiding_only_the_second_composite_column_preserves_a_public_single_property_seek`、`vector_search_rejects_hidden_sources_before_cache_or_zero_limit`、`fulltext_hidden_source_rejection_is_identical_for_nodes_relationships_and_history`；当前 6 项 |
| M18 | [phase18_mutation_access.rs](../../../crates/lithograph-core/tests/phase18_mutation_access.rs)：`omitted_empty_and_explicitly_allowed_rules_preserve_writes`、`write_property_rules_use_union_literal_prefixes_and_full_name_patterns`、`protected_node_properties_reject_explicit_writes_before_no_op_shortcuts`、`protected_relationship_properties_have_the_same_write_boundary`、`forbidden_initial_properties_rollback_nodes_relationships_and_prior_writes`、`merge_checks_only_the_executed_property_mutations`、`creation_permission_does_not_restrict_existing_element_updates_or_merge_matches`、`deletion_permission_and_property_protection_are_independent`、`label_permission_includes_initial_labels_but_allows_unlabeled_creation`、`schema_permission_is_independent_of_property_and_version_permissions`、`query_masking_does_not_weaken_canonical_constraint_validation`、`permission_allowances_cannot_override_property_or_existing_read_only_guards` |
| MR18 | [phase18_mutation_reads.rs](../../../crates/lithograph-core/tests/phase18_mutation_reads.rs)：`write_return_masks_explicit_values_enumeration_maps_and_graph_objects`、`write_barriers_and_count_use_masked_values`、`property_map_mutations_copy_only_query_visible_properties`、`merge_does_not_match_hidden_properties_but_still_validates_canonical_constraints`、`new_properties_and_paths_are_masked_without_changing_the_written_state`、`merge_null_patterns_still_reject_before_matching_existing_missing_properties` |
| B18 | [phase18_batch_access.rs](../../../crates/lithograph-core/tests/phase18_batch_access.rs)：`sequential_and_concurrent_batches_share_dynamic_write_rules_and_continue_status`、`rejected_batch_rolls_back_every_row_in_that_batch`、`fail_and_break_keep_prior_durable_batches_after_access_refusal`、`sequential_and_concurrent_read_batches_mask_properties_and_outer_conditions`、`batch_write_rhs_uses_masked_values_while_canonical_sources_remain_intact`、`denied_outer_suffix_does_not_undo_completed_inner_batch`、`cancellation_keeps_only_the_already_published_permitted_batch` |
| VP18 | [version Patch tests](../../../crates/lithograph-core/tests/phase18_version_access/patch.rs)：`patch_is_not_blanket_denied_and_protected_changes_rollback_before_publishing`、`patch_explicit_same_value_property_write_is_denied_even_without_effective_delta`、`opaque_diff_rejects_hidden_query_properties_but_result_rules_leave_patch_complete`、`authorized_patch_entity_deletion_does_not_treat_cleanup_as_protected_property_writes` |
| VH18 | [version History tests](../../../crates/lithograph-core/tests/phase18_version_access/history.rs)：`reset_and_revert_check_real_target_changes_and_leave_history_intact_on_denial`、`squash_allows_protected_history_replay_when_final_effective_state_is_identical`、`rebase_allows_replaying_old_protected_writes_when_final_value_does_not_change`、`rebase_rolls_back_replayed_commits_when_final_protected_value_would_change`、`reset_and_revert_intersect_create_delete_label_and_schema_permissions`、`rebase_cannot_return_conflict_values_of_query_hidden_properties`、`reset_compares_protected_nan_by_canonical_value_instead_of_float_equality`、`reset_rejects_protected_signed_zero_change_despite_numeric_equality` |
| VM18 | [version Merge tests](../../../crates/lithograph-core/tests/phase18_version_access/merge.rs)：`fast_forward_finalize_rechecks_this_invocations_policy_and_preserves_session_on_denial`、`conflict_resolution_protects_explicit_same_value_requests_and_keeps_revision_on_error`、`ready_merge_finalize_rechecks_protected_effective_changes_after_earlier_resolution`、`merge_conflict_records_are_rejected_if_they_expose_query_hidden_property_values`、`merge_finalize_create_delete_label_and_schema_permissions_use_actual_graph_effects`、`session_abort_and_resolve_require_version_permission_without_consuming_session`、`relationship_property_with_label_like_name_is_not_a_label_change` |
| VR18 | [version Reference tests](../../../crates/lithograph-core/tests/phase18_version_access/references.rs)：`reference_and_commit_metadata_operations_are_not_graph_property_writes`、`version_boolean_rejects_reference_metadata_and_session_writes_without_side_effects`、`maintenance_permission_is_independent_from_version_and_graph_property_permissions` |
| EXT18 | [phase18_external_access.rs](../../../crates/lithograph-core/tests/phase18_external_access.rs)：`fulltext_schema_is_denied_before_host_tokenizer_lookup_or_construction`、`external_io_permission_does_not_override_schema_or_readonly_restrictions`、`ordinary_writes_and_standard_indexes_work_with_existing_host_backed_definitions`、`semantic_create_arguments_use_query_property_visibility` |
| VC18 | [canonical.rs](../../../crates/lithograph-core/tests/phase18_version_access/canonical.rs)：`squash_preserves_protected_signed_zero_in_all_float_bearing_property_types`、`reverse_and_cross_branch_diff_preserve_signed_zero_and_ignore_unchanged_nan`、`revert_of_public_change_succeeds_with_unchanged_nan_and_preserves_history`、`patch_before_condition_matches_canonical_nan_and_distinguishes_signed_zero`、`rebase_replays_protected_signed_zero_and_unchanged_nan_without_false_conflicts`、`sparse_and_full_merge_distinguish_signed_zero_and_auto_merge_canonical_nan`、`explicit_merge_resolution_keeps_canonical_float_and_revision_semantics`、`delete_modify_dependencies_use_canonical_node_and_relationship_property_states`、`criss_cross_virtual_base_uses_canonical_nan_and_signed_zero_states`、`signed_zero_resolution_change_invalidates_the_previously_validated_candidate_revision`；本轮定向 10/10 与最终完整门禁通过 |
| VS18 | [schema_canonical.rs](../../../crates/lithograph-core/tests/phase18_version_access/schema_canonical.rs)：`semantic_schema_diff_retains_scalar_and_nested_signed_zero_identity`、`semantic_schema_patch_before_condition_rejects_numerically_equal_wrong_definition`、`reset_and_patch_check_opaque_schema_changes_without_mutating_history_on_denial`、`ready_merge_finalize_rechecks_opaque_schema_identity_and_preserves_session_on_denial`；本轮定向 4/4 与最终完整门禁通过 |
| VE18 | [effects.rs](../../../crates/lithograph-core/tests/phase18_version_access/effects.rs)：`non_fast_forward_merge_rechecks_all_effect_permissions_without_consuming_ready_session`、`schema_only_patch_is_independent_of_graph_property_and_entity_permissions`、`rebase_replays_entity_label_and_schema_history_when_final_restricted_effects_are_unchanged`、`rebase_final_effect_denial_rolls_back_all_replayed_commits_layers_and_schema`、`squash_preserves_delete_and_schema_history_without_treating_replay_as_new_effects`；本轮定向 5/5 与最终完整门禁通过 |
| SQL18 | [phase18_access_sql.py](../../../tests/phase18_access_sql.py)：现为 14 组 × scalar/rows，共 28 组；保留原 8 组，审计新增 `streaming_policy_isolation`、`controlled_stream_early_close`、`controlled_result_length_failure`、`batch_graph_outputs`、`graph_view_access_intersection`、`interpolated_lifecycle_rejection_aborts_explicit_work`；每组 integrity.ok=true。最终 v0.3.2 debug 真实库 28/28 通过，见 `target/phase18-recheck/sql.log` |
| Q18 | [phase18_interpolation_access.rs](../../../crates/lithograph-core/tests/phase18_interpolation_access.rs)：`interpolated_expression_subqueries_execute_with_default_and_explicit_access`、`interpolated_graph_subqueries_keep_query_visibility_across_rows_and_batches`、`interpolated_aggregates_keep_the_global_group_and_lazy_graph_branches`、`lazy_graph_booleans_preserve_three_valued_logic_and_type_errors`、`scoped_graph_expressions_keep_list_variables_accumulators_and_shadowing`、`scoped_graph_expressions_preserve_hidden_properties_plain_maps_and_group_inputs`、`scoped_graph_expressions_keep_null_empty_and_type_semantics`、`graph_sort_subqueries_can_read_projected_aliases_without_replacing_the_group`、`interpolated_csv_reads_succeed_when_allowed_and_are_rejected_before_open_when_denied`；本轮最终定向 9/9 通过 |
| QC18 | [phase18_interpolation_classification.rs](../../../crates/lithograph-core/tests/phase18_interpolation_classification.rs)：`interpolation_keeps_external_io_and_maintenance_classification`、`semantic_calls_inside_interpolation_keep_transaction_owning_restriction`、`interpolation_version_procedures_keep_write_and_lifecycle_classification`、`interpolation_does_not_bypass_version_composition_restrictions`、`candidate_interpolation_preserves_maintenance_and_version_context_boundaries`；修复前 4 项 red，最终扩展为 5/5 通过 |
| SC18 | [phase18_schema_canonical.rs](../../../crates/lithograph-core/tests/phase18_schema_canonical.rs)：`semantic_schema_equivalence_preserves_nested_provider_config_signed_zero`、`identical_canonical_provider_config_still_rejects_an_equivalent_index`；Schema DDL 的 canonical definition 回归，最终 debug/coverage/release 均 2/2 通过 |
| ERR18 | [Extension unit tests](../../../crates/lithograph-extension/src/tests.rs)：`query_access_denied_preserves_category_and_sqlite_auth`、`explicit_transaction_forwards_per_execution_access_rules`、`output_encoding_filters_only_graph_properties_without_changing_runtime_values`；总 Extension suite 14 项 |
| IO18 | [真实 external smoke](../../../tests/phase18_external_access_smoke.c) 的 `check_schema_and_search`、`check_graph_and_standard_index`、`check_version_schema_restoration`；scalar/rows 的拒绝同时断言 Provider validate/embed、tokenizer xCreate/xTokenize 全为零及 Branch head 不变 |
| CACHE18 | [真实 Provider cache gate](../../../crates/lithograph-test-support/src/bin/lithograph-phase15-provider-cache.rs) 的 `check_warm_cache_access_boundary`：先 warm，再停 HTTP；Engine 离线 cache hit 成功；独占锁住 Provider cache 时两 SQL surface 均先返回 ACCESS_DENIED/SQLITE_AUTH，cache 字节不变；连同既有 cache gate 共 8 checks |
| CACHE032 | 同一 gate 的 [initialization.rs](../../../crates/lithograph-test-support/src/bin/lithograph-phase15-provider-cache/initialization.rs)、[children.rs](../../../crates/lithograph-test-support/src/bin/lithograph-phase15-provider-cache/children.rs) 与 [server.rs](../../../crates/lithograph-test-support/src/bin/lithograph-phase15-provider-cache/server.rs)：新增 missing/empty 首次多进程、foreign Schema 竞争、初始化取消/超时无 partial Schema、外部对象拒绝及持 writer 离线 warm hit，连同原 8 项共 15 checks；debug 定向、最终 CI 与新包 smoke 均通过 SQLite 3.45.0 / 3.51.0 / 3.53.4 各 15 项，日志见追加最终记录 |
| CSV18 | [phase18_load_csv_access.py](../../../tests/phase18_load_csv_access.py)：28 组 = scalar/rows × file/HTTP × literal/param/nested CALL/direct COUNT/real interpolation/zero rows/dead interpolation；每组正向真实结果与拒绝 SQLITE_AUTH，HTTP callback/head 不变。最终 v0.3.2 debug 真实库 28/28 通过，见 `target/phase18-recheck/csv.log` |
| IOV18 | [phase18_external_access_version.c](../../../tests/phase18_external_access_version.c)：20 组历史操作/host/SQL surface（16 拒绝、4 纯 fast-forward 无 callback 允许）及 2 组 opaque providerConfig canonical identity；覆盖 Diff 正负零、Reset Schema 禁令、Patch/Revert/Rebase/ready Merge 复合权限及允许 callback。最终 v0.3.2 debug 真实库 22/22 通过，见 `target/phase18-recheck/external.log` |
| ART18 | 本机 `target/phase18-recheck/canonical-artifact.log`：公开 SQL 独立运行 protected signed-zero Squash、NaN unrelated Revert、sparse Merge、full Merge、Schema ProviderConfig signed-zero 五组；每组先确认真实 extension 0.3.2、最终 full integrity.ok=true，5/5 通过。此为本机构建库回归，不是已发布制品证据 |
| DOC18 | [当前开发扩展文档实测](#当前开发扩展的文档实测2026-10-18yi-mac)：528×2 Cypher、138 SQL、5 Python 应用、C host、runtime inventory 与静态链接覆盖 |

## 实施顺序与验证

1. 保存设计与本验收矩阵，核对已有 parser、PreparedQuery、graph read、mutation、version/diff、结果编码与 transaction 路径。
2. 建立 query-local 共享上下文和严格 options/AST 检查，先证明默认配置与能力拒绝，再接 query/index/Search。
3. 并行完成普通写入、完整版本/Session/维护适配与 SQL result/transaction 传播；使用现有 Layer/Delta/diff，不建立全图副本。
4. 运行访问控制定向测试及受影响 mutation/version/index/Search 回归，补齐实际 SQL 与 external no-call 证据。
5. 更新使用文档与可执行断言，运行完整仓库门禁，Phase 级 review → 修复 → 受影响重验 → final diff review。

必须完成：`cargo fmt --all -- --check`、编译、workspace all-target/all-feature Clippy、Core/test-support 与 Extension 分组测试、`cargo make quality`、`scripts/ci.sh`。真实 SQLite `.load` 与 scalar/rows/explicit transaction、Provider 双扩展、适用 inherited TCK/冻结 fixtures、文档 coverage/link/generator/example 门禁均保留；不以 parser acceptance 或仅 Core tests 代替外层验收。没有新失败或改动时不重复无关门禁。

全部 AC18 项与入口清单通过，相关 correctness/security/storage/compatibility/performance findings 闭环，文档与最终工作树一致，才标记 `done`。省略、失败、未执行不是通过；提交/推送/发布不属于本 Phase 完成证据。

## 实施证据

以下按推进顺序保留前轮完成、审计重新打开和本轮复验过程；中间段落中的“待验证”只说明当时状态。当前完成结论以本文开头矩阵和最后的“追加 cache 修复最终完成状态”为准。

2026-10-18 开始：当前工作树在实施前为 clean；Phase 00–17 保持已完成历史状态。已保存本 Phase 的正式设计与验收基线。代码、测试与完整门禁尚待实际运行后逐项记录；本记录不作为功能已完成证据。

### 设计与文档基线（2026-10-18）

已保存 Interfaces / Query Engine 的正式合同和 Runtime 宿主边界，登记 38 个已注册 Procedure 与非 Procedure 入口。Guide 新增完整访问控制调用，Reference 补齐 options、错误与事务；所有新增能力标注 unreleased，v0.3.1 固定链接和发行资产未变。

静态检查通过：`check_docs.py` 核对 79 个公开 Markdown 页的本地链接/锚点，覆盖映射保持 50 个 capability family、151 个 required item、38 个 delta/supplemental item，新增 options 后为 50 个公开 surface。`verify_cypher.py --check` 登记 528 个 Markdown Cypher 示例，`--self-test` 通过；新增 Python 文档脚本语法检查通过，`git diff --check` 通过。以上只证明文档结构与断言登记，不证明新接口运行时已通过；真实 SQLite、完整示例与 Phase 门禁待运行。

### 当前开发扩展的文档实测（2026-10-18，Yi Mac）

使用本工作树构建的 `target/debug/liblithograph.dylib`，CLI 为 Homebrew SQLite 3.51.0（`/opt/homebrew/opt/sqlite/bin/sqlite3`），Provider fixture 为现有 `target/phase10/sqlite-3.53.4/phase13_synthetic_embedding_provider.dylib`。这是未发布开发库验收，不是 v0.3.1 发行制品新增能力或新的 Release 验收。

- `verify_cypher.py ... --sqlite3 ... --provider ...`：528 个 Markdown 示例 × scalar/rows 两入口，共 1,056 次正反场景全部通过；包含列、值、明确错误 category、stream 事件顺序与最终 summary 断言。
- `verify.py ... --sqlite3 ...`：Guide/Reference 的 138 条 SQL 同时通过 Python SQLite 与 CLI 执行，原有错误/类型/CSV/事务/integrity 边界通过；新 `access_controls.py` 的 query/result/普通 Map/regex、允许业务写入、保护字段 no-op/dynamic/replacement/delete 拒绝且 head 不变、explicit transaction 整体 abort、Squash 最终有效状态不变成功、Reset/Diff 受保护字段拒绝与 integrity 断言全部通过。
- `generate_reference.py --extension ... --sqlite3 ... --check`：172 条函数签名与 38 个 Procedure 一致；注册清单与本计划的 38 项入口核对一致。
- `python_quickstart.py`、`version_workflow.py`、`sql_transaction.py`、`streaming.py` 与新 `access_controls.py` 全部通过；C host 以 warning-as-error 重新编译应用示例并链接现有 SQLite 3.53.4 fixture，对本开发扩展的 SQL-only execution 通过。

文档静态检查与实际示例已闭合；完整 Core/Extension、quality/CI、专项副作用证据和 Phase 级 review 仍由后续实际结果决定，本 Phase 暂不标记 done。后续代码若影响这些调用路径，按受影响范围重新验证。

### 领域实现与定向验收（2026-10-18）

复现全部新专项的入口为独立 Core invocation： `cargo test --locked -p lithograph-core --features test-support --test phase18_access_options --test phase18_interpreter_access --test phase18_index_access --test phase18_mutation_access --test phase18_mutation_reads --test phase18_batch_access --test phase18_version_access --test phase18_external_access`。已通过 O18（含 PROC18）9、R18 6、IDX18 3、M18 12、MR18 6、B18 7、Version 22、EXT18 4，共 69 项。PROC18 实际覆盖 38 个注册名称的 named 禁令与 30 次独立 external-I/O/Schema/version/maintenance effects 禁令，全部在 invocation 前返回 ACCESS_DENIED/SQLITE_AUTH。`target/phase18/ci.log` 首轮 Core suite 记录原有 68 项成功；新增 PROC18 随整个 options suite 另行补验 9/9 通过，完整 CI 尚未结束。

真实 SQL18 已通过 16/16 组，Extension 已通过 14/14；IO18 的 synthetic Provider/tokenizer 真实 callback 正反对照通过。CACHE18 已通过 8 checks，包含停止 HTTP 后 Engine 从 Provider-owned warm cache 成功，以及 cache 独占锁下拒绝路径不触达 Provider/cache 的边界。Version 新用例另明确：保护属性 NaN→其它值→相同 canonical NaN 的末态允许；+0 与 -0 的 canonical state 不同，受保护 Reset 拒绝。

领域 review 的 scope 内 findings 已修复并重验，剩余完成项为最终完整 quality/CI 与其后的 diff/状态同步。本阶段仍保持 in_progress，不记录 commit、push 或 release 完成。

### 统一注册入口与 review 闭环

新增 PROC18 的 `cargo test --locked -p lithograph-core --features test-support --test phase18_access_options` 实际通过 9/9。读取真实 SHOW PROCEDURES 得到的 38 个名称逐项 named 拒绝、按真实 effects 分组的 30 次独立拒绝均通过；它与 DOC18 中逐入口实际成功的正向例结合，闭合全部注册入口的验收映射。最终 read/write 领域 review 均无未闭环 finding。

首轮 quality 的 duplicate gate 在原 1% 阈值处暴露 1.01% 重复；将本 Phase 新增的两处字符串插值递归相同逻辑合并复用后，796/79,786 行约 0.9977%，原阈值通过。此时 quality 继续执行 instrumented tests/coverage，CI 继续真实 SQLite 平台门禁；不能将这一中间成功当作完整 quality/CI 通过。

### 完整 CI 通过，quality 最终 coverage 进行中

`scripts/ci.sh` 已完整退出 0，执行日志为 `target/phase18/ci.log`。实际完成 vendor/golden、format、workspace Clippy、Core/test-support 与 Extension/Provider 分组测试、inherited TCK 执行、扩展构建/制品检查，以及 SQLite 3.45.0 / 3.53.4 和本机 SQLite 3.51.0 的真实 load/SQL/事务/Provider/tokenizer 门禁。Phase 01–09/12/13 的适用 runtime probes、SQL18 16 组、IO18 零 callback 与 CACHE18 8 checks 全部通过。

同一 CI 再次通过 DOC18：528×2 Cypher、138 SQL、172 signatures/38 Procedures、Python 应用和 C host。18 个冻结 Cypher fixtures 由 `enabled_cypher25_fixtures_execute_against_the_real_engine` 实际执行且全通过；日志末尾单独打印的 `inventory` 只是 inventory，不取代该执行证据。compatibility harness self-check 中的 intentional failure 是验证器故意设置的负向断言，命令本身成功，不是未闭环的产品失败。

`cargo make quality` 的最终一轮已通过 format、Clippy、rustdoc、complexity、duplicate、依赖与供应链检查；instrumented Core（含 69 项新专项）已通过，instrumented Extension 已构建，正进行真实 integration/coverage。最后 coverage 与最终 diff/status 尚未完成，本 Phase 此时仍为 in_progress。

### 前轮完成验收（2026-10-18，Yi Mac；后续审计已重新打开）

最终工作树的完整 `cargo make quality` 与 `scripts/ci.sh` 均已实际退出 0。quality 完成于 293.19 秒，regions **83.92%**、functions **84.76%**、lines **85.74%**，原有覆盖阈值全部通过；format、workspace all-target/all-feature Clippy、rustdoc、complexity、原 duplicate 阈值、依赖、供应链及 instrumented Core/Extension/真实 integration 门禁全部通过。新增 C external-access smoke 已纳入既有 C 复杂度检查。

最终 instrumented Core 新专项为 O18/PROC18 **9**、R18 **6**、IDX18 **3**、M18 **12**、MR18 **6**、B18 **7**、Version **22**、EXT18 **4**，合计 **69/69**，均 0 failure；该最终结果包含新增 38-name/30-effect PROC18 和字符串插值递归共用逻辑，替代首轮 CI 中原有 68 项的局部计数。Extension **14/14**、真实 scalar/rows SQL **16/16**、IO18 零 Provider/tokenizer callback、CACHE18 **8 checks** 均通过。

最终 quality 的 inherited TCK 为 **3,777/3,777 applicable passed、0 failed**；120 个场景保持既有显式分类，没有新增排除或降低标准。18 个冻结 Cypher fixtures 通过实际 engine execution。CI 完整通过 SQLite **3.45.0 / 3.51.0 / 3.53.4** 的适用 load/SQL/transaction/Provider/tokenizer 门禁，DOC18 **528×2 Cypher、138 SQL、5 Python 应用、C host、172 signatures/38 Procedures** 及链接/覆盖全部通过。执行日志为本机 `target/phase18/ci.log` 和 `target/phase18/quality.log`，不是远端 CI 或发布证据。

全部 38 个 Procedure 的实际正向例、命名禁令、独立 effects 及领域保护证据已按上表闭合；非 Procedure 入口、权限交集、版本最终有效状态、失败原子性和历史/Schema 完整性验收均通过。两次独立领域 review 的最终结果无未闭环 finding；root 已完成最终 diff review，核对新增 C gate、Provider-cache Engine 参数、全部 27 个新增文件的语法/空白、staged 为空与 `git diff --check`，无范围外修改。文档完成状态更新后的链接/覆盖和新增文档空白再检查通过。

AC18-01–14、全部注册入口及非 Procedure 矩阵闭合，Phase 18 标记 done。能力仍为 **unreleased**；v0.3.1 tag/发行资产、storage format、Commit identity、Cypher Profile 与 Provider SPI 未变。本轮仅完成本机实施与验收，**未提交、未推送、未发布**；远端 CI、发布和下游集成不在本轮交付范围。

### 审计后重新打开（2026-10-18）

前轮的测试通过、覆盖率及本机 full gate 记录保持真实，不删除或改写为失败；它们没有覆盖本轮审计发现的 signed-zero Squash 和 NaN 无关 Revert。当时状态恢复 in_progress，原 done 结论不再代表本轮完成。本轮先核对持久 canonical value、版本操作与 Cypher 数值比较的合同归属，再修复真实重组路径并补齐上表组合用例；记录以最终实际测试和 review 结果为准。发布准备由 root 在验收之后统筹，接口维持 unreleased。

### 本轮事务/流式漏测补齐

SQL18 原 8×2=16 组保留，新增 stream 策略隔离/并行 cursor、受控提前关闭、结果长度失败、顺序/并发 batch 图对象返回、Graph View/access/staged 交集 5×2=10 组，当前开发库真实 `.load` **26/26** 通过。B18 原 7 项计数不变，`rejected_batch_rolls_back_every_row_in_that_batch` 扩充顺序及 concurrent `OF 2 ROWS` 的整批回滚，**7/7** 通过。以上来自本轮现有开发库定向结果，canonical 生产修复后的最终 artifact 仍须重建复验，不据此提前恢复 done。

### 本轮文档调用补验（历史推进记录）

用户参考补充版本“字段没变”使用既有 canonical 类型/内容含义，明确正负零区别与同 canonical NaN 的状态相同，不改变 Cypher 数值查询比较。`access_controls.py` 新增 `canonical_version_values`，通过公开 SQL/typed JSON 复现受保护负零 Squash 和保留 NaN 的无关 Revert，并断言结果与 integrity；新调用已登记到既有文档程序，待最终修复库实际验证，不用未运行示例声称修复完成。

新增文档调用在修复前开发库实际复现两个 red：`access_controls.py` 原四组仍成功，新增负零 Squash 的符号断言失败；独立的 NaN 无关 Revert 返回 `LITHOGRAPH_INVALID_ARGUMENT: patch before-condition failed`。两次均使用合成临时图与公开 SQL/typed JSON，不触碰业务数据。这个证据确认问题也经过 application SQL 边界，不只存在于内部 Rust 值比较；修复后的同一调用待重建库重验。

### v0.3.2 候选材料与定向验证过程

用户已明确发布准备目标为 v0.3.2，Cargo/lock metadata 由 root 更新。已创建 [候选说明草案](../../releases/v0.3.2.md)，最小同步 CHANGELOG Unreleased，当前用户手册/新接口标明发布候选、尚未发布；v0.3.1 固定下载/tag/制品事实保留。Provider cache 首次并发初始化问题没有混入已解决 finding。尚未执行远端 exact-SHA CI、六平台 Release Matrix、tag/上传/发布制品验证。

本轮版本专项目前 **41/41**：原 22 + effects 5 + canonical 属性 10 + Schema canonical 4；相关 checkpoint 8、storage 14、Schema 38、旧 version 46 加本组共 **147/147**。canonical 属性旧生产 7 项红后修复为扩展 10 项全部通过；Schema 4 项旧生产均红，修复后 4/4 通过，定向 Clippy 通过。精确新增用例在 [canonical.rs](../../../crates/lithograph-core/tests/phase18_version_access/canonical.rs)、[schema_canonical.rs](../../../crates/lithograph-core/tests/phase18_version_access/schema_canonical.rs)、[effects.rs](../../../crates/lithograph-core/tests/phase18_version_access/effects.rs)。公开 SQL/Provider 的 v0.3.2 真实库结果见下文；最终 quality/CI 与包内完整 smoke 的通过证据见下文。

Schema/graph 与查询生产修复均已冻结；独立 canonical/runtime/test review 无剩余 finding。reduce 函数名识别、字符串插值内 COUNT/COLLECT/EXISTS、lazy branch、列表作用域/累加器/变量遮蔽、排序投影别名和原分组上下文已有长期回归，Q18 最终定向 **9/9**、QC18 **5/5** 通过。QC18 还核对 metadata/事务拥有权、版本操作组合与 candidate 上下文边界，避免插值绕过既有生命周期合同。新最终 evidence 目录为 `target/phase18-recheck`；本节不把前轮 `target/phase18` 的 green 当成本轮最终结果，当时 Phase 保持 in_progress，最终完成见下文。

最终永久 Core 专项合计 **112**：options 12、interpreter 8、index 6、mutation 12、mutation reads 6、batch 7、version 41、external 4、独立 Schema canonical 2、interpolation 9、classification 5。该集合包含所有本轮补验；最终 quality、CI 和 release 三份日志均已逐 target 核对为 112/112，证据见下文，不由前轮 69 的 green 推算。

最终 debug 扩展已实际构建为 **0.3.2**。以该库重新生成 `functions.md` / `procedure-inventory.md` 并运行 generator `--check`，**172 signatures / 38 Procedures** 一致。公开 `access_controls.py` 的原四组与新增 `canonical_version_values` 均通过，后者在真实 SQL 边界验证受保护 `-0.0` Squash 的符号、canonical NaN 的无关 Revert 及 integrity。这些结果闭合对应文档调用的 red；全量文档示例与最终 quality/CI 已由本轮门禁通过，证据见下文。

SQL18 随后在同一 v0.3.2 debug 真实库通过 **28/28**，含新增 metadata 插值事务 lifecycle 拒绝/whole staged abort。ART18 独立公开 SQL **5/5** 通过，每组确认版本 0.3.2 并运行完整 integrity。release 优化构建已成功，本机 `target/phase18-recheck/release/lithograph-macos-arm64.tar.gz` 与同目录 `SHA256SUMS` 已生成，解包后 VERSION 与真实 embedded version 均为 0.3.2；包内完整 smoke 随后通过，证据见下文；以上不代表远端 CI、六平台或发布资产验收。

### 本轮外联与配置组合漏测登记

IO/config 组已冻结新增回归：CSV18 28 组，IOV18 的 20 个历史/host/surface 场景与 2 个 opaque ProviderConfig canonical 场景；C smoke 另补 Full-text Relationship 及隐藏 FTS Node/Relationship 的零 callback，每次拒绝核对 head、Commit/Layer/Schema 数不变。CSV18 与新增 C 复杂度检查已接入 CI/coverage。

修复前库的 CSV 原有形态 24 组通过，当时真实插值的完整 28 组待查询修复后执行；新增 opaque 配置 Diff 在旧库如预期为 red（丢失符号变化 operation）。这些属于漏测补齐与红灯证据，不计为最终候选通过。后续已统一以重建 v0.3.2 artifact 运行，结果如下。

随后以最终 v0.3.2 debug 真实库复验：CSV18 **28/28** 退出 0，涵盖真实字符串插值的 file/HTTP 读取；HTTP 正向每次恰有 1 请求、合计 10 次，全部拒绝路径请求增量为 0。IOV18 **22/22** 退出 0，其中 20 个版本/host/surface 场景与 2 个 opaque ProviderConfig canonical 场景全部通过；各类 callback 均有正向大于 0 的对照，拒绝逐次验证 callback 为 0，head 与 Commit/Layer/Schema 数不变。日志分别为 `target/phase18-recheck/csv.log`、`target/phase18-recheck/external.log`；新增专项无未闭环 finding。

候选文档独立 review 发现两处文本问题：Reference 的外联说明未清楚区分完整 AST 禁令与 Schema 实际变化所需的外部验证，Phase 当前段保留已经过期的待重建结论。均已修订并再次只读复核，无剩余 finding。静态门禁再次通过 80 个公开 Markdown、10 个 design/development/CHANGELOG 文档的本地链接/锚点、528 个示例登记、5 个新文档文件语法/空白与 `git diff --check`。没有改变本轮已冻结的可执行断言。

### 本轮最终源码门禁（2026-10-18，Yi Mac）

`target/phase18-recheck/quality.log` 与 `ci.log` 对应的完整 `cargo make quality` / `scripts/ci.sh` 均已捕获 **exit 0**。quality **519.49s**，最终 coverage 为 regions **84.33%**、functions **84.97%**、lines **86.10%**；format、Clippy、rustdoc、复杂度、重复率、依赖与 supply-chain 门禁未降低原阈值。两个日志各自实际运行 11 个 Phase 18 Core targets，逐项结果均为上述 **112/112、0 failure**；release 优化配置同一 112 项与 `phase09_version` **46/46** 也通过，记录在 `release-core.log`。

最终 inherited TCK 为 **3,777/3,777 applicable passed、0 failed**，120 个既有分类不变，18 个冻结 Profile fixtures 实际执行通过。CI 的 Core、Extension **14/14**、Provider 分组、SQLite 多 runtime 真实 load/事务/双扩展及外联门禁均通过；文档 **528×2 Cypher、138 SQL、5 Python 应用、C host、172 signatures/38 Procedures** 全部通过。以上为本机最终源码门禁，不是远端 exact-SHA CI 或六平台 Release Matrix。

解包候选库的独立 canonical SQL 再次 **5/5**，每组版本 0.3.2/full integrity 通过，见 `release-canonical.log`。本机候选包内完整 `release-artifact.log` 随后捕获 **exit 0**，实际加载解包后的 `release/unpacked/lithograph.dylib` 及同包 Provider。Phase 01–09、artifact/surface、SQL18 **28/28**、CSV18 **28/28**、IOV18 **22/22**、Provider cache **8 checks**（含 warm-hit 拒绝），以及 SQLite **3.45.0 / 3.53.4** 适用 load/SQL/transaction/Provider 双扩展/Phase 12/13 全部通过。本机 interop create/verify 也通过；这是同平台验收，不宣称跨平台兼容已实测。

### 访问控制完成状态与发布边界（追加 cache 修复前）

AC18-01–14、AC18-R01–08、全部 38 个 Procedure 和非 Procedure 入口矩阵均已通过。两条最初审计 red、Schema canonical、插值与生命周期等后续发现均完成复现 → 修复 → 定向回归 → 独立复核，canonical/runtime/test/docs review 无未闭环 finding；最终源码与候选包门禁完成后，本 Phase 当时恢复 **done**。该次完成结论以上述本轮 112 项和实际门禁为依据；随后用户追加的 Provider cache 修复状态见下一节。

v0.3.2 仍为 **发布候选、尚未发布**。本机 macOS arm64 包 SHA-256 为 `a615b5d7a2ff5aef8a6c1f47e8be2f736d732c8d8525e300ae069fcf5f15e649`；包及本机日志在 `target/phase18-recheck/`，不进入 Git。远端 exact-SHA CI、六平台 Release Matrix、Windows PowerShell/MSVC 路径、真正跨平台 interop、tag、上传与发布制品回验均未执行。本轮未提交、未推送、未 tag、未上传或正式发布；本机通过不冒充这些外层验收。

### 追加 cache 修复最终完成状态

用户已授权修复、补测、循环 review 和重新准备 v0.3.2，未授权 commit、push、tag、上传或正式发布。Provider 源码与测试已经冻结；初始化取得 `BEGIN IMMEDIATE` 后重查 schema，另一进程已经完成初始化时复用相同 marker/schema/accounting 校验。schema inventory 同时核对 name/type 与真实 `sqlite_` 字面前缀，拒绝 VIEW、`sqlitex`、用户 index/trigger 混入或冒充；warm cache 仍不要求 writer。cache schema version、key encoding、配置字段、SPI 和 storage format 保持不变。

`target/cache-init-032/before.log` 对旧 Provider 的真实双进程 barrier 复现一个请求成功、一个 `IO_ERROR`；`after-debug.log` 记录新 Provider 的 **15 checks 全部通过**。新增 missing path/已有空文件各让 4 个进程完成空 schema 观察，再由 winner 初始化并先 publish，随后 3 个 stale reader 成功返回；验证向量、UNIQUE、accounting、winner entry 保留与 reopen/hit。新增 7 个 family 共 33 次 SPI 调用（11 次 HTTP 成功、9 次离线 warm 成功、13 次预期拒绝），拒绝核对零 HTTP、无修改；初始化取消或有界超时不留下 partial schema，释放阻塞后可以恢复，持 RESERVED writer 的离线 warm hit 不改 cache 字节。production Provider **24/24** unit、Clippy、rustfmt、Lizard 通过，独立源码/测试 review 无剩余 finding。

初次完整 quality 曾通过；随后 cache 权限 fixture 在 SQLite 3.53.4 的 CLI 错误文本没有打印 `(23)`，使初轮 CI 因测试的显示格式假设失败。修正 fixture 后直接通过 SQLite C API 从同一次执行取得真实 `SQLITE_AUTH` **23** 与 `ACCESS_DENIED`，并在同一 connection 上验证 `RETURN 1` 恢复；scalar/rows 都继续核对独占 cache 锁与 cache 字节不变，没有放宽产品错误合同。初始日志保留为 `quality-initial.log` / `ci-initial.log`；最终完成以下面的重跑为准。

最终 `quality-final.log` 与 `ci-final.log` 均捕获 **exit 0**。quality **359.72s**，coverage 为 regions **84.33%**、functions **84.98%**、lines **86.11%**，全部原门禁与阈值保持；quality 与 CI 各自实际通过 **112/112** Phase 18 Core 专项，Extension **14/14**、Provider **24/24** 与适用 runtime/双扩展回归通过。quality inherited TCK 为 **3,777/3,777 applicable、0 failed**，120 个既有分类不变；18 个冻结 fixtures 实际执行通过。CI 再次完成 SQLite **3.45.0 / 3.51.0 / 3.53.4** 各 **15 cache checks**、SQL **28/28**、CSV **28/28**、外联 **22/22**；文档 **528×2 Cypher、138 SQL、5 Python 应用、C host、172 signatures/38 Procedures** 全通过。

新包 `target/cache-init-032/release/lithograph-macos-arm64.tar.gz` 的 SHA-256 为 `c397cd8e0ea53b78b2a1c069c4ce8b78f6ba3ace60a7aea37c22d632c0cd5997`，与同目录 `SHA256SUMS` 一致；解包 VERSION 与真实版本均为 0.3.2，`package-version.log` 记录直接加载结果。新解包 Engine / Provider 的完整 `release-artifact.log` 捕获 **exit 0**：三个 SQLite runtime 各 **15 cache checks**、SQL **28/28**、CSV **28/28**、外联 **22/22** 及既有 load/SQL/transaction/双扩展/Phase 12/13 全通过，新 interop fixture create/verify 通过。`release-canonical.log` 独立公开 SQL **5/5**，每个 fixture 核对真实 0.3.2 与完整 integrity.ok=true；Engine SHA-256 为 `0902df457cb3cab8732b5434d52566a2036222c0bcc3ffc96086b9d4f7fae5f6`。旧 `release-core.log` 的优化配置 112 项仍是追加前证据，本次没有将它改称新运行；旧 `a615…` 包也保留为历史。

Unix CI、release smoke 与 Windows wrapper 已接入同一永久 gate；Windows 本机未执行 MSVC/PowerShell，不能把脚本静态通过当成目标平台验收。最终源码/测试/gate 与文档独立 review 无剩余 finding；80 个公开 Markdown、此次 9 个文档的本地链接/锚点/覆盖、新文档空白与 `git diff --check` 均通过，没有修改冻结后的可执行断言。AC18-C01–05 与原 AC18 / AC18-R 全部闭环，Phase 18 恢复 **done**。v0.3.2 仍是**本机未发布候选**；远端 exact-SHA CI、六平台 Release Matrix、真实跨平台 interop、tag、上传与发布后回验均未执行。未提交、未推送、未创建 tag、未上传或发布。v0.3.1 的已发布 known issue 与串行预热规避继续适用。

<a id="v032-release-delivery"></a>

## v0.3.2 发布交付记录

用户在本机实施与制品验收完成后，明确授权提交和推送，并要求精确 SHA 的远端 CI 与六平台 Release Matrix 全部通过后正式发布 v0.3.2。本 Phase 的开发验收保持 done，发布状态按下面的实际结果单独记录。此前“未授权提交/发布”及“本机未发布候选”段落保留当时事实，不代表当前授权边界。

首次发布源码 `50b26f95262625cab33f08d834e7b7a49492446d` 已提交、推送到 main 并核对远端 SHA，但首轮远端未全绿：CI 的 Linux job 失败，macOS/Windows basic jobs 通过；Release Matrix 的 Linux x64/arm64 cache fixture 预期 `IO_ERROR` 2 却得到成功 0，Windows arm64 则因测试拼成 `file://C:\...` 而被生产 URI 校验正确拒绝。前者已用本机 Homebrew SQLite A/B 复现：持有 writer 后执行 `fs::read` 会打开并关闭同一 inode 的另一个 fd，POSIX 因此释放进程自己的锁；`target/release-032/posix-lock-close-ab.json` 记录保留该快照读取时独立进程成功、移到持锁前则真实返回 `BUSY` 5。cache fixture 正在保持 500 ms 等待、零 HTTP、字节与 Schema 断言的前提下修正，并补独立进程 `BUSY` 5 前置证明；生产实现不变。Windows 路径 fixture 已复用既有 Phase 08/15 helper 生成 `file:///C:/...`，release 定向 9/9、Clippy 与独立 review 通过。该轮失败不计为平台验收完成；修复后仍须以新 SHA 重新通过完整远端 CI 和六平台矩阵。

| 交付项 | 当前实际状态 |
| --- | --- |
| 实现、完整本机质量与包内验收 | 已完成；结果与本机 SHA 见前文 |
| 正式版本文档与发布说明 | 已准备 v0.3.2 版本适用说明和固定 tag/下载入口；下载入口在正式发布后使用 |
| 首次发布提交与推送 | [50b26f95262625cab33f08d834e7b7a49492446d](https://github.com/bYiyLi/Lithograph/commit/50b26f95262625cab33f08d834e7b7a49492446d) 已提交、推送 main，远端 SHA 已验证 |
| 精确 SHA 的远端 CI | [CI 39107295376](https://github.com/bYiyLi/Lithograph/actions/runs/39107295376) 的 Linux job 失败，macOS/Windows basic jobs 通过；等待 fixture 修复后的新 SHA 完整重验 |
| 六平台 Release Matrix | [Release Matrix 39107295388](https://github.com/bYiyLi/Lithograph/actions/runs/39107295388) 的 Linux x64/arm64 cache 与 Windows arm64 URI fixture 失败，本轮不计六平台通过；等待新 SHA 完整重验 |
| v0.3.2 tag 与正式发布 | 已授权在前述远端检查全部成功后执行；尚未创建 tag、上传或发布 |
| 正式下载资产与发布制品回验 | 尚未执行；本机包和历史 v0.3.1 证据不替代正式资产验收 |

tag workflow 直接使用 [v0.3.2 发布说明](../../releases/v0.3.2.md) 作为 GitHub Release 正文。公开手册的固定 v0.3.2 链接预先随发布提交准备，不表示 tag 或资产已经存在；待实际完成后在此追加源码、run URL、tag、资产校验和与制品回验结果。
