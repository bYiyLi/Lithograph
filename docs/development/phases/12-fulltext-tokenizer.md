# Phase 12：Full-text / FTS5 Tokenizer 扩展

**状态：`ready`**

## 1. 目标与范围

在 Phase 00–11 的已完成基线上，使 Full-text Index 可以引用实际宿主 SQLite connection 中任意符合 FTS5 contract 的 tokenizer 及其参数，不再为每个 tokenizer 修改 Lithograph 映射。实现范围和行为唯一真源是 [Design §11.5](../../design.md#115-full-text)，本计划只安排工作、依赖、验证和真实状态。

本 Phase 包含配置/Schema、原生 tokenizer 委托、query-time analyzer、历史/cache、失败原子性及 SQL/Native integration。允许 Design 已明确的 analyzer binding 破坏升级，不交付旧名称兼容层。Phase 08/11 保持 `done`；它们的历史验收不代替本 Phase 的新验收。

不交付新的 Cypher grammar/config keys、第三方分词算法、大型中文词典依赖、通用 plugin/backend 框架、任意 FTS table options、持久化 FTS cache 改造、Vector/HNSW 或 KG OS 特例。Commit、push、新版本发布与重新运行 hosted Release Matrix 需要各自授权，不作为本计划文件写入动作的附带操作。

## 2. 前置条件与当前差距

- Phase 00–11 `done`；直接依赖 Phase 08 Full-text、Phase 09 Schema/版本入口/Native transaction、Phase 11 Snapshot state/read guard 基础。
- [Design](../../design.md) §3.2、§4.1/4.4、§7.7/7.8、§9、§10、§11.2–11.5、§14、§17；本次不更改 §11.6 Vector 合同。
- [FTS5 研究证据](../../research/fts5-tokenizer-contract.md) S1–S4；[Compatibility inventory](../cypher25-compatibility.md) 的 Phase 12 supplemental inventory。
- 检查基线 `2cbca18`：仍有两个 analyzer 白名单/映射；canonical STRING 和完整 definition cache digest 可复用；DDL 没有 FTS5 constructor probe；query-time override 使用去重词集合近似。

设计、依赖和下文 acceptance 已齐全，因此可以开始实现；**没有 Phase 12 代码完成或测试通过声明**。实施开始时重新检查 HEAD、Git diff、上述设计和源码，不把此检查基线当成未来仓库事实。

## 3. Feature 顺序

```text
12.1 Specification 边界与 synthetic tokenizer 测试底座
 -> 12.2 Versioned configuration / DDL / SHOW
 -> 12.3 FTS cache / historical Snapshot / connection lifecycle
 -> 12.4 Query-time analyzer 原生委托与查询语义
 -> 12.5 Version operation / Native / SQL cross-surface closure
 -> 12.6 Compatibility / quality / 文档收尾
```

每个 Feature 先运行对应 targeted tests，修复其发现的问题再继续；最终在 12.6 统一运行完整门禁。实现暴露的局部缺口先回到 Design 补齐，不能在本计划中另建公共合同。

矩阵中跨 Feature 的条目分阶段累积证据：当前 Feature 只要求其拥有的子场景通过，不以前置 Feature 尚未拥有的 adapter/override 能力制造依赖环；12.6 必须核对每个完整条目，不能把局部证据当作整行完成。下面的 `query/`、`storage/` 路径相对 `crates/lithograph-core/src/`。

### Feature 12.1 Specification 与可执行测试底座

**依赖：前置 Phase；来源：Design §11.5.1–11.5.2、§11.5.6–11.5.7。**

建立 Full-text 局部 specification 校验/SQL literal 编码辅助，去掉对未转义字符串拼 SQL 的依赖。原生委托所需的瞬态解析以 FTS5 原文规则作 oracle，不引入自己的配置语言。

在既有 test-support/SQLite fixture 机制上增加最小 synthetic tokenizer：通过真正的 FTS5 API 注册，支持记录参数与 flags、确定性 token/synonym，以及可控制的 constructor/tokenization 失败。至少验证两个不同注册名可经同一路径使用，不能把 synthetic 名字写入生产 whitelist。允许 test-only 小型 C loadable extension；fixture、头文件和编译脚本不得成为生产词典/分词依赖，源码复用遵守许可规范。

**主要位置**：`query/semantic_index.rs` 的 Full-text 相关职责、必要的局部 helper、`crates/lithograph-test-support`、`tests/`。需要局部分文件时不顺带重构 HNSW。

**Acceptance**：FT12-01、02、03 的 native FTS5 oracle 部分先可执行；参数含空格/引号/空项/Unicode 时得到准确 argument 数组；故意破坏 quoting、顺序、QUERY flags 或失败传播时 fixture 能检测。无实际 tokenizer 注册的 mock 不算通过。

### Feature 12.2 Versioned configuration、DDL 与 SHOW

**依赖：12.1；来源：Design §11.5.2–11.5.4。**

修改当前配置 parser/default、FullText definition 消费者和 SHOW；在实际 Schema 执行边界加入本次改变 definition 的原生构造验证，不在无副作用 planning 或全量 Schema load 中执行。明确 `IF NOT EXISTS` 的静态检查与真实 no-op，不把缺失插件变成无关 schema mutation 的阻塞。

**主要位置**：`query/schema/command/index_config.rs`、`storage/schema_state.rs`、`query/schema/show.rs`、`query/schema/execute.rs`，以及既有 definition equality/Slot 消费路径。

**Acceptance**：FT12-01–06 的 DDL/SHOW 部分；新默认和完整 spec 可创建/展示；原有两个名称不再获得特殊转换；默认省略、类型错误、未知配置、缺失 tokenizer、构造失败和取消均有断言；失败前后比较 Branch head、canonical Commit 数与 TEMP 残留。query options 与版本/adapter 组合分别在 12.4/12.5 关闭。不为已足够的 STRING 模型增加持久化双份配置或 storage migration。

### Feature 12.3 Cache、历史与 connection lifecycle

**依赖：12.2；来源：Design §11.5.3、§11.5.5、§11.5.7。**

将真实已注册 tokenizer 接入当前 TEMP FTS build/rebuild 路径。覆盖完整 definition 与目标 Snapshot key、构建失败清理、DROP/recreate、跨 Branch/historical/staged state，以及每个连接独立注册的边界。

**主要位置**：`query/semantic_index.rs`、现有 `Snapshot::cache_identity` 消费点和 Full-text 相关 integration tests。共享 digest helper 如需调整，必须证明 HNSW key/行为未受影响。

**Acceptance**：FT12-07–11 和 17 的普通 Full-text/cache 部分；同名 index 参数变化不读旧 cache；历史在 cache 缺失和新 connection 上按历史配置重建；未注册时明确失败，注册后可恢复。注入 corpus 中途失败/interrupt 后不能复用半成品；query/rebuild 不新增 canonical Commit 或写 `main`。版本操作与 override 部分分别由 12.5/12.4 完成。

### Feature 12.4 Query-time analyzer 与可见结果

**依赖：12.3；来源：Design §11.5.6。**

替换现有 query-as-document / `DISTINCT term` override 近似，让普通与不同 analyzer 两条路径都遵守 FTS5 tokenization mode 和完整查询结构。按 Design 实现局限于 provider 内的委托适配，不给外部提供第二套 tokenizer 注册机制。

**主要位置**：`query/semantic_index.rs`、`query/completeness/execute/registry.rs`、最小原生 FTS5 adapter；FFI 生命周期验证与既有 workspace lint 一并执行。

**Acceptance**：FT12-12–16 及 17 的 override 部分；新测例先锁定当前近似路径的错误结构，再验证修复。索引分词与查询分词不同必须各自收到正确 flags；短语顺序、AND/OR/NOT、Property 限定、prefix、colocated synonym、score 与 Graph View-before-pagination 均有组合测例。两个并存 cursor/不同 analyzer 调用不得串配置；结束/取消释放 query-owned cache 与 tokenizer。普通 warm query 不为 override 支持新增 corpus 重建。

### Feature 12.5 Version / Adapter 交叉验收

**依赖：12.4；来源：Design §11.5.3–11.5.7 和既有 §4/9/10。**

检查所有会发布 Full-text definition 的入口，不把原生可用性验证只接到 CREATE 的一种 adapter。版本结构读取/纯 ref move 与真正引入新定义的操作按 Design 区分。构建真实 `.load Lithograph + .load synthetic tokenizer` 的集成闭环；也验证两者反向加载顺序。

**主要位置**：`query/version/`、`query/schema/`、Extension Native staged transaction 路径、SQL scalar/rows、现有 real-extension probes。

**Acceptance**：FT12-05、06、09、10、18；Native 多次执行创建/查询/修改在提交前可见，成功只产生最终 Commit；任一配置/执行失败整体 abort。Patch/Merge/Rebase/Revert 中参数变化可观测，失败不发布新 head；纯 ref move/SHOW 无插件仍可工作。Core、SQL scalar、rows 与 Native 对共同支持的读语义一致，rows 拒绝 DDL，不能绕过原有只读边界。

### Feature 12.6 完整门禁与文档收尾

**依赖：12.5；来源：本计划 §4–§6 和全局完成标准。**

将新 suite/probe 纳入真实 CI 和 coverage，不只把测试源码放在仓库里；运行下文门禁，复查全部生产 call sites、失败路径、最终 diff 和本文 acceptance。更新 `docs/guide/search.md`、集成/排障、`docs/reference/limits.md` / `procedures.md` 及必要 CHANGELOG，准确标记新版本或未发布行为，不篡改 v0.1.0 Release Notes。开发路线和 supplemental inventory 只按实际执行结果改状态。

**Acceptance**：FT12-19–20；下表全部通过且具体报告绑定实际 revision/worktree，才能将本 Phase 改为 `done`。本次设计/计划写入不执行这些未来实现步骤。

## 4. Acceptance matrix

每项必须有真实 automated test、命令结果或规定的人工 review 证据；下表当前均为 `planned`，不是测试执行报告。Feature Acceptance 引用本表，公共行为仍以 Design 为准。

| ID | 必须验证的场景 | 验收证据 / 判定 | 状态 |
| --- | --- | --- | --- |
| FT12-01 | 新默认 `unicode61`、`porter unicode61`；旧 `standard-no-stop-words` / `english` | 新行为与直接 FTS5 对照；旧值失败而非映射；显式注册旧名并使用合法内层 quoting 后走通用路径 | `planned` |
| FT12-02 | 任意自定义注册名与 ordered arguments | 至少两个 synthetic 名字成功；exact name/args 验证，生产无测试特例 | `planned` |
| FT12-03 | 嵌套引号、空格、空参数、Unicode、参数顺序、NUL、注入载荷 | 与直接 FTS5 grammar 差分；malformed spec 拒绝；SQL sentinel 表/数据不被载荷修改 | `planned` |
| FT12-04 | 未知 Index OPTIONS/indexConfig/query options；错误类型/null/空 spec | 指定稳定错误；未知 key 不被吞掉或透传任意 table options | `planned` |
| FT12-05 | 未注册、非法 args、xCreate 失败、资源/interrupt、cleanup failure | 新增/改变 Schema 前后 head/Commit 数/Schema 不变；无可复用半成品；错误分类不混淆 | `planned` |
| FT12-06 | EXPLAIN、prepare、SHOW、DROP、真正 no-op、无关图/schema mutation | 无无意 plugin 构造/加载或 corpus build；no-op 无新 Commit；静态未知/类型检查仍执行 | `planned` |
| FT12-07 | Node/Relationship、多 Label/Type、多 Property、String/List/缺失属性 | 继承相应 target/text 提取与去重行为；完整结果及 score 类型正确 | `planned` |
| FT12-08 | cache deletion/rebuild、DROP/recreate 同名异配置 | 结果相同或按新配置正确改变；cache key 隔离；无 canonical write | `planned` |
| FT12-09 | name/args-only definition change、SHOW、Diff/Patch、分支/合并 | 完整字符串 round-trip；单一 Index slot 变化可见；失败发布 rollback | `planned` |
| FT12-10 | 历史 Snapshot 与新 connection/reopen | 冷重建使用历史配置；当下未注册历史 tokenizer 明确失败；注册后恢复；无 current-head 借用 | `planned` |
| FT12-11 | connection A/B 隔离、外部资源、同名实现生命周期 | A 注册不使 B 可用；fixture 版本/资源固定；没有伪造的跨连接 registry 或算法 fingerprint | `planned` |
| FT12-12 | query-time analyzer 省略/相同/不同、缺失/invalid args、空图/空 query/零 limit | 只有查询文本使用 override；不改变 definition 或 Commit；实际调用的空结果路径不掩盖配置错误 | `planned` |
| FT12-13 | phrase、AND/OR/NOT、Property/prefix 与 analyzer override 组合 | 预期结果由固定 corpus 和实际 tokenizer 的独立 oracle 判定；不是词集合近似 | `planned` |
| FT12-14 | QUERY/DOCUMENT/PREFIX/AUX、token offsets、colocated synonyms | synthetic 记录/断言真实回调；callback 错误传播、部分构造和所有成功实例释放 | `planned` |
| FT12-15 | score/tie、Graph View Node/Relationship endpoint、skip/limit | visibility-before-pagination；0/1/多页、不足页、hidden top result 不漏/重；score FLOAT 有限非负 | `planned` |
| FT12-16 | 并存/交错 analyzer、reentrancy、EOF/取消/失败清理 | immutable pair 不串配置；query-owned TEMP/handle 回到基线；不得覆盖宿主注册名 | `planned` |
| FT12-17 | 默认 warm vs cold/rebuild、不同 analyzer 慢路径 | 记录 build/tokenizer 调用次数与耗时/TEMP；普通 warm 不重建；override 成本明确且结束释放 | `planned` |
| FT12-18 | Native staged visibility/abort、SQL scalar/rows/Native、真实双 extension 加载 | 共用 host connection；同一语义一致，失败原子；最低/current SQLite 都实际运行 | `planned` |
| FT12-19 | 既有 Full-text、Vector、版本、Cypher compatibility regression | 对应 suite 与 inherited TCK 无未知 failure/skip；不篡改语言 oracle 来消除 backend 差异 | `planned` |
| FT12-20 | format/compile/clippy/quality/CI、最终 diff 和用户文档 | 全局 gate、相对链接、whitespace、无产物/secret/无关修改；文档区分新行为与 v0.1.0 | `planned` |

## 5. 验证执行与证据记录

拟新增 `crates/lithograph-core/tests/phase12_fulltext_tokenizer.rs` 和 test-support 的 `lithograph-phase12` probe；这些名称是计划目标，当前尚不存在。复用已有 fixture 生命周期，不提交数据库、字典文件或动态库制品。Core/Extension tests 必须保持分开的 Cargo invocation，避免 `rusqlite/loadable_extension` feature unification 污染 standalone SQLite。

实施后的 targeted 与 regression 顺序：

```sh
cargo test --locked -p lithograph-core --test phase12_fulltext_tokenizer
cargo test --locked -p lithograph-core --test phase08_search_ingestion
cargo test --locked -p lithograph-test-support --test phase06_compatibility
cargo test --locked -p lithograph-core -p lithograph-test-support
cargo test --locked -p lithograph-extension
cargo build --locked -p lithograph-extension
cargo fmt --check
cargo clippy --locked --workspace --all-targets --all-features
cargo make quality
scripts/ci.sh
git diff --check
```

真实 integration 还必须执行新增双 extension probe、Native/SQL parity 与最低 SQLite **3.45.0** / 仓库当前冻结 runtime **3.53.4** 的对应 smoke。扩展既有 `scripts/sqlite-345-smoke.sh`、`scripts/sqlite-3534-smoke.sh`、`scripts/ci.sh` 与 quality coverage 的调用链，使新 probe 确实被执行；inventory metadata 输出不能代替 execution report。不提高 SQLite minimum，也不假设 v2 API 总存在。

需要记录实际命令、构建 revision、SQLite/API version、目标平台、测试数、passed/failed/skipped 和注入失败场景。六平台 test/编译接入保持现有工程机制；本地验收不能宣称 hosted matrix 或第三方中文插件已验证。发布前的跨平台 release gate仍归发布流程；出现 FFI/platform finding 时本 Phase 必须修复相应代码与可执行回归，不能用“尚未发布”跳过已知问题。

本次不是性能专项重跑：用小型固定 corpus 证明默认 warm 不重建和 override 有界资源生命周期；仅当本次改动或失败证据影响既有规模合同才扩大性能验证。不因配置优化默认再跑 10M/100M 或修改 Vector benchmark contract。

## 6. Review 与完成标准

Review 必须交叉核对 Design、全部配置消费者、Schema/版本发布入口、FTS callbacks、测试 oracle 和状态文件。重点查找：未转义 SQL、丢失参数边界、旧名称残留特例、runtime 检查误放 prepare、历史 cache 读当前定义、缺插件的静默 fallback、override 破坏表达式、同名注册覆盖、部分构造泄漏、失败移动 head，以及测试只证明注册而未证明真实 query。

每轮发现记录准确场景、位置和修复；重跑受影响的 checks，再继续 review。没有剩余本次范围内的 finding、FT12-01–20 全部有通过证据、文档与实现一致且 final diff 已检查，才完成本 Phase。不以“设计已评审”“可以开始开发”或旧 Phase 的 green report 替代实现验收。

**当前结果：设计与开发计划就绪；实现、targeted tests 和 Phase 12 integration/compatibility gates 均未执行。**
