# Lithograph Project Instructions

## Language

- 项目设计、开发计划、开发记录和 README 使用简体中文。
- Source-code identifier、Cypher keyword、SQLite API、ABI symbol、error code、CLI command 和配置 key 保持原始英文。
- Agent-facing instruction 在英文能显著减少歧义时可以使用英文。

## Product Invariants

实现不得改变以下产品边界：

- Lithograph 是标准 SQLite loadable extension，不 Fork SQLite，不引入独立 Server/Daemon。
- 一个 SQLite database 对应一个 Versioned Property Graph；版本通过 Commit / Branch 表达，不复制数据库文件模拟 Branch。
- Query engine 的兼容目标是 `docs/design/compatibility.md` 定义的 Cypher 25 Profile；不得创建 Lithograph query dialect 代替已有 Cypher semantics。
- Versioning 是 storage foundation。所有 graph/schema/index write 从 Root Commit 起进入 immutable history，不先实现 mutable-only storage 再补 history。
- Canonical history 由 immutable Commit / Layer / Schema object 构成；Branch ref 可变；checkpoint、statistics 和 physical search/index content 是可重建 derived data。
- GraphQLite、TerminusDB、Git、Neo4j/Cypher 和 SQLite 是 evidence/reference，不是自动依赖，也不能覆盖 `docs/design.md` 入口与 `docs/design/` 专题已确认的 Lithograph contract。
- 默认参考外部项目的公开 behavior、tests 和 architecture，不直接复制源码。任何源码级复用必须先检查该版本许可证与 attribution/NOTICE 要求，并把必要 notices 随代码保留；不能为了省实现时间引入许可证不兼容。

## Repository Truth

判断仓库、设计、实现、Git、测试、运行环境和历史事实时，先检查当前 repository 和真实命令结果，不使用历史聊天替代仓库事实。

真源职责：

- `README.md`：普通用户产品介绍与入门入口。
- `docs/design.md`：设计入口、产品定义、全局边界与设计职责表；与职责表列出的 `docs/design/` 专题共同构成唯一设计真源。
- `docs/design/`：按职责分别拥有接口、查询、存储、版本、Schema/Search 与 runtime 的完整合同；跨域规则链接到拥有该合同的专题，不复制正文。
- `docs/research/`：外部研究和参考证据；研究不是产品设计。
- `docs/development/README.md`：开发路线、Phase 状态与全局完成标准。
- `docs/development/cypher25-compatibility.md`：Cypher Profile inventory 与兼容验收状态。
- `docs/development/phases/`：每个 Phase 的 scope、Feature、依赖和 acceptance。
- `AGENTS.md`：仓库级执行规则。

不要把同一个 design decision 复制到 development 文档作为第二真源。Development 文档引用 design section，并只描述实现顺序、验收和真实状态。

## Read Before Change

修改行为前必须：

1. 先从 `docs/design.md` 的职责表定位并读取拥有该行为的设计正文及其必要依赖；
2. 读取当前 Phase 文档；
3. 检查相关代码、测试和 Git 状态；
4. 对 Cypher compatibility 变更读取 `docs/development/cypher25-compatibility.md`；
5. 外部事实会改变实现判断时重新检查 authoritative source，不从旧研究快照猜测当前行为。

如果当前实现请求暴露出 design 未覆盖的行为，先依据已确认产品目标、reference evidence 和现有 invariants 完成最小必要 design 修订，再继续实现。只有在无法从这些来源裁决、且不同答案会实质改变产品 contract 时才需要用户决定。

## Autonomous Execution

对明确授权的创建、修改、修复或 Phase 实现请求，持续执行到该请求的完成条件满足：

- 不停在 capability 说明、方案、scaffold、局部 Feature 或“是否继续”的询问；
- 常规实现细节从 design、当前代码、tests、references 和最小方案原则自行判断；
- 在提问前先完成全部不依赖该问题、且已授权的检查、实现和验证，使真正需要决定的内容已经具体可审阅；
- 可逆、只读、review、test 和请求已经授权的 repository write 不增加额外批准步骤；
- commit、push、发布、破坏性数据操作和外部生产副作用仍按其独立授权边界执行，不从一般实现请求推导。

失败后先检查实际状态。只重试确认未执行、可安全重试且仍在授权范围内的动作；不重复可能已经成功且会产生重复副作用的动作。

## Design Discipline

默认实现满足当前 Phase 和最终 design contract 的最小方案。

新增抽象、兼容层、配置、extension point、依赖或大范围重构前必须能回答：

> 它解决了哪个当前已存在的 design requirement、Phase acceptance 或已验证 correctness/performance 问题？

无法指出具体来源就不增加。

不得因为“未来可能需要”“更通用”“更优雅”提前建立第二套 storage abstraction、query dialect、plugin system 或 distributed architecture。

## Cypher Compatibility Rules

- `CY25-2026.08` 的 observable semantics 是 acceptance contract，不以 parser 能接受语法作为完成依据。
- 每个新增 Cypher feature 必须覆盖 positive、negative/error、type/null 和与相邻 clause/operator 组合的 semantic tests。
- openCypher TCK 是 inherited baseline；Cypher 25 新能力按 compatibility matrix 增加 fixtures。
- 与 Neo4j oracle 结果不一致时先判断是否属于 `docs/design/compatibility.md` 定义的 current-graph Profile；属于 Profile 就修复实现或明确修正 design，不能把差异静默标为 expected failure。
- Compatibility matrix 中的 scenario 只有真实自动化测试通过后才能标记 `done`。
- 不通过加入 alias、special case 或 hidden compatibility mode 掩盖 parser/planner/storage 的结构性错误。

## Storage and Versioning Invariants

实现 storage/version code 时必须保持：

- committed NodeId / RelationshipId 不复用；
- Commit、Layer、Schema object immutable；
- Merge Commit layer 相对 first parent 表达，second parent 仅记录 DAG lineage；
- Branch head move 与对应 graph/schema write 在同一 SQLite transaction；
- historical query pin immutable Commit；
- direct mutation `_lithograph_*` internal tables 不是 engine 内部捷径，所有 mutation 经 storage/version API；
- Lithograph-owned checkpoint、statistics、range/text/full-text/HNSW 等 derived state 删除后可以从 canonical history/当前 versioned definition 重建；
- Lithograph-owned derived state 不得成为 correctness source；Embedding Provider 自己的独立 result cache 不属于 Lithograph storage/canonical-history invariant；
- storage migration 不改变既有 Commit ID 或 history semantics；
- 不占用宿主应用的 `PRAGMA user_version`。

违反以上任一项属于 architecture regression，不以测试暂时通过为理由接受。

## Phase Delivery

Feature 是实现单元，Phase 是默认交付单元。

执行一个 Phase 时：

1. 确认 Phase scope、依赖、design inputs 和 acceptance；
2. 按 Feature dependency 连续实现完整 vertical slice；
3. 先运行 targeted verification，再运行 Phase 要求的 integration/compatibility gates；
4. 做 Phase-level review，修复 scope 内 correctness、compatibility、storage-integrity、security 和 performance findings；
5. 检查最终 diff，删除临时诊断、generated junk、secret 和无关改动；
6. 同步 Phase、compatibility matrix 和必要 design 文档到真实状态；
7. 只有全部 acceptance 满足才将 Phase 标记 `done`。

代码写完、测试一部分通过、单个 Feature 完成或 scaffold 存在都不等于 Phase 完成。

## Verification

验证范围与改动相称，但必须直接证明受影响 contract：

- Rust：format、compile、clippy、unit/integration tests；
- SQLite Extension：真实 `.load`、init、application-facing SQL execution surface smoke；涉及 Embedding Provider 时另验证 Provider SPI / dual-extension load；
- Cypher：targeted fixtures + applicable compatibility suite；
- Storage/Version：transaction rollback、hash/integrity、snapshot、branch、merge/conflict、migration fixtures；
- Search：result semantics、historical snapshot consistency 与 rebuild/fallback；
- Platform/Release Phase：cross-build 和真实目标平台 load acceptance；
- 文档：重新读取修改目标、检查 local links、`git diff --check`，并检查 untracked 新文件的 trailing whitespace。

通过必要验证后，只有出现新改动、新失败或具体未解决 finding 才扩大/重复测试；不要为了“更彻底”无限增加无关 gate。

## Phase Completion Evidence

Phase 标记 `done` 前必须存在：

- 完整实现，不以 mock 替代核心路径；
- Phase acceptance matrix 全部通过；
- 成功路径和关键失败/rollback/conflict 路径验证；
- Phase-level review findings 闭环；
- `docs/development/cypher25-compatibility.md` 与实际兼容状态一致；
- development 文档状态与仓库一致；
- final diff review 完成。

Commit 和 push 状态单独记录；没有实际执行就不能声称已提交或已推送。

## Safety and Repository Hygiene

- 不提交 secrets、credentials、local database、benchmark dataset、temporary trace、build artifact 或 crash dump。
- 保留用户已有且与本任务无关的修改。
- `LOAD CSV`、filesystem/network fixtures 使用 synthetic test data，不把真实私有路径或 credential 固化进测试。
- Fuzz/crash test 使用 disposable database，不对真实用户 database 运行破坏性 fixture。
- 内部表损坏测试只在临时 fixture database 执行。

## Instruction Placement

根 `AGENTS.md` 只放 repository-wide rules。某个 subsystem 出现只有该目录需要的长期规则时，才在更深目录增加 `AGENTS.md`；不要复制根规则。
