# 部署、安全与性能

[设计入口](../design.md) · [开发状态与验收](../development/README.md)

本文件拥有宿主 SQLite/ABI、部署制品、安全边界及性能不变量与量化目标。性能目标是指定基线的工程验收要求，不是已测结果或跨硬件 SLA；实施状态、测量结果与证据分别见 [开发计划](../development/README.md) 和 [性能研究](../research/phase11-performance-evidence.md)。

<a id="deployment"></a>

## Deployment 与 Runtime Boundary

<a id="implementation-and-sqlite-abi"></a>

### Implementation Language 与 SQLite ABI

Lithograph core 使用 **Rust 2024 Edition**。SQLite loadable-extension boundary 使用 SQLite 官方 `sqlite3ext.h` / `sqlite3_api_routines` host API table，通过 Rust `rusqlite` 的 `loadable_extension` 支持和底层 `libsqlite3-sys` loadable-extension bindings 实现。

Extension artifact **不得**启用 bundled SQLite，也不得直接静态/动态链接一个私有 SQLite 副本来满足 Engine 调用。所有 SQLite API 调用必须解析到加载该 Extension 的 host SQLite API table；这样同一个 artifact 才能被 stock SQLite 以标准 `.load` 机制跨平台加载，并避免两个 SQLite runtime 同时操作同一 `sqlite3*`。

Rust 负责 parser/semantic model、planner、executor、version engine、typed values、storage abstraction、vector index 和 C ABI。允许生成 parser code 或依法复用 grammar，但不允许把外部 parser library 的 AST 作为 storage/executor 公共合同。首个实现依赖基线固定 `rusqlite 0.40.1` + `libsqlite3-sys` loadable-extension path；依赖升级只能在保持本节 ABI 不变量并通过最低 SQLite、当前 SQLite 与跨平台真实 load acceptance 后进行。

<a id="sqlite-baseline"></a>

### SQLite Baseline

支持基线是 SQLite **3.45.0+**，并要求：

- loadable extension support；
- FTS5；
- `sqlite3_get_clientdata()` / `sqlite3_set_clientdata()` connection client-data API，用于 Embedding Provider binding；
- thread-safety 与 transaction behavior 符合 SQLite 官方公开 API。

Lithograph 不静默修改宿主的 journal mode、synchronous level 或其它 durability PRAGMA。

<a id="artifacts"></a>

### Supported Artifacts

发布矩阵：

```text
macOS   arm64, x86_64
Linux   x86_64, aarch64
Windows x86_64, arm64
```

所有平台共享同一 storage format、Cypher Profile 与 test corpus。

<a id="security"></a>

## Security Boundary

Lithograph 是 embedded extension，没有独立 account / role / authentication layer。读取和写入数据库文件、`LOAD CSV` 文件、HTTP(S) 与加载 Extension 的权限都继承宿主进程和 SQLite connection。

Managed Semantic Provider 可能把 source/query String 发送给网络服务或本地模型 runtime；加载和配置该 Provider 等价于 Host 主动授予相应外部处理能力。Lithograph 对 `providerConfig` 保持 provider-opaque：如果调用方直接配置 `api_key`、secret header 或其它敏感值，它们会像其它 config 一样进入 Commit/Schema/history/SHOW/backup，Lithograph 不自动脱敏或阻止；如果配置 `api_key_env`，数据库只保存环境变量名，运行时解析出的 secret value 不进入 SQLite。Provider 也可能把 caller-supplied secret 发送到任意 caller-supplied HTTP endpoint，因此 endpoint/credential trust 由调用方负责。

`graphView` 仍不是 authorization boundary，但 Managed Semantic query 的 on-demand fallback 只对本次 Graph View 中可见的候选 source text 发起 Provider 调用；它不能为了建立全图 HNSW 而在受限 query 中顺带把不可见 owner 的文本发送给外部 Provider。需要预热整个 Index 的 `db.index.semantic.rebuild` 是显式 maintenance surface，不接受 `graphView`，由拥有完整 database execution authority 的调用方执行。

`graphView` 不是 authorization boundary：它只约束一次 execution 的可见 Property Subgraph。能够直接调用 Lithograph 且自行选择 options 的主体可以省略该 option 访问完整 graph；需要强制隔离的上层必须控制 execution surface 与 option construction。

执行 Cypher、初始化、migration、version mutation 和 integrity-maintenance 的 SQL entrypoints 注册为 direct-only surface，不能从持久化 trigger/view/schema expression 隐式触发。纯信息函数只有在确认无副作用后才可注册为 innocuous。

`_lithograph_*` 是内部表。直接 SQL 修改这些表不属于公开 API；由于数据库文件所有者最终拥有 SQLite 全权限，Lithograph 不伪装成能阻止文件所有者篡改，而通过 hash、referential integrity 和 `lithograph_integrity_check()` 检测损坏。

该 integrity contract 证明 database 内仍存在的 canonical evidence 是否自洽，不提供独立于数据库文件的 authenticity / anti-tamper trust anchor。拥有文件全权限的主体若删除全部 Lithograph internal evidence，结果在单文件内部与从未初始化的 pristine database 不可区分；若同时自洽重写全部 canonical data/hash，也不能仅靠同一文件证明其历史真实性。需要这种外部真实性保证的部署必须由宿主另行提供 trusted backup、signature/attestation 或其它外部锚点，Lithograph 不在 embedded database 内伪造该能力。

<a id="large-scale-invariants"></a>

## Large-scale Invariants

以下是架构不变量，不依赖某台机器的绝对 latency：

- 单 hop 邻接扩展使用 source/target/type 可寻址结构，不扫描全部 Relationship；
- equality/range indexed lookup 使用 index seek，不扫描全部 matching domain；
- streaming query memory 与 executor batch / semantic barrier 相关，不与最终 row count 线性增长；
- historical query 从 checkpoint + bounded overlay 解析，不要求从 Root 重放全部 history；
- derived index / checkpoint 可 rebuild，不阻塞 canonical history correctness；
- Managed Semantic 对 query/source 的重复 exact text 必须先做 embedding-space cache lookup + batch 去重；在固定 Provider/config 下，将同一文本复制到 N 个 owner 不得导致 N 次外部 Provider call。cache enabled 且 database 可写时，普通 query 对校验成功的 miss 自动执行短 persistent publish；只读或 cache disabled 时只做 bounded TEMP/memory materialization；
- planner statistics 可以增量刷新，不能要求每个 query 扫描全图计算 cardinality；
- Graph View 不能通过预先 materialize 整个子图实现；scan/seek/expand/search 必须在现有 Snapshot access path 上按需执行 visibility check，且不得因 view 导致本可 seek 的查询退化为无条件全图扫描；
- 10M Node / 100M Relationship benchmark tier 必须作为 release hardening 的真实规模验证，覆盖 traversal、indexed lookup、write、history、diff 与 search；通过条件是正确完成、无 OOM、无意外全图扫描，并建立可持续 regression baseline。
- Merge conflict enumeration 必须 bounded/pageable；大量 conflict 的 start/list/resolve/finalize 不能要求一次把全部 conflict 或完整 candidate materialize 到 caller memory。Open Session 只持久化 pinned inputs + resolution set，candidate/conflict 可以重算或临时 spill。

<a id="performance-evidence"></a>

### Performance Evidence Contract

既有功能/规模验收记录不能替代当前代码的物理访问路径及资源边界证明。单次 wall time、少量结果行、`AdjacencySeek` / `IndexSeek` 名称或 logical `dbHits` 都不能代替该证明。旧 baseline 及其限制见 [性能证据](../research/phase11-performance-evidence.md)。

Benchmark 报告至少保存：Git commit 与 dirty-tree digest、fixture seed/version/真实 cardinality、pinned Commit、数据/索引量与分布、CPU/RAM/OS、Rust/build profile、实际 SQLite version/compile options、journal/synchronous/cache/temp 参数、读取 adapter、并发数、batch 大小、缓存状态与重复次数。不得通过关闭 durability、安全检查或减少 fixture cardinality 获得未标注的“优化”。Fixture 构造与完整 integrity check 单独计时，不混入或静默从被测 query 中移除工作。

每次查询统一从参数/options 解码或 Core prepare 入口计到全部结果被消费/释放，分别报告 prepare、Snapshot resolve、cache lookup/build/overlay、first-row、完整消费和 serialization/callback 成本。Core、Native callback 与 SQL `lithograph_rows` 分别测量，不直接比较不同 adapter 的数字；`MATCH ... RETURN 1` 必须真正消费每行，不能用 `count(*)` 或预知 cardinality 替换。旧 runner 对 streaming query 在 prepare 后开始计时，对 `execute` 则包含 prepare，新的报告必须消除这种口径差异。

缓存实验固定分为四类：同 connection 的 warm read；新 connection/进程但 persistent generation 存在；generation 缺失/被删除后的 fallback 与显式 rebuild；小 delta 后的 ancestor-generation read。它们分别统计，不汇总成一个 P95。OS page cache cold 只有明确完成并记录隔离方法时才能如此命名；仅重开连接仍可能是 OS warm。统计、checkpoint、index readiness 与测试顺序都要记录，不能用 preceding EXPLAIN/查询隐藏预热成本。

性能诊断保存底层 SQL template/绑定条件、`EXPLAIN QUERY PLAN`、公开 `sqlite3_stmt_status` 的 VM_STEP / FULLSCAN_STEP / SORT 等以及 Engine 的 resolved-state 构造次数、Layer 加载数、base/delta owner 检查数、generation build 次数。后者是 internal/test instrumentation，不增加公开 query options 或承诺新的 ABI metrics 字段。FULLSCAN_STEP 为零不能排除错误的宽 index-range scan；必须结合 VM work 和无关数据规模增长实验。SQLite 32-bit statement counters 在溢出前分段采样/reset 并聚合到宽计数，溢出/不可用标为无效证据，不能报告小值通过门禁；可选 scanstatus 不成为 stock runtime 的必需编译选项。

固定 degree/result/相关 delta，将无关图数据增至原来的10倍时，邻接与 ready index path 的实际读取工作不得近似线性增长。工程门禁为 `work(10×unrelated) ≤ 2×work(unrelated) + 1000 VM steps`，汇总一次query的全部相关SQLite statement，配合正确endpoint/type plan和不必要SORT检查；允许B-tree的对数变化，不允许宽主键范围扫描。大结果集 memory 包含 resolver、overlay、visibility、index candidates、row buffers、SQLite cache 与 TEMP spill；报告 process peak RSS、增量 RSS、TEMP/WAL/disk bytes。不能只证明输出256行一批，就声称整个 executor 的内存有界。

<a id="performance-targets"></a>

### 固定基线性能目标

以下是针对 **Apple M2 / 8 CPU / 16 GiB、Release build、同一固定 SQLite runtime 与 10M Node / 100M Relationship fixture** 的工程目标，不是已测结果或跨硬件 SLA。性能优化前固定可重复的 before 基线；该基线下的性能验收须达到下表与结构性门禁。更换机器/runtime 要重建同机 before/after，不能混比；不能在观察优化结果后通过放宽目标或减少数据把失败改成通过。

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

<a id="stress-workloads"></a>

### 扩展压力场景与范围控制

性能验收必须覆盖相互独立的100K与1M Search corpus，不能把“大图里1000个 sample documents”写成百万向量压测。固定并记录 document 长度、vector dimensions/coordinate type、seed、similarity、HNSW build/search 参数、top-k、过滤选择性及历史/staged状态。至少有100K×1536和1M×128维向量场景；不同维度不比较成同一个延迟曲线。另有1M全文文档场景。10M向量可作为容量探索，不是本轮强制范围，也不得在未测前宣传支持该规模的低延迟。

Vector 用确定的 query sample 对 exact top-k oracle 报告 recall@k、分数/排序与 visible filtering；oracle construction 单独计时。若 exact top-k 的第 `k` 名与更多候选在实际 coordinate type / similarity 计算后具有完全相同的 cutoff score，则这些 cutoff tie candidate 在 recall@k 中等价，不能仅因 deterministic ID tie-break 选择了另一组同分结果而判为 miss；仍必须逐项验证返回 score、去重以及稳定的 score/identity 排序。ANN参数/数据相同条件下 recall@10 不得低于优化前，验收最低均值为0.95；不能降低 recall 换延迟。全文结果与相同语义 oracle 比较。报告 index build/加载/查询、cold/warm/new connection、peak RSS/磁盘以及历史 correctness；若这些新增场景暴露架构或 OOM 问题，完成最小必要设计修订后修复，不预先重写 FTS/HNSW。

Mixed-workload 验证覆盖1/4/8个 reader与一个 writer、不同 Graph View、Branch/Tag移动、GC、cache eviction/rebuild；每种并发配置持续压力至少30分钟，记录吞吐、P95、BUSY/retry、失败数、writer持锁、WAL与资源回收。WAL reader pin 不得让返回值跨 Snapshot 漂移；不能为提高吞吐忽略冲突/CAS。10K-conflict Merge 仍分40轮并保持 revision/candidate/finalize语义，先测热点再优化 resolution，不改变公众冲突协议。

性能改动按证据优先：邻接键序 → query-scoped resolved state → persistent Standard Index base/delta → 剩余实测热点。Row slot 化、Search算法重写、更多索引或并行调度不是默认任务；没有 profiling/acceptance 驱动就不增加。具体实施与完成状态由 [性能开发计划](../development/phases/11-performance-optimization.md) 维护；验收以固定集合闭合，不以“所有可能优化都做完”作为无限任务。
