# Phase 19：查询正确性、取消与执行内存限额

**状态：`done`（本轮 review 修复、开发复验与远端六平台验收完成；v0.3.3 发布准备中）**

## 目标、依赖与设计输入

修复合法反引号名称查询查不到、长表达式收到取消仍继续计算的问题，并增加宿主显式启用的 execution 内存预算。依赖已完成的 [Phase 15](15-sql-execution-provider-cache.md)、[Phase 18](18-query-access-controls.md)；保留 Phase 18 和 v0.3.2 的发布证据，不将已发布二进制视作支持本 Phase 新能力。

唯一设计合同分别由 [Interfaces / Query Options](../../design/interfaces.md#query-options)、[Query Engine / 名称与执行](../../design/query-engine.md#engine-structure)、[Cancellation](../../design/query-engine.md#cancellation-and-connection-state)、[Execution Resource State](../../design/query-engine.md#execution-resources)、[Runtime / 中断恢复](../../design/runtime.md#sqlite-interrupt-recovery) 和 [内存边界](../../design/runtime.md#execution-memory-boundary) 拥有。本计划只维护工作依赖、验收和真实状态。

范围仅为 Lithograph；保持 `CY25-2026.08`、storage format 3、Commit identity、`EmbeddingProviderV1` 与标准 SQLite extension 边界。不修改 Loreweft，不新增 Rust AST bridge、独立执行进程、全局 allocator 或依赖。本轮已获提交与推送授权；正式发布和 Loreweft 接入不属于本轮交付。

## 本轮 review 状态

本轮 review 发现的问题已修复，专项回归、最终 quality / 本机 CI、release-build SQL 与文档检查均已完成。当前验收证据使用 `target/phase19-review/`，不沿用旧 green；本轮目标范围内的复审已闭环，下文单独保留 transaction batch 的既有实现边界。实现 revision 已提交并推送至 `origin/main`，其 repository CI 与六平台 Release Matrix 全部成功，精确 SHA 与 job 证据见[本轮远端验收](#phase19-remote-acceptance)。随后授权的 v0.3.3 发布见[发布交付记录](#v033-release-delivery)；Loreweft 接入不属于本轮交付。

下文 `target/phase19/` 的日志、构建 checksum、数字和源码 review 结论保留为**上一轮历史证据**，仅对应当时工作树；当前制品与复验结果由本轮新证据证明。Feature 与 QR 的当前复验状态另列，不把旧 green 迁移为新结果。

### 本轮修复与复验

本轮冻结源码的最终 quality、本机 CI 与 release-build SQL 验证均已通过，覆盖以下 review 修复：

- Temporal 固定 offset 的合法形式与 typed JSON 往返保持一致；非法 Unicode offset 返回受控错误，避免切片 panic。`round()` mode 与 temporal `truncate()` unit 校验不再创建长字符串副本，`toLower()` 预留覆盖实际输出 capacity。
- Vector 校验、Graph View membership 与索引 proof 的长遍历使用当前 execution 的中断检查，避免只在整段遍历前后检查。
- scoped globals 恢复、dynamic key、Path binding 转换和 conditional 结果复制在分配前预留；返回值与输出 batch 持有自己的 lease，cursor 取消或 drop 不提前释放仍存活结果的预算。
- 已进入 Ready 的 sort/distinct spill 在取消及重复 cleanup 时保留 `INTERRUPTED` / SQLite code 9，避免清理错误覆盖真实取消。
- 索引 maintenance、cold/warm candidate 与 runtime workspace 使用统一受控路径，预留必要的同时存活工作缓冲，并保留搜索与索引结果语义。
- lineage、staged overlay 与 checkpoint cache 的容量在扩容前预留；生产 Property visitor 随输出值转移 lease，Snapshot drop 后保留仍存活值的计账。
- connection-local TEMP guard 与宿主同名 object 不兼容时，在 graph write / explicit BEGIN 前拒绝，保留宿主 object 与业务数据。

对应永久回归位于 [Temporal offset](../../../crates/lithograph-core/tests/phase19_temporal_offsets.rs)、[Temporal format](../../../crates/lithograph-core/src/query/functions/temporal_format_tests.rs)、[global bindings](../../../crates/lithograph-core/src/query/completeness/execute/global_bindings_tests.rs)、[expression](../../../crates/lithograph-core/src/query/expression/evaluate_tests.rs)、[stream resources](../../../crates/lithograph-core/src/query/stream/resources_tests.rs)、[index cache](../../../crates/lithograph-core/src/query/schema/index/cache/tests.rs)、[Snapshot read](../../../crates/lithograph-core/src/storage/snapshot_read/tests.rs) 与 [真实 SQLite SQL 探针](../../../tests/phase19_execution_resources.c)。当前结果如下。

### 本轮完整门禁证据

| 本轮门禁 | 实际结果 | 本机记录（均位于 `target/phase19-review/`） |
| --- | --- | --- |
| `cargo make quality` | **exit 0，622.28 s**；最终 format、workspace Clippy、rustdoc、复杂度、重复率、依赖与 supply-chain、coverage 和集成任务完成 | `quality.log` |
| Production coverage | TOTAL **regions 84.92% / functions 86.55% / lines 88.13%**；最低 per-file line coverage 为 `query/stream/project.rs` 的 **51.38%**，高于原 **50%** 下限；没有新增文件豁免、`allow` / `expect` 或阈值调整 | `quality.log` 末尾 coverage report |
| 最终 Core / Extension | Core lib **282/282**；Extension 独立 invocation **20/20**；Phase 19 index / mutation / names / program / resources / temporal / version 七组 **42/42**；早期 targeted-core3 的 279 只为定向历史 | `quality.log` |
| 最终 inherited TCK | **3897 total / 3777 applicable / 3777 passed / 0 failed / 120 not applicable**；`not_applicable_reasons` 与上一轮报告完全相等，没有改变原分类或分母 | `tck.json`、`quality.log` |
| 本机 `scripts/ci.sh` | **exit 0**；Core / Extension 独立 invocation，SQLite **3.45.0 / 3.51.0 / 3.53.4**、SQL / transaction / access、Provider SPI / dual-extension、ABI、compatibility / TCK inventory 全部完成 | `ci.log` |
| 文档与真实示例 | **80 Markdown** 链接/锚点/coverage、**528 Cypher examples × 2 surfaces = 1056 次执行**、**138 SQL statements**、172 function signatures / 38 Procedures、Python 应用和 C lifecycle 全部通过；最终状态回填另做 scoped 文档检查 | `ci.log` |

<a id="phase19-remote-acceptance"></a>

### 本轮远端验收

2026-10-18，已提交并推送至 `origin/main` 的实现 revision 为 [`d7602c24b7c1bc399a6d12fdefb9f610c62c2e07`](https://github.com/bYiyLi/Lithograph/commit/d7602c24b7c1bc399a6d12fdefb9f610c62c2e07)。该精确 SHA 的 [repository CI `39136521139`](https://github.com/bYiyLi/Lithograph/actions/runs/39136521139) 为 `completed / success`，quality、Linux/macOS canonical gates 与 Windows load/ABI gates 共 **4/4 jobs 成功**。[Release Matrix `39136521126`](https://github.com/bYiyLi/Lithograph/actions/runs/39136521126) 同为 `completed / success`；前置 [storage-fixture](https://github.com/bYiyLi/Lithograph/actions/runs/39136521126/job/117702023075) 与下列 **6/6 平台 jobs 全部成功**。各平台均完成 Engine / Provider release build、版本与访问控制回归、真实 load / ABI / runtime / interoperability gates 和打包。

| 平台 | 实际结果 | 直接证据 |
| --- | --- | --- |
| Linux x64 | `completed / success` | [job `117703701960`](https://github.com/bYiyLi/Lithograph/actions/runs/39136521126/job/117703701960) |
| Linux arm64 | `completed / success` | [job `117703701878`](https://github.com/bYiyLi/Lithograph/actions/runs/39136521126/job/117703701878) |
| macOS x64 | `completed / success` | [job `117703701941`](https://github.com/bYiyLi/Lithograph/actions/runs/39136521126/job/117703701941) |
| macOS arm64 | `completed / success` | [job `117703701872`](https://github.com/bYiyLi/Lithograph/actions/runs/39136521126/job/117703701872) |
| Windows x64 | `completed / success` | [job `117703701901`](https://github.com/bYiyLi/Lithograph/actions/runs/39136521126/job/117703701901) |
| Windows arm64 | `completed / success` | [job `117703701887`](https://github.com/bYiyLi/Lithograph/actions/runs/39136521126/job/117703701887) |

macOS x64 的完成日志 `target/phase19-review/macos-x64-completed.log` 另确认 Phase 19 C 探针在 SQLite **3.45.0 / 3.53.4** 各 **8/8**、共 **16/16 cases passed**；两套 runtime 共 10 次真实取消均返回 code 9，发出 interrupt 后最大延迟 **0.600 ms**，最终 cross-platform storage fixture 为 `status: ok`。这些数值只属于该 macOS x64 job。

原始核验记录为 `target/phase19-review/ci-followup.json` 与 `matrix-followup.json`。这两个 run 仅证明上述实现 SHA；本次仅回填验收记录，后续文档 commit 的 SHA 与其交付状态单独记录，不把实现 run 归给文档 SHA。本轮运行来自 `main`，没有创建 tag，[publish-release](https://github.com/bYiyLi/Lithograph/actions/runs/39136521126/job/117709438604) 按条件 `skipped`；该实现提交验收时新能力仍为 **Unreleased**，随后授权的版本发布另见下文。

### 本轮 release-build 验证证据

以下结果来自本轮冻结工作树的本机 macOS arm64 构建。`release-build.log` 为 exit 0，Cargo 报告 **1m 35s**；Engine 为 `target/release/liblithograph.dylib`，SHA-256 **`7f673a6c7fafd76536555476bd12b7e2ae14277e438d9812b66047a7feaf93ed`**，与 `target/phase19-review/release-sha256.txt` 及重新计算一致。这里的 release build 是本机优化构建，不代表 tag 或正式发布。

| 本轮外层验证 | 实际结果 | 本机记录（均位于 `target/phase19-review/`） |
| --- | --- | --- |
| C SQL：SQLite 3.45.0 / 3.51.0 / 3.53.4 | 每套 names / memory / rollback / batches / stream / spill / lifecycle / cancel **8/8**，共 **24/24**，均 exit 0；包含新 TEMP guard collision 的写前拒绝检查 | `release-345.log`、`release-current.log`、`release-3534.log` |
| Python SQL：SQLite 3.51.0 | 固定 **256 MiB** 增量消费 **1,000,000** 个 Map/String rows，同规模 scalar collector 超预算拒绝；option、参数、cursor 隔离、rollback 与取消通过，exit 0 | `release-python.log` |
| Python 真实取消 | scalar / rows 发出 interrupt 后均约 **0.1 ms** 返回；原始日志精度为 **0.0001 s**，不解释成更高精度测量 | `release-python.log` |
| Transaction batch 预算边界 | 最新构建相同 query：8 MiB 时 code 18、0 row / 0 summary、仅新增 1 个 durable Commit，数据为 `[[1]]`；省略预算时 code 0、5,001 rows / 1 summary、新增 2 个 Commit，数据为 `[[1],[2]]`；两者均恢复 autocommit，exit 0 | `release-batch-boundary.log` |

三个真实 SQLite runtime 每套五次嵌套表达式取消均为 code 9，均低于本机 500 ms 目标；原始发出 interrupt 后的延迟如下：

| SQLite runtime | 五次延迟（ms） |
| --- | --- |
| 3.45.0 | 0.237 / 0.493 / 0.154 / 0.153 / 0.092 |
| 3.51.0 | 0.109 / 0.230 / 0.114 / 0.082 / 0.222 |
| 3.53.4 | 0.167 / 0.289 / 0.207 / 0.081 / 0.136 |

上述 C 日志逐 case 记录 process RSS high-water、初始化 baseline、差额、SQLite memory high-water 与 host TEMP；这些是整个独立 case 进程的观测，不是单 query RSS 硬上限，host TEMP 也不包含执行器私有 spill connection。旧 coverage 和旧 Engine checksum 仍只属于后文历史证据；本轮 coverage 与最终门禁使用上表最新结果。

### 当前 review 确认的既有边界

Transaction subquery 的 inner batch 目前仍先形成完整 `RowSet`，durable commit 后再写入输出 spill；这是 HEAD 既有路径，并非本轮引入。显式预算使同时存活的 batch 结果受控：超限返回 `RESOURCE_ERROR`，当前 batch 回滚，earlier durable batch 保留；省略预算时保留旧行为，内存仍可能随单个 batch 的结果量增长。当前实现不能据此宣称已实现 [Storage / Transaction Subqueries](../../design/storage.md#transaction-subqueries) 要求的 batch 内 provisional rows 自动 spill；该设计合同保持不变，也不能把提高预算描述为 RSS 硬限制。

本轮 crossreview 曾对上一轮未发布 release build 进行合成 probe；该旧结果只保留为历史。上表 `release-batch-boundary.log` 已使用本轮最新构建复验相同 query，明确证明预算拒绝、当前 batch 回滚、earlier durable 保留及恢复 autocommit 的边界；它不证明 batch 内 provisional rows 已自动 spill。

## 上一轮开发验收证据（历史）

以下结果来自 **2026-10-18 上一轮的本机 macOS arm64 未提交工作树**，基于 HEAD `dea918d03d5c44c9945dfdc8aae7b239e3a85363`。这是上一轮开发验收，不代表本轮修复后结果、新 commit、远端 CI、六平台 Release Matrix 或正式发布；v0.3.2 发行库仍不接受 `memoryLimitBytes`。

| 最终门禁 | 实际结果 | 本机记录 |
| --- | --- | --- |
| `cargo make quality` | **exit 0，566.72 s**；format、workspace Clippy、rustdoc、复杂度、重复率、unused dependency、supply-chain、独立 Core / Extension、TCK、真实 SQL / Phase 19 / Provider 门禁全部通过 | `target/phase19/quality-delivery.log` |
| Production coverage | TOTAL **regions 84.71% / functions 86.30% / lines 87.86%**；最低 per-file line coverage 为 `query/stream/project.rs` 的 **51.82%**，高于原 **50%** 下限，没有文件豁免 | 同上末尾 coverage report |
| 本机 `scripts/ci.sh` | **exit 0**；Core / Extension 分开 invocation，SQLite **3.45.0 / 3.51.0 / 3.53.4** 适用真实 load / SQL / transaction / Provider SPI / 双扩展与 Phase 19 gates 全部通过；compatibility self-check、inventory 与 TCK inventory 完成 | `target/phase19/ci-delivery.log` |
| 最终 inherited TCK | **3897 total / 3777 applicable / 3777 passed / 0 failed / 120 not applicable**，原分类和分母不变；包含最后的 Path 错误优先级修复，完整 quality/CI 亦再次通过 | `target/phase19/tck-verified.json`、`tck-verified.stderr` 与最终两份门禁日志 |
| 文档与真实示例 | **80 Markdown** 链接/锚点/coverage、**528 Cypher examples × 2 surfaces = 1056 次执行**、**138 SQL**、172 function signatures / 38 Procedures、Python 应用与 C lifecycle 全部通过 | `target/phase19/ci-delivery.log` |

最终优化构建 **46.52 s，exit 0**，包含 bound metadata 与普通/已删除 Path 修复。Engine 为 `target/release/liblithograph.dylib`，SHA-256 **`fcb5cf41aa698d2378d247e2f833d3d9fc4c0610c8cf0efa54a2b45805dbdbca`**；重新计算一致。构建记录为 `target/phase19/release-build-delivery.log`。

| 最终 release-build 外层验收 | 实际结果 | 本机记录 |
| --- | --- | --- |
| C SQL：SQLite 3.45.0 / 3.51.0 / 3.53.4 | 每套 names / memory / rollback / batches / stream / spill / lifecycle / cancel **8/8**，共 **24/24**，三轮均 exit 0；各 runtime 使用同源严格编译的 SQLite harness，每个 case 独立进程加载上述最终 Engine | `target/phase19/release-345-delivery.log`、`release-current-delivery.log`、`release-3534-delivery.log` |
| Python SQL：SQLite 3.51.0 | 固定 **256 MiB** 完成 **1,000,000** 个 Map/String 结果，同规模 scalar collector 资源拒绝；option、range 16/256 MiB、参数、cursor 隔离、rollback 与取消通过，exit 0；scalar / rows 取消延迟 **2.7 / 4.0 ms** | `target/phase19/release-python-delivery.log` |
| Metadata SQL：SQLite 3.51.0 | scalar / rows × 省略预算 / 64 MiB，**4/4 组**；deleted Relationship type、Path `TYPE_ERROR`、Branch head 与关系 Property 错误后的 rollback 均通过，exit 0 | `target/phase19/release-metadata-delivery.log` |
| Metadata 与相邻回归 | Phase 05 **71/71**、Phase 18 interpreter access **8/8**、Phase 19 resources **9/9**，共 **88/88**；删除后 `type()` / `elementId()`、普通与已删除 Path 的类型错误、Property 的 `Semantic` 错误与 rollback 保留 | `target/phase19/metadata-verified.log` |
| 其它资源与性能专项 | Phase 19 五组 **35/35**、相邻 Phase 18 classification **5/5**；Phase 07 **38/38，1.87 s**；最终 Core lib **235/235，2.48 s**，随后最终 quality/CI 再次全套验证 | `target/phase19/targeted-core8.log` 的通过 targets、`core-lib-final.log` 与最终门禁；targeted-core8 整体首次 fixture 失败见历史记录 |

完整门禁的执行入口如下；`scripts/ci.sh` 保持 Core 和 loadable-extension ABI 的独立 invocation，不能改成 workspace 统一测试：

```sh
cargo make quality
scripts/ci.sh

cargo test --locked -p lithograph-core -p lithograph-test-support
cargo test --locked -p lithograph-extension
cargo run --locked --quiet -p lithograph-test-support --bin lithograph-phase10-tck
```

外层 release SQL 回归入口为 [C 探针](../../../tests/phase19_execution_resources.c)、[独立进程驱动](../../../scripts/phase19-execution-resources.sh) 和 [Python SQL 回归](../../../tests/phase19_resources_sql.py)：

```sh
scripts/phase19-execution-resources.sh target/release/liblithograph.dylib
python3 -B tests/phase19_resources_sql.py target/release/liblithograph.dylib
```

SQLite 3.45.0 / 3.53.4 最终回验逐 case 使用已按同一最终 C 源码严格编译链接的各自 amalgamation harness；未重复编译无变化的 SQLite object。永久 Core 回归为 [名称](../../../crates/lithograph-core/tests/phase19_names.rs)、[资源](../../../crates/lithograph-core/tests/phase19_resources.rs)、[program](../../../crates/lithograph-core/tests/phase19_program_resources.rs)、[mutation](../../../crates/lithograph-core/tests/phase19_mutation_resources.rs)、[version/candidate](../../../crates/lithograph-core/tests/phase19_version_resources.rs) 与 [删除后 metadata](../../../crates/lithograph-core/tests/phase05_mutation/regressions.rs)；定向结果和完整门禁共同证明成功、错误、取消、回收与原子性。

## Feature 顺序与完成状态

| Feature | 工作与交付 | 依赖 | 状态 |
| --- | --- | --- | --- |
| F19-01 名称正确性 | 先记录旧行为；统一静态名称解码，覆盖 Label/Type、predicate、scan/seek、mutation/Schema/Index | 15、18 | `done`；本轮 names、TCK、quality / CI 与三套 release SQL 复验 |
| F19-02 表达式取消 | 先记录旧行为；cursor 资源状态向表达式/函数传中断检查，接入失败/关闭清理 | F19-01 | `done`；本轮长值/遍历、spill cleanup 与真实 interrupt 复验 |
| F19-03 执行内存预算 | 先记录旧行为；options/参数解码、运行时预留、值/缓冲/结果计账、TEMP flush、隔离/回收 | F19-02 | `done`；本轮 lease / workspace / capacity 与外层预算复验；既有 batch 边界见上文 |

## 验收矩阵

以下验收保留原要求，证据对应本轮 `target/phase19-review/` 的冻结源码与 release build；修复前结果仍由历史基线提供。内存预算与拒绝的本轮验收不把上文既有 batch 内 provisional spill 差距计作新增交付。

| ID | 必须验证的结果 | 状态与直接证据 |
| --- | --- | --- |
| QR19-01 | 三个已知问题都有永久回归与修复前结果；设计、执行选项、错误和版本边界一致 | `done`；历史旧制品红测、本轮 Core / C / Python 回归与文档门禁 |
| QR19-02 | 中文/英文 Label、Relationship Type、WHERE Label predicate 的普通与反引号形式同值；名称内双反引号、空格、标点、关键字准确匹配，动态字符串不二次解码 | `done`；本轮 names 7/7、三个 runtime 的 names case 与 TCK 3777/3777 |
| QR19-03 | 普通/复杂 Pattern、索引路径、历史 Snapshot、Graph View、CREATE/MATCH/Schema/Index，以及 scalar/rows 的名称语义一致 | `done`；本轮 names、受影响 Phase 04–07、scalar/rows SQL 与完整 quality / CI |
| QR19-04 | 可控回调证明表达式/循环检查间隔至多 256 work units，长文本分块至多 64 KiB；range、嵌套 reduce、allReduce、列表/flatten、长值和字符串路径均覆盖 | `done`；本轮 expression / functions / storage / encoder、Vector / Graph View / proof 与 Core lib 282/282 |
| QR19-05 | 真实 SQLite 执行中 interrupt 返回 code 9、无成功 summary，连接可恢复；本机原嵌套 reduce 复现以发中断后 500 ms 内退出为目标 | `done`；本轮三套 release C 共 15 次 code 9，Python scalar / rows、guard collision 与事务/参数恢复 |
| QR19-06 | memoryLimitBytes 的合法边界和所有错误类型/越界拒绝；省略兼容，scalar/rows/explicit execution 及其它 options 组合正确 | `done`；本轮 Core options、Extension 20/20、release C / Python 与 explicit transaction gates |
| QR19-07 | size(range(1,500000)) 在 16 MiB 预算下于大分配前失败，256 MiB 下正确返回；参数解码、嵌套列表、字符串增长、复制和编码副本无法绕过 | `done`；本轮 Core / Extension 预留断言、三个 runtime 的 memory case 与 Python |
| QR19-08 | 参数/值/row/batch、排序/聚合/去重、结果编码/scalar collector 的同时存活内存计账；预算溢出明确拒绝，semantic barrier 按字节提前 spill | `done`；本轮 lease / capacity / workspace、复制/编码与既有 sort/distinct/aggregation spill 回归；transaction batch 的超限拒绝通过，batch 内 provisional 自动 spill 的既有差距见上文 |
| QR19-09 | 固定预算逐行消费百万行不按累计输出增长；同规模 scalar collector 超预算失败；不同连接/cursor 独立，嵌套/分支/concurrent batch 共享一次预算 | `done`；本轮固定 256 MiB 百万行 Python、C stream/lifecycle、Core 独立与并发共享预算回归 |
| QR19-10 | 成功/error/interrupt/early-close 后预留与 TEMP 释放；普通写不留未发布 Commit/Branch move，explicit transaction 整体 abort，earlier durable batch 保留 | `done`；本轮 Phase 19 七组 42/42、Core / Extension、三套 C rollback/batches/lifecycle/cancel 与最新 batch-boundary SQL |
| QR19-11 | 独立 release-build SQL 探针记录真实 SQLite、取消延迟、进程峰值、SQLite 内存和 TEMP 使用；不把预算宣称为 RSS 硬上限 | `done`；本轮三套 release C 原始测量、最新 Engine checksum 与上文口径 |
| QR19-12 | targeted fixtures、适用 compatibility/TCK、cargo make quality、scripts/ci.sh、SQLite 3.45.0 与当前 runtime 真实 load/SQL gates 完整通过；Core/Extension 独立 invocation | `done`；本轮 quality / CI 均 exit 0、TCK 3777/3777、三套 release SQL 24/24 |
| QR19-13 | 分配入口/clone/collect/扩容/JSON 编解码/中断传播与清理路径 review findings 闭环；必要重验、文档链接/空白和最终 diff 检查通过 | `done`；本轮 review 修复与永久回归、完整 quality / CI、80 文件文档门禁与最终 scoped links / diff / 新文件空白检查；既有 batch 边界保持透明 |

## 上一轮取消与内存测量（历史）

最终三个实际 runtime 各五次嵌套表达式取消均返回 code 9，全部低于本机 500 ms 目标；每个 cancel case 另验证大参数解码中断及普通、active explicit、caller-owned transaction 的实际恢复，不产生成功 summary。确定性检查间隔是跨环境的主要正确性依据。

| SQLite runtime | 五次发出中断后的延迟（ms） | 本机记录 |
| --- | --- | --- |
| 3.45.0 | 0.151 / 0.297 / 0.164 / 0.133 / 0.068 | `target/phase19/release-345-delivery.log` |
| 3.51.0 | 0.119 / 0.250 / 0.085 / 0.110 / 0.121 | `target/phase19/release-current-delivery.log` |
| 3.53.4 | 0.107 / 0.174 / 0.301 / 0.297 / 0.155 | `target/phase19/release-3534-delivery.log` |

C 测量每个 case 独立进程的 RSS high-water、初始化后 baseline high-water、两者差额、SQLite memory high-water 和 `hostTempBytes`，不是把 case 内每条 query 分开测得的峰值。例如 memory case 同时包含低预算拒绝和高预算成功，不能把其峰值归因于被拒绝的单条查询。以下为最终 SQLite 3.51.0 原始 bytes，其余值见各 runtime delivery 日志：

| release C case | Process peak | Peak 减 baseline peak | SQLite peak | Host TEMP |
| --- | --- | --- | --- | --- |
| memory | 124141568 | 108232704 | 448272 | 20480 |
| stream | 17219584 | 1277952 | 448272 | 20480 |
| spill | 23363584 | 7503872 | 3110624 | 20480 |
| cancel | 287490048 | 271581184 | 134709360 | 24576 |

cancel case 包含 **128 MiB** 合成参数输入；宿主已有输入和 SQLite 自身内存不在引擎预算内，上述值不是 RSS 上限。`hostTempBytes` 只含宿主连接可观察的 TEMP 页面，不涵盖执行器私有 spill connection。Core `byte_budget_spill_records_private_temp_and_early_close_cleans_output` 实际记录私有 sort TEMP **logical bytes = 159744**，early-close 后 scratch/output table 数 **0**、资源计账 **0**；该 logical page 观测也不是进程全部 TEMP 峰值。结果在 `target/phase19/targeted-core8.log` 和最终完整 quality/CI 中通过。

## 修复前结果与上一轮 review 闭环（历史）

修复前使用正式 v0.3.2 macOS arm64 Engine `target/release-032/unpacked/macos-arm64/lithograph.dylib`，SHA-256 为 `e82855ae001f08e4a2c2a0d71636c53039dc29ae9f3344265f12fe287305670c`，与正式制品审计及重新计算一致；宿主 SQLite 3.51.0，全部数据库为合成临时 fixture。

| 基线问题 | 修复前实际结果 | 本机记录 |
| --- | --- | --- |
| 反引号名称 | 创建两个人物和一条关系后，首个 ``MATCH (n:`人物`) RETURN count(n)`` scalar 结果断言失败，exit 1 | `target/phase19/baseline/names.log` |
| 内存选项发布边界 | range 500000 加 16 MiB 新选项返回旧库 `INVALID_ARGUMENT` / code 1（unknown option），而新回归要求资源拒绝 code 18，exit 1；不将它冒充 RSS 测量 | `target/phase19/baseline/memory.log` |
| 嵌套 reduce 取消 | 发出真实 interrupt 后 **5901.587 ms** 才 code 9，超过 500 ms，exit 1；另一轮 **7152.294 ms** 的旧 exit 0 仅表示最终收到中断，当时未默认检查延迟，不能作为及时性通过 | `target/phase19/baseline/cancel-threshold.log`、`cancel.log` |

分配、复制、编码、callback 与清理路径的独立 review finding 均已修复并补回归；最终源码 review 无未闭环问题。真实 SQLite interrupt 恢复的 TEMP guard / 公开 BLOB API、实际 autocommit 确认，以及 caller-owned whole-transaction rollback 边界由 Runtime 唯一拥有，并由最终 SQL 验证。实施期间以下旧失败与中断记录保留为历史，不作为最终 green：

- `quality-complete.log` / `ci-final.log` 暴露 staged Layer 每次重扫累计 delta 的平方级回退，旧运行主动 SIGTERM。unique 增量更新 / shared 受控 COW 修复后，原两条约 5,000 节点 seed 所在 Phase 07 **38/38，1.87 s**；新增线性 work-count、COW 取消原子性与 sparse-tree 预留回归通过。sample 为 `phase07-staged-regression-sample.txt`；`targeted-core8.log` 首次 lib 的旧 32 MiB fixture 预算失败由 256 MiB fixture 与 `core-lib-final.log` **235/235** 重验闭环。
- `quality-accepted.log` / `ci-accepted.log` 的 Full-text 取消 fixture 固定 callback 次数失效；改按实际 FTS `_docsize` 达到 8 锁存，断言部分工作 **8..64**，原 root/ready/retry 断言保留，`fulltext-final.log` **11/11**，最终全套再次通过；无生产修复。
- `ci-verified.log` / `tck-final-attempt.json` 曾为 **3776/3777**，唯一 Return2 [14] 是删除后 `type(r)` 回退；`quality-verified.log` 因生产修复主动终止。受控 bound metadata、普通/已删除 Path 错误优先级与 Property 错误/rollback 已修复，最终 `metadata-verified.log` **88/88**、`tck-verified.json` **3777/3777**；较早补 Path 前 `tck-final.json` 不冒充最终证据。
- 早期 release C 误把 no-active `tx_commit` 断言成 code 1，修为原合同 `SQLITE_MISUSE` (21)；Python helper 保留 binding 的 `InterfaceError` 分类。旧 `release-sql-*-final.log` 的失败保留；此前 `accepted` 制品与测量只证明 metadata 修复前功能，最终一律使用 `delivery` Engine、日志与 checksum。

## 上一轮 diff 与当前交付边界

上一轮独立只读 hygiene 审查覆盖 **132 tracked 修改 + 78 untracked 新文件 = 210 文件**；新增 74 Rust、1 Phase 19 文档、1 驱动、2 SQL 探针，均为 UTF-8 text，0 trailing whitespace、0 binary，无 temporary database、build/log 产物或 secret。Cargo dependency/lock、Provider crate/header、Phase 18 历史与 release 文档未改；storage format 3、Commit identity 与 SPI entry signatures 保持；CHANGELOG 的 v0.3.2 及以下历史正文逐字保留。`allow` / `expect` 从 47 降至 44，没有新增豁免；quality 增加两个本 Phase gate，未降低阈值。当时文档重新读取、80 文件链接/锚点/coverage、`git diff --check` 与新文件 trailing-whitespace 检查通过；这些文件数与历史 diff 结论不代表本轮新增修改的统计。

本 Phase 的本轮修改、复审修复、验证与文档同步已完成；实现 revision 已提交并推送至 `origin/main`，repository CI 与六平台 Matrix 的精确 SHA 证据见[本轮远端验收](#phase19-remote-acceptance)，本机与远端证据各自保留来源。Lithograph 新能力的 v0.3.3 正式发布状态见下文[交付记录](#v033-release-delivery)；Loreweft 仍需升级依赖并由宿主显式传入 `memoryLimitBytes`，其接入与完整下游验收不计入本轮完成结果。最终文档按当前结果重读，并检查 scoped local links / anchors / coverage、`git diff --check` 和新文件 trailing whitespace；当前制品使用本轮 checksum，旧记录保留历史身份。

<a id="v033-release-delivery"></a>

## v0.3.3 发布交付记录

用户于 2026-10-18 授权发布新版本。本轮将 workspace 与内部精确版本依赖同步为 **0.3.3**，保持 storage format 3、Commit identity 和 Provider SPI V1；旧 v0.3.2 及 Phase 18 的发布记录不变。

发布准备包含 CHANGELOG、[v0.3.3 Release Notes](../../releases/v0.3.3.md)、README、安装路径与同版 Guide / Cypher / Reference。实现提交的已有 green 仅作开发基线；版本提交的精确 SHA 必须先通过 CI 与六平台 Matrix，再创建 annotated tag，等待 tag CI / Matrix 与 publish job 成功，随后下载正式资产核对 checksum、成员、版本和架构，并在本机 macOS arm64 实际回验。此处当前仅记录准备状态，尚未创建 v0.3.3 tag 或发布资产；后续结果按真实动作回填。Loreweft 升级接入不在本次发布范围。

版本准备本机检查使用 `target/release-033/`：`--locked --offline` metadata 确认 6/6 workspace packages 均为 0.3.3，外部 dependency/lock entries 未变；release build **51.08 s、exit 0**，真实加载版本为 0.3.3、Profile 与 format 范围不变；SQLite 3.51.0 的 Phase 19 C 八组 **8/8** 和 Python 固定预算百万行/取消/生命周期检查均 exit 0，运行时 inventory **172 signatures / 38 Procedures** 与文档一致；131 份 Markdown 的链接、锚点、coverage 与 `git diff --check` 通过。版本提交及后续远端结果仍按各自精确 SHA 记录。
