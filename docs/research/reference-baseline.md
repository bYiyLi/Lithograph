# Lithograph 外部参考基线

**研究快照日期：2026-09-10**

本文保存 Lithograph 设计使用的外部证据。它不是产品设计真源；最终设计以 [设计入口及其职责表列出的专题](../design.md#design-ownership) 为准。

## 1. SQLite

来源：

- <https://www.sqlite.org/loadext.html>
- <https://www.sqlite.org/vtab.html>
- <https://www.sqlite.org/appfunc.html>
- <https://www.sqlite.org/c3ref/get_clientdata.html>
- <https://www.sqlite.org/releaselog/3_44_0.html>

观察：

- SQLite loadable extension 可以注册 application-defined SQL functions、virtual tables、collations 与 VFS；
- eponymous-only virtual table 可以作为 table-valued function；
- loadable extension 不会替换 stock SQLite parser，因此不能仅靠 `.load` 为 SQLite 增加新的顶层 Cypher grammar；
- SQLite application-defined function callback 可以调用其它 SQLite interfaces，但不能关闭 connection，也不能 finalize/reset 当前正在执行它的 statement；Lithograph 因此仍用真实 integration test 验证同 connection SQL Bridge recursion 与 mutation boundary；
- `sqlite3_get_clientdata()` / `sqlite3_set_clientdata()` 从 SQLite 3.44.0 起提供 connection-owned wrapper state 与 close-time destructor；Lithograph 的 3.45.0+ baseline 因此可以用它保存 Native explicit transaction state，而不需要 process-global mutable transaction registry；
- 这决定 Lithograph 使用 SQL function / table-valued function 作为 raw SQLite bridge，同时在同一个 shared library 中提供 Native C ABI。

## 2. GraphQLite

来源：

- <https://github.com/colliery-io/graphqlite>
- <https://github.com/colliery-io/graphqlite/releases>

观察：

- GraphQLite 是 SQLite extension，并提供 Cypher、Python、Rust 与 raw SQL interface；
- v0.6.0 于 2026-06-04 发布；
- 项目报告 openCypher TCK 3,876 scenarios 中 97.7% passing；
- 仓库当前使用 MIT License；
- 其工程证明了 SQLite extension、Property Graph、Cypher parser/execution、TCK-driven compatibility 与 bindings 的组合可行。

Lithograph 采用：

- SQLite extension bridge 的实现经验；
- openCypher grammar / parser 测试经验；
- TCK-driven compatibility 方法；
- SQLite 图编码、parameter binding、error propagation 与 binding packaging 经验。

Lithograph 不继承：

- openCypher 作为语言上限；
- 97.7% coverage 作为完成标准；
- clause dispatcher / Cypher-to-SQL transpilation 作为唯一执行架构；
- 任何会阻止 Cypher 25、Versioned Graph 或大规模执行目标的现有实现限制。

代码复用不是默认动作。实现优先复用公开行为、测试方法和架构经验；若直接复制/修改外部源码，必须在当次变更中检查许可证、保留要求和 NOTICE/attribution，并确保与 Lithograph 最终发布许可兼容。

openCypher 官方 specification / grammar / TCK 仓库使用 Apache License 2.0。Phase 00 可以固定并 vendor TCK test data，但必须保留对应 LICENSE/NOTICE。

## 3. TerminusDB

来源：

- <https://terminusdb.org/docs/terminusdb-explanation/>
- <https://terminusdb.org/docs/version-control-operations/>
- <https://terminusdb.org/docs/openapi/>
- <https://terminusdb.org/docs/version-controlled-json/>
- <https://github.com/terminusdb/terminusdb-store>

观察：

- TerminusDB 将数据更新记录为 immutable commit / delta layer；
- Branch 是轻量版本引用；
- 支持 commit log、time-travel、diff、patch、merge、rebase、squash、reset 等 Git-like data operations；
- Diff 是结构化数据变化，而不是文本 diff；
- terminusdb-store 是版本化 graph storage reference，但数据模型是 triple/document graph，不是 Lithograph 的 Property Graph。

Lithograph 采用：

- immutable delta layer；
- immutable commit history + mutable branch ref；
- snapshot/time-travel；
- structural diff / patch；
- branch / three-way merge / conflict；
- rebase / squash；
- checkpoint / derived materialization 思路。

Lithograph 不复制 TerminusDB 的 RDF/triple/document schema 或 WOQL query model。

## 4. Git

来源：

- <https://git-scm.com/docs/git-merge>
- <https://git-scm.com/docs/git-merge-base>
- <https://git-scm.com/docs/git-rebase>
- <https://git-scm.com/docs/gitfaq>

观察：

- 常规 branch merge 使用 merge-base + two heads 的 three-way merge；
- 一对 Commit 可能存在多个同等 best merge bases；Git merge strategy 会为 merge 合成 common-ancestor tree，而不能假设任取一个 best base 与完整 merge 等价；
- rebase 的核心是确定当前 Branch 上需要移植的 Commit 集合，再按顺序逐个 replay 到新的 base tip；因此 replay-range boundary 与一次 two-head merge 的 virtual merge base 是不同问题；
- merge commit 可以记录两个 parent；
- ref 与 immutable commit history 分离，使 branch reset 和历史保留成为自然操作。

Lithograph 把 three-way merge 从“文本行”改成 Property Graph logical slot：element existence、label membership、property、schema/index object。

## 5. Cypher 25

来源：

- <https://neo4j.com/docs/cypher-manual/current/>
- <https://neo4j.com/docs/cypher-manual/current/deprecations-additions-removals-compatibility/>
- <https://neo4j.com/docs/cypher-manual/current/functions/graph/>
- <https://neo4j.com/docs/cypher-manual/current/appendix/gql-conformance/supported-optional/>
- <https://neo4j.com/docs/cypher-manual/25/clauses/clause-composition/>
- <https://neo4j.com/docs/cypher-manual/current/subqueries/subqueries-in-transactions/>
- <https://neo4j.com/docs/operations-manual/current/scalability/composite-databases/querying-composite-databases/>
- <https://neo4j.com/docs/cypher-manual/current/clauses/search/>
- <https://neo4j.com/docs/cypher-manual/current/schema/graph-types/>
- <https://feedback.neo4j.com/changelog/neo4j-aura-august-2026-release>

观察：

- Cypher 25 从 Neo4j 2025.06 开始，并持续增加功能；
- 2026.02 后新数据库明确使用 Cypher 25；
- Graph Types 在 2026.02 引入并在 2026.06 GA；
- `VECTOR` 在 2025.10 引入；
- `SEARCH` 在 2026.01 引入并用于 ANN vector search；
- 2026.08 Aura release 增加 native UUID 与 string interpolation。
- Cypher 25 支持 GQL optional feature GQ01 `USE` graph clause，但 Neo4j 当前 graph references 面向 database / composite-database constituent graph；`graph.byName()` 也明确用于 Composite Database 的 constituent graph selection。
- 因此 `USE` 提供“选择一个 graph 后执行 query”的标准语义证据，但不能直接表达 Lithograph 当前“同一个 SQLite database / Versioned Property Graph 内按 Label visibility 选择 query-local induced subgraph”的需求。Lithograph 不重定义 `USE`，而把 Graph View 放在 execution options / Engine boundary。
- Cypher clause composition 把 graph state 作为 clause 之间的输入/输出：一个 clause 观察全部前序 clause writes，不能观察后序 clause writes。Graph View 因此固定 selector 而不能固定 query-start element membership；visibility 必须作用于每个 clause 实际接收的 graph state。
- `CALL { ... } IN TRANSACTIONS` 把 subquery batch 放进独立 inner transactions 并产生中间 commits；ordered semantics 下后续执行可观察前序 writes。Lithograph 因此让同一个 Graph View selector 传播到 batch，但每个 batch 基于自身 pinned Commit 重新计算 visibility。

因此 Lithograph 使用冻结 Profile `CY25-2026.08` 判断首个完整兼容版本，后续 Cypher 25 additions 通过新的 Profile 升级。

## 6. OpenAI Model Guidance

来源：

- <https://developers.openai.com/api/docs/guides/latest-model>

与 Lithograph `AGENTS.md` 相关的 guidance：

- 对已经授权的工作要持续执行到用户目标完成，不停在 capability、plan 或 partial result；
- 在提问前先完成上下文已经授权、可自行检查和可形成具体结果的工作；
- 对可逆、只读或已明确授权的动作不要增加无依据的批准步骤；
- `AGENTS.md`、skills 与其它 model-visible instructions 会显著影响执行，应避免冲突和含糊规则；
- 测试范围应与改动相称，完成必要验证后不因“更彻底”无限扩大范围。

这些规则只用于开发协作行为，不进入 Lithograph 数据库产品语义。

## 7. Rust Toolchain

来源：

- <https://blog.rust-lang.org/releases/latest/>

2026-09-03 Rust 官方发布 1.98.1，修复 1.98.0 的 vtable miscompilation。Lithograph Phase 00 因此冻结 Rust `1.98.1` + Edition 2024 作为首个实现 toolchain，而不是使用未 pin 的 `stable` channel。

## 8. Rusqlite Loadable Extension Boundary

来源：

- <https://github.com/rusqlite/rusqlite/blob/master/Cargo.toml>
- <https://github.com/rusqlite/rusqlite/blob/master/examples/loadable_extension.rs>
- <https://github.com/rusqlite/rusqlite/blob/master/libsqlite3-sys/Cargo.toml>

2026-09-09 仓库基线：

- `rusqlite 0.40.1`，Rust 2024 Edition，MIT License；
- `loadable_extension` feature 明确转发到 `libsqlite3-sys/loadable_extension`；
- 官方 example 的 extension entrypoint 接收 SQLite 提供的 `sqlite3_api_routines*`，并通过 `Connection::extension_init2(...)` 初始化；
- `bundled` 是另一条显式 feature，会构建私有 SQLite，不符合 Lithograph “stock SQLite loadable extension” 边界。

Lithograph 因此固定：Extension artifact 使用 `loadable_extension` host API-table 路线，禁止 `bundled` SQLite。这样 Rust Engine 操作的 `sqlite3*` 与加载 Extension 的 host SQLite 属于同一 runtime，避免跨平台 symbol/allocator/runtime 混用。
