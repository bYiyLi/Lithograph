# Phase 11：Performance Optimization

**状态：`done`**

## 1. 目标与范围

在Phase00–10已经完成的功能/兼容基线上，关闭已证实的邻接访问、重复Snapshot解析和Standard Index冷启动问题，建立可重复的端到端latency、吞吐、物理工作量、memory与并发回归门禁。读者是Lithograph实现与验收人员；目标不是重新宣告功能完成，也不是承诺所有硬件上的统一SLA。

Phase10保持 `done`，其原始证据作为历史保留。后续调查发现的真实access-path问题由本Phase修复，不能用旧checklist勾选或logical operator名称作为免修理由。产品行为唯一真源为 [设计入口与其职责表中的专题](../../design.md#design-ownership)；本计划只定义实施顺序、Feature owner与验证。

**必须交付**：真实性能harness、邻接keyset、query-owned resolved state/read guard、format3迁移、persistent Standard Index base/delta及明确rebuild入口、受影响Graph View/transaction/adapter回归、扩展Search/Merge/mixed-workload测量、六平台收尾。

**不默认交付**：新Cypher方言、新C ABI、Server/daemon、分布式/并行查询、JIT、通用配置/插件层、全executor slot化、FTS/HNSW重写、10M高维向量SLA。只有本Phase真实profiling或验收失败支持时，才做最小必要的局部实现，不提前建第二套engine。

## 2. 依赖与Design Inputs

- Phase00–10全部 `done`；Phase 11 的优化前测量基线来自 `cccb760` / storage format 2，Phase 11实现提交使用storage format 3，不能把before基线描述成当前实现。
- [加载与初始化](../../design/interfaces.md#loading-and-initialization)、[SQL Bridge](../../design/interfaces.md#sql-bridge)、[Native C ABI](../../design/interfaces.md#native-c-abi)、[Query Options](../../design/interfaces.md#query-options)、[Query-scoped Resolved State](../../design/query-engine.md#query-scoped-resolved-state)、[Adjacency Keyset Cursor](../../design/storage.md#adjacency-keyset-cursor)、[Persistent Standard Index Base + Delta](../../design/schema-and-indexes.md#persistent-standard-index)、[Performance Storage Format 3](../../design/storage.md#storage-format-3)、[Performance Evidence Contract](../../design/runtime.md#performance-evidence)、[固定基线性能目标](../../design/runtime.md#performance-targets)、[扩展压力场景与范围控制](../../design/runtime.md#stress-workloads)、[D12](../../design/decisions.md#d12) / [D13](../../design/decisions.md#d13)。
- [性能证据](../../research/phase11-performance-evidence.md)：E1–E4区分已观察问题、代码证据与未证实热点。
- [Compatibility inventory](../cypher25-compatibility.md)：继续冻结 `CY25-2026.08`，性能优化不增加waiver。
- [Phase10](10-compatibility-release.md)：继承correctness/recovery/release基础，不把旧单次计时当新P95。

Design与前置Phase已具备，11.1→11.8的实现、量化验收、完整门禁、review finding与文档同步现已全部闭合，因此状态为 `done`。

## 3. Feature顺序与职责

```text
11.1 Reproducible measurements / physical-work counters
 -> 11.2 Adjacency keyset
 -> 11.3 Query-scoped state / read lifetime
 -> 11.4 Format 3 / persistent generation vertical slice
 -> 11.5 Incremental index overlay / cross-state coverage
 -> 11.6 Measured executor hotspots
 -> 11.7 Search / Merge / mixed-workload coverage
 -> 11.8 Final performance / recovery / platform closure
```

11.1必须先完成，不能优化后再选择有利的before数据。每个Feature先targeted验证再继续，不在每次局部改动后重复全量20分钟级测试；11.8统一执行完整门禁。

### Feature 11.1 Reproducible performance harness

**优先级：P0；来源：[Performance Evidence Contract](../../design/runtime.md#performance-evidence)、[固定基线性能目标](../../design/runtime.md#performance-targets)、[扩展压力场景与范围控制](../../design/runtime.md#stress-workloads)；依赖：Phase10。**

扩展现有test-support和scale runner，不引入通用benchmark服务。新增独立Phase11 runner/report，保留Phase10原report不覆盖。至少输出Design规定的provenance、时段分解、latency样本、CPU/RSS/TEMP/WAL、物理plan/counters和结果校验。

实现项：

- 统一Core prepare-to-consume计时；Native与`lithograph_rows`同query分开计时。显式init、fixture导入、DDL/build与独立EXPLAIN不得藏进/移出某个case的成本。
- 分离warm、新connection persistent-ready、generation-missing/rebuild、post-delta四类；OS cache未清理时明确标为未控制。
- 保存每条case query/params/options、seed、distribution、expected rows/checksum、sample count和全部failure；真实校验cardinality，不能只通过min/max ID推测没有缺行。
- 使用公开SQLite counters，Engine hooks记录resolve/Layer load/build/changed-owner数。正常release查询不启用高开销诊断；必须测并报告instrumentation自身开销。
- 在原始基线代码上采集新的before报告。新增hooks需要代码时，只添加instrumentation后冻结独立tree digest，先测before再改算法；不得把后续优化混入before。

**Acceptance**：下文6.1中既有接口可执行的before cases能独立运行且report可重现；generation-reopen、format3与rebuild等新增能力在对应Feature加入after cases，before标为 `not_available` 而不是假造通过，也不反向阻塞11.1。故意引入错误SQL access path、重建或额外resolve时，counter/结果gate能失败；未测/计数溢出不当成0。基线报告被保留并具有可核查摘要。

### Feature 11.2 Adjacency access-path closure

**优先级：P0；来源：[Adjacency Keyset Cursor](../../design/storage.md#adjacency-keyset-cursor)；依赖：11.1。**

主要owner：`storage/snapshot_scan.rs`、`storage/snapshot.rs`、`query/stream.rs`、`query/completeness/path.rs`及mutation matcher的邻接调用者。

按Design迁移typed/untyped、outgoing/incoming/incident的cursor，checkpoint/overlay共用顺序，处理tombstone与self-loop。搜索并替换所有在热路径仍依赖globalRelationshipId分页的调用；不要只修benchmark那条query。保留canonical ID、Layer排序与公开cursor协议。

**Acceptance**：

- positive/negative、0/1/high degree、incoming/outgoing/undirected、parallel/reverse/self-loop、多个types、page-size1/17/256/4096全部与canonical oracle一致。
- 第一/末页、整页删除、跨页同endpoint重复、overlay add/remove、历史与staged Snapshot、Graph View过滤后空页均不漏/重/死循环。
- 同一小degree增加10x无关Relationship，SQLite work满足6.2结构gate；large hub不会每页重扫/排序全部degree。3.45.0与冻结release runtime都验证真实plan，不依赖易变EXPLAIN文本逐字匹配。

### Feature 11.3 Query-owned resolved state与resource lifetime

**优先级：P0；来源：[Query-scoped Resolved State](../../design/query-engine.md#query-scoped-resolved-state)；依赖：11.2。**

主要owner：`storage/snapshot.rs`、`query/plan.rs`、`query/stream.rs`与Extension的`execution.rs`/`rows.rs`/`native.rs`。

将owned immutable resolved state与connection accessor分离，贯通planner和cursor；以read guard覆盖pin-to-consume。读guard不扩大writer ownership、不改变caller-owned transaction，清理路径必须覆盖正常结束与每类早退。调整内部Rust API不能改变SQL/C ABI。

**Acceptance**：

- 同一普通read的resolved-state build为1，消费batch从1增加到1000以上不增加lineage/Layer重放；prepare与execute共享，不只是每处各缓存一份。
- writer在另一connection移动Branch/Tag、GC旧ref、逐出checkpoint/cache时，已打开reader稳定读取旧结果；下一个execution看到新状态。
- EOF、LIMIT、cancel、callback error、panic、xFilter rescan、xClose、connection teardown均释放guard；宿主outer transaction既有写入不被read cleanup提交或撤销。
- 同connection重入mutation/GC/maintenance拒绝且无副作用；多个read cursor可同时存在。Native explicit transaction的跨execution staged可见性、时钟、fail-closed abort与Merge revision无回归。
- 图checkpoint与SQLite WAL checkpoint分开统计，read释放后writer/WAL可继续推进；实现不存在lifetime transmute或悬挂connection引用。

### Feature 11.4 Format 3与persistent index纵向闭环

**优先级：P0；来源：[Generation identity 与持久布局](../../design/schema-and-indexes.md#generation-layout)、[Build、publish、read-only 与 maintenance](../../design/schema-and-indexes.md#build-publish-maintenance)、[Adoption boundary](../../design/schema-and-indexes.md#adoption-boundary)、[Performance Storage Format 3](../../design/storage.md#storage-format-3)；依赖：11.3。**

主要owner：Core storage schema/integrity/migration、`query/schema/index/cache.rs`、Schema DDL、procedure inventory、Extension init/version/adapters。

先完成一条真实Range Index slice：fresh3或2→3 → definition/anchor generation →关闭connection→另一connection直接seek→删除fixture payload→read fallback→显式rebuild→相同结果与history。完成之后覆盖Text、Point、Relationship Lookup及Relationship property indexes，不把只通过Range的局部实现记为Feature完成。

`lithograph.index.rebuild`完全按[Build、publish、read-only 与 maintenance](../../design/schema-and-indexes.md#build-publish-maintenance)执行，不新增并行public配置体系。固定table/index inventory由同一schema声明验证；generation build不全局DROP其它index。NewDDL和Native staged index在正确Commit/durability边界发布。

**Acceptance**：

- exact-schema、fresh/init/repeat-init、旧format只读、旧format写拒绝、1→2→3全程rollback、2→3rollback、future-format拒绝及旧Engine读3拒绝通过。
- 原databaseId、Commit/Layer/Schema hashes、oldformat hash验证、refs/data/session都保持；迁移不全量构建index。
- newconnection/reopen读取不重新build；name相同但definition不同不能复用，无关Schema修改不导致cache失效。
- rebuild结果/错误/options、SHOW/validate/EXPLAIN、READ_ONLY_ADAPTER、READ_ONLY_SNAPSHOT、explicit transaction rejection均有测试。
- crash/cancel/disk-full/authorizer-cleanup故障不发布半generation、不误提交宿主外层transaction。测量完整build与writer持锁，不能只测complete-marker写入。

### Feature 11.5 Incremental base + delta reads

**优先级：P0；来源：[Range / Text / Point](../../design/schema-and-indexes.md#range-text-point)、[Read path 与 delta overlay](../../design/schema-and-indexes.md#base-and-delta)；依赖：11.4。**

在persistent base之上实现target-state changed-owner overlay，不复制全index、不为每个Commit建立全generation。已有generation的Relationship property读取使用其相关type/domain access path；初次完整构建与Lookup复用按[Build、publish、read-only 与 maintenance](../../design/schema-and-indexes.md#build-publish-maintenance)执行并独立计时，不能把全库枚举带入ready read。

**Acceptance**：

- 1/100/1000 owners的SET/REMOVE/delete/label-domain change；Composite任一成员改变；旧match→不match、新match、相同值、缺property、不同type/null全部与scan oracle一致。
- 空Commit、无关property/Label/Schema变化无full build；跨Branch、first-parent merge、reset/revert、历史typed proof不使用错误generation。
- Native跨execution与Merge candidate绑定各自state identity，revision变化使query-local cache失效；commit前不能以staged结果污染durablegeneration。
- 输出page不重复收集delta/candidates；Graph View在semanticLIMIT/top-k/aggregation前生效；没有无界结果ID集合。
- 至多保留Design允许的anchors，GC/eviction不保留本应删除的canonical history；缺失、incomplete、encoding mismatch与可检测payload损坏走完整fallback，不跳过单个坏row。
- 首行前cache失效的fallback与已输出结果后的fail-closed分开测试，遵守[Adoption boundary](../../design/schema-and-indexes.md#adoption-boundary)；不得重复输出或在错误之后发送成功SUMMARY。离线单entry篡改由显式integrity oracle覆盖，不把未做的全量核验称为point query已验证。

### Feature 11.6 剩余executor热点

**优先级：P1；来源：[Query-scoped Resolved State](../../design/query-engine.md#query-scoped-resolved-state)、[扩展压力场景与范围控制](../../design/runtime.md#stress-workloads)；依赖：11.5。**

重跑分段profile，先量化Graph View endpoint重复读取、statement prepare、page归并、BindingRow clone、projection/serialization成本。已有lowest-cardinality Label optimizer只补路径覆盖缺口；不再新建一套。

Proof允许时复用起点、批量终点检查、有预算visibility cache、移动而非clone bindings。若前三项优化后已满足性能目标且未证实allocation主导，记录“不需要slot重构”的证据即可；反之只在受影响read pipeline实施最小slot/row改动。该分支不允许用“以后优化”豁免6.3性能目标。

**Acceptance**：保存新的profile与决策；必要改动有before/after收益，Graph View/OPTIONAL/重复行列名/类型错误/path uniqueness/staged revisions回归全通过，cache达到预算后逐出而非OOM。

### Feature 11.7 扩展压力与非回退验证

**优先级：P1；来源：[固定基线性能目标](../../design/runtime.md#performance-targets)、[扩展压力场景与范围控制](../../design/runtime.md#stress-workloads)；依赖：11.6。**

实现Search corpus、Native多execution写、Version/Diff与10K-conflict Merge，以及1/4/8 readers+1writer压力场景。沿用Design的数据/recall与计时规则，不将1000样本成绩放大宣传，不把total finalize计为writer hold。

**Acceptance**：Search的规模/维度/recall/历史/Graph View证据齐全、无OOM；30分钟mixed run结果与snapshot oracle一致，错误/BUSY分类完整、资源可回收；Merge40轮结果/Revision/CAS/一次finalize/GC roots正确。发现热点时只修本Feature的真实finding，不预先重写Search或Merge算法。

### Feature 11.8 Final closure

**优先级：P0；来源：[Integrity、Recovery 与 Migration](../../design/storage.md#integrity-recovery-migration)、[Large-scale Invariants](../../design/runtime.md#large-scale-invariants)；依赖：11.1–11.7。**

重跑6.1–6.3全部hard gates、完整compatibility与repository quality、真实Extension/native/rows、1/2→3迁移/崩溃与六目标Release Matrix。小CI fixture验证结构和semantics，真实10M/100M在固定performance host做定量验收，两者不能相互替代。

同步README的真实实现状态、Design的已实现/目标标记、compatibility执行证据、Phase11与vlog；保留Phase10历史与before report。正式Release、commit、push仍需各自授权；只有真实hosted结果存在才勾选远端acceptance。

## 4. Implementation/Evidence状态

Phase 11实现提交已经实现并完成targeted验证的范围包括：11.1性能harness/物理counter、11.2邻接keyset、11.3 query-owned resolved state/read guard、11.4 format3/persistent Standard Index/rebuild、11.5 ancestor/staged changed-owner overlay，以及11.6基于实测暴露的Range/new-connection/HNSW热点修复。固定性能机上的10M Node / 100M Relationship验收已证明邻接、Range seek、1000-row Range、1000-owner delta、SQL rows/Native streaming与RSS目标满足[固定基线性能目标](../../design/runtime.md#performance-targets)；1M×128、100K×1536 Vector与1M全文语料已保存cold/warm、historical/Graph View和tie-aware recall证据，10K-conflict Merge已保存prepare/writer-wait/writer-hold拆分。

Phase 11现已闭合：1/4/8-reader三档mixed workload均各运行30分钟并通过；Search、10K Merge、10M/100M定量门禁、最终repository-wide regression/quality/coverage、real-load与当前SQLite CI均通过。实现提交 `4695ebbd5887b7fcafa2dd64d9886c4a066bfd05` 已推送到 `origin/main`，GitHub hosted Release Matrix run `35091276800` 的Linux x64/arm64、macOS x64/arm64、Windows x64/arm64六个平台job全部 `success`，并分别生成 `lithograph-linux-x64`、`lithograph-linux-arm64`、`lithograph-macos-x64`、`lithograph-macos-arm64`、`lithograph-windows-x64`、`lithograph-windows-arm64` artifact。同期CI run `35091276893` 也为 `success`。

## 5. Requirement追溯

下表是验收索引；每条正式行为/量化要求以所指Design为唯一真源。Class与优先级描述本Phase执行与验收职责，不产生第二套产品定义。

| ID | Class | 验收对象 | 来源 | 优先级 | Verification |
| --- | --- | --- | --- | --- | --- |
| P11-MEASURE | quality | 可重现、分缓存/adapter的计时与物理work | [Performance Evidence Contract](../../design/runtime.md#performance-evidence) | P0 | 11.1、6.1/6.2 |
| P11-ADJ | functional | 同序keyset与完整overlay邻接语义 | [Adjacency Keyset Cursor](../../design/storage.md#adjacency-keyset-cursor) | P0 | 11.2、6.2 |
| P11-STATE | data | query-owned state/read guard与revision | [Query-scoped Resolved State](../../design/query-engine.md#query-scoped-resolved-state) | P0 | 11.3、并发/清理矩阵 |
| P11-CACHE | data | generation reuse、delta覆盖、可重建 | [Persistent Standard Index Base + Delta](../../design/schema-and-indexes.md#persistent-standard-index) | P0 | 11.4/11.5 |
| P11-API | interface | rebuild procedure/adapters/errors | [Build、publish、read-only 与 maintenance](../../design/schema-and-indexes.md#build-publish-maintenance) | P0 | procedure inventory + SQL/Native |
| P11-FORMAT | operational | 显式format3迁移与旧历史保持 | [Performance Storage Format 3](../../design/storage.md#storage-format-3) | P0 | 11.4/11.8 recovery/migration |
| P11-SAFETY | security | 无隐式read写入、schema/TEMP-trigger防线 | [加载与初始化](../../design/interfaces.md#loading-and-initialization)、[Build、publish、read-only 与 maintenance](../../design/schema-and-indexes.md#build-publish-maintenance) | P0 | readonly/authorizer/collision tests |
| P11-LATENCY | quality | 定量延迟、RSS、非回退目标 | [固定基线性能目标](../../design/runtime.md#performance-targets) | P0 | 11.8、6.3 |
| P11-STRESS | quality | Search质量、并发与资源回收 | [扩展压力场景与范围控制](../../design/runtime.md#stress-workloads) | P1 | 11.7/11.8 |

## 6. Verification与完成门禁

### 6.1 Workload矩阵

| 场景 | fixture | 验证与报告 |
| --- | --- | --- |
| CI access-path | 固定degree/result，10K→100K→1M无关relationships | 多方向/type/keyset/overlay，物理work不随无关数据线性增长 |
| 核心scale | 10M Nodes / 100M Relationships、1M hub、相关属性/约束 | scan/seek/traversal/0结果，全结果消费，不只核对首行 |
| Snapshot维度 | 相同图、不同比例delta；0/256/4096个empty/small Commit链 | resolve/Layer load与输出batch解耦，history/anchor成本单列 |
| cache生命周期 | ready/reopen/missing/rebuild；0/1/100/1000 changed owners | 不全域复制；old/new匹配、分支与rollback正确 |
| 图模型/Schema | Node与Relationship；多Label/type；Range/Text/Point/Lookup | Composite、类型/null、Graph View及readonly历史 |
| Search | [扩展压力场景与范围控制](../../design/runtime.md#stress-workloads)的100K/1M文档与向量集合 | build/read/cold/warm、recall与visible exact oracle |
| Version/transaction | 1000Node写；多execution；256 Commit/4页History；300ops Diff；10K conflict | latency + counters + CAS/revision/rollback；writer阶段拆分 |
| Mixed workload | 大图；1/4/8 readers + 1writer，每种至少30分钟 | correctness、throughput、P95、BUSY/失败、WAL/RSS、取消后回收 |

只使用runner生成并标识的disposable fixture或其独立副本。cache删除、GC、migration/crash不得对真实用户database执行；不把本地26GiB fixture、token或trace提交Git。参考host空间不足属于未完成capacity case，不能自动改小数据后沿用同一report标签。

### 6.2 结构性hard gates

- 固定degree/result下，汇总每个query的全部相关SQLite statement，满足[Performance Evidence Contract](../../design/runtime.md#performance-evidence)的无关数据增长比值gate；同时验证covering endpoint/type path和无不必要SORT。NodeID的B-tree point work允许对数变化，不把不含endpoint条件的主键范围扫描叫seek通过。
- 同一read execution一次resolved state；batch-size变化不增加lineage/Layer重放。Counter覆盖prepare和consume，不只测其中一段。
- persistent-ready/reopen及小delta查询的full generation build count为0；changed-owner读取与实际相关delta对应，不扫描10M owner来判断变化。
- O(result-count) retained集合、每页重扫hub、每个新Commit复制全index、read adapter隐式main write、遗漏rollback/cleanup，任一出现即失败；绝对latency暂时好看也不能豁免。
- 0结果与“命中在尾部”的case同样必测，避免 `LIMIT 1` 提前退出掩盖无关扫描。

### 6.3 定量hard gates与report

完整采用[固定基线性能目标](../../design/runtime.md#performance-targets)的统一hardware、重复次数、每状态P50/P95、长query中位数/最大值、latency/RSS与非回退要求，不在本计划重复维护另一份数值表。Before/after共用query/fixture/adapter/runtime/durability配置；generation存在与否必须标记，完整build和reopen成本同时保存。

每个Feature结束保存可核查的摘要（tree、环境、命令、数据manifest、原始artifact位置、结果、已知限制）。完整原始样本输出到ignored `target/phase11-performance/`；文档中的证据摘要与实际report一致，缺原始report或失败记录不全不能通过。

### 6.4 Regression命令与平台

现有命令继续有效。Phase11当前新增并实际使用：Core integration target `phase11_cache`；test-support binaries `lithograph-phase11-performance`、`lithograph-phase11-search`、`lithograph-phase11-delta`；以及 `phase11-adapter-bench.sh`、`phase11-stream-bench.sh`、`phase11-mixed-stress.sh` 的真实SQLite/Native C harness。Format3/recovery仍由现有 `phase10_recovery`、Phase09 real-load probe与Native ABI smoke覆盖，不为了Phase编号复制第二套recovery测试。

Targeted至少覆盖现有 `phase02_storage`、`phase02_checkpoint_integrity`、`phase04_query`、`phase05_mutation`、`phase06_query`、`phase07_schema`、`phase08_search_ingestion`、`phase09_version` 以及Phase10 hardening/compatibility/recovery；按受影响Feature选择，11.8执行全部。Core tests与loadable-extension probe继续独立Cargo invocation，避免feature-unification误把host ABI模式带入Standalone测试。

最终运行 `cargo make quality`、`scripts/ci.sh`、真实SQLite rows/Native probes与六目标Release Matrix。最低runtime维持SQLite3.45.0，另外验证仓库冻结的release-current runtime（当前3.53.4），不把“当前”解释为实施日自动升级。所有目标都验证format3 shared fixture、migration与旧版本拒绝；完整10M/100M定量测试不要求在每个hosted runner复制一遍，但固定host结果不能替代六平台correctness。

## 7. Phase Acceptance

- [x] P11-MEASURE：11.1基线/原始样本、缓存口径、adapter、计时拆分及物理counter完整。
- [x] P11-ADJ：所有邻接方向/type/overlay/path调用通过语义与物理访问gate。
- [x] P11-STATE：一次resolve与read guard、并发GC、staged/candidate revision、全部清理路径通过。
- [x] P11-CACHE：所有本期Standard Index跨connection复用与delta/eviction/rebuild正确，无每Commit全域重建。
- [x] P11-API/P11-SAFETY：rebuild入口、options/errors/readonly/SHOW/EXPLAIN和内部schema防线通过。
- [x] P11-FORMAT：fresh3、legacy reads、1/2→3、rollback/crash/mixed-format history与旧Engine拒绝通过。
- [x] 11.6完成实测热点review；必要优化有收益与完整语义回归，不需要的重构有明确不采用证据。
- [x] P11-STRESS：Search规模与tie-aware recall、10K Merge、Native事务、1/4/8-reader每种并发配置30分钟压力全部通过。
- [x] P11-LATENCY：[固定基线性能目标](../../design/runtime.md#performance-targets)全部目标和6.2结构gate通过，已有Version/write无性能回退。
- [x] 全部applicable compatibility、`cargo make quality`、coverage、真实SQLite/ABI probes与`scripts/ci.sh`通过。
- [x] 当前Phase 11实现提交的Linux x64/arm64、macOS x64/arm64、Windows x64/arm64六平台hosted Release Matrix真实artifact/interop通过；run `35091276800` 的六个平台job与artifact均确认成功。
- [x] Design/README/Development/compatibility/vlog与实际状态一致，证据显式保留旧26GiB migration慢路径与hosted matrix结果，没有隐藏失败样本。
- [x] 最终review无scope内未解决finding；`git diff --check`、link/status/hygiene/secret检查通过，diff无数据库、benchmark report、临时trace或无关改动；hosted matrix已作为独立远端acceptance闭合。

## 8. 完成定义

只有上述项全部有真实通过证据，才将本Phase设为 `done`。功能仍可用、benchmark能跑完、某条raw SQL很快、只在warm单次跑通，都不等于Phase11完成。实现中出现无法从现有Design裁决的合同差异时，先完成最小Design修订并同步受影响acceptance；普通工程细节不反复请求确认，也不能用无期限“继续优化”替代固定交付范围。
