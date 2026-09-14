# Phase 10：Compatibility Closure and Release Hardening

**状态：`in_progress`**

## 1. 目标

关闭全部 compatibility、correctness、scale、recovery、migration、cross-platform 和 release gap，形成首个可以对外宣称完整的 Lithograph release。

## 2. 依赖

- Phase 00–09 全部 `done`。

## 3. Design Inputs

- 完整 `docs/design.md`；
- `docs/development/cypher25-compatibility.md`；
- 所有 Phase acceptance record。

## 4. Features

### Feature 10.1 Compatibility closure

- applicable openCypher TCK failures 清零；
- `CY25-2026.08` matrix `planned/partial` 清零；
- built-in function/procedure inventory 自动核对；
- parser accepted-but-unimplemented path 清零；
- unexplained skip/expected-failure 清零；
- SQL Bridge / Native shared query result parity。

### Feature 10.2 Fuzz / property tests

- lexer/parser fuzz；
- AST round/pretty where applicable；
- typed value encode/decode property tests；
- delta canonicalization/hash property tests；
- diff/patch algebra property tests；
- snapshot/checkpoint equivalence property tests。

### Feature 10.3 Crash and recovery

Fault injection points：

```text
before layer write
after layer / before commit
after commit / before branch move
after branch move / before SQLite commit
before/after Commit Data sidecar write
before/after Tag ref create/move/delete
explicit transaction after begin / after staged execute / before final layer / after commit row / before branch move / explicit abort / connection teardown cleanup
Merge Session start expected-head race / after start / resolve read-to-writer revision race / after resolution batch / candidate read-revision race / finalize prepare-to-writer race / before finalize branch CAS / after merge commit before session delete / stale abort / abort cleanup
checkpoint/index rebuild
migration steps
```

每个 crash fixture reopen 后只允许合法 pre/post transaction state。

### Feature 10.4 Storage migration

- previous-format fixture；
- format `1 -> 2` forward migration，验证 Commit Data / Tag / Merge Session operational storage 建立且既有 Commit ID/history 不变；
- migration rollback；
- too-new format rejection；
- Commit ID/history preservation；
- corrupted hash refusal。

### Feature 10.5 Scale benchmark

Release tier：

```text
10,000,000 Nodes
100,000,000 Relationships
```

工作负载：

- label/type scan；
- indexed equality/range seek；
- high/low-degree one-hop traversal；
- variable path；
- write batch + commit；
- multi-execution Native explicit transaction + single Commit；
- large-conflict Merge Session：bounded conflict pagination、multi-round resolution、candidate inspection、read-phase finalize preparation、short-writer single finalize；
- historical query；
- cursor-based large Commit-DAG traversal；
- Tag lookup / GC-root reachability；
- Commit Data get/set/clear；
- branch diff；
- full-text；
- vector SEARCH；
- checkpoint rebuild。

Gate：正确完成、无 OOM、无 unintended full graph scan、streaming memory invariant 成立，并保存机器/SQLite/version/workload baseline 供后续 regression 对比。

### Feature 10.6 Cross-platform release

矩阵：

```text
macOS arm64/x86_64
Linux x86_64/aarch64
Windows x86_64/arm64
```

每个 artifact：

- build；
- symbol/ABI check；
- real SQLite `.load`；
- `lithograph_init`；
- read/write/history/Tag/Commit-Data/explicit-Commit/explicit-transaction/Merge-Session smoke；
- storage-format interoperability fixture。

同一平台 artifact 额外在支持矩阵中的 SQLite **3.45.0 minimum** 与当前稳定 SQLite runtime 各执行一次 load/init/read/write smoke，证明 loadable-extension ABI 不依赖构建机私有 SQLite。

### Feature 10.7 Final architecture/security review

Review：

- no SQLite fork/private API；
- no second mutable source of truth；
- no hidden Cypher dialect/compatibility waiver；
- direct-only side-effect surfaces；
- LOAD CSV authority boundary；
- no secrets/local fixtures in package；
- dependency license/security audit；
- public error/result/ABI stable；
- Graph View 没有被实现成自定义 Cypher dialect、result post-filter 或可绕过的 adapter-only filter；
- Commit Data / Tag 保持 sidecar 边界：不改变 immutable Commit hash/Snapshot，Tag 不自动移动且参与 GC root；
- paginated history cursor pin immutable start Commit，不因 Branch/Tag 后续移动漂移；
- Native explicit transaction 保持 connection-scoped ABI、multi-execution -> one Commit、`expectedHead` CAS、cross-execution Graph View staged visibility、`LOAD CSV` boundary、fail-closed abort/connection teardown 与短 single-writer boundary；caller-owned SQLite transaction / Cypher transaction batching 没有被错误折叠成同一语义；
- Merge Session 保持 durable operational workspace 而不是第二套 history：不跨 conflict-resolution / finalize preparation 生命周期持有 writer，conflict bounded/pageable，start expected-head、resolve/finalize/abort revision CAS 正确，revision 精确绑定 candidate inspection/finalize，open Session 参与 GC root，只有 finalize 的短 writer phase 才能 Commit/移动 Branch；

### Feature 10.8 Documentation closure

同步：

- README 用户介绍/安装/quickstart；
- Design 与真实 architecture；
- compatibility final report；
- release build/install usage docs；
- Phase 状态与 acceptance evidence。

## 4.1 Current Implementation / Evidence

Phase 10 已开始 hardening，但尚未达到 release closure：

- `phase10_hardening` 使用 deterministic generated/property fixtures 覆盖 parser fail-closed、recursive Lithograph JSON value round-trip、Layer canonicalization/hash permutation、`layer_between` replay equivalence、public diff/patch forward+inverse algebra，以及 checkpoint materialization 与 Snapshot equivalence；当前 **6/6** 通过。
- `phase10_compatibility` 新增 release-level compatibility regression：mutating / Version Procedure `EXPLAIN` 完成 validation+planning 但不产生 graph/ref side effect；`PROFILE` 与普通 execution 返回相同 rows 并记录 rows/dbHits/time foundation；SQL Bridge scalar summary 与 Native `SUMMARY` event 通过共享 serializer 暴露相同 `metrics.rows/dbHits/elapsedMicros`，补齐已有 Core metrics 在 public adapter boundary 被丢弃的问题；parse/semantic/type error 保留 source position，constraint error 保持稳定分类，SQLite `BUSY/LOCKED` 在 Core public taxonomy 与 Extension adapter 都稳定映射为 `BUSY` 并保留 primary SQLite code；当前 core compatibility **3/3**，extension summary-shape regression 已覆盖 aggregate metrics。`PROFILE` 的 per-operator runtime counters 与最终 oracle/TCK closure 仍未完成，因此 compatibility rows 不提前标 `done`。
- `phase10_recovery` 已覆盖同一 writer transaction 在未 Commit 时 close/reopen 必须回到合法 pre-state，Commit 后 close/reopen 必须保留 Commit/ref、Commit Data、Tag、checkpoint 并保持 integrity clean，以及 uncommitted Commit Data clear + Tag move/create/delete 组合在 teardown/reopen 后完整恢复 pre-state；本轮进一步加入真实 subprocess death + WAL recovery 边界：writer 在 SQLite COMMIT 前直接结束进程时，reopen 必须恢复 Branch/history/Tag/allocator pre-state；COMMIT 成功后立即结束进程时，reopen 必须保留 Commit/ref、Commit Data、Tag、checkpoint 并保持 integrity clean。当前 test binary **6/6** 通过（其中 1 个 test 是仅供父测试调用的 crash subprocess harness）。
- 既有 real-extension format `1 -> 2` probe 已扩展为有 descendant history 的 format-1 fixture：migration 前后 frozen Root Commit、descendant Commit ID、两级 Snapshot 与 parent DAG edge 均保持不变，同时继续覆盖 migration failure rollback；Native/real-load matrix 仍覆盖 SQLite 3.45.0 minimum / 3.51.0 current runtime 与 Phase 09 restart/session/transaction behavior。这些 evidence 会继续扩展为 Phase 10 release migration/fault matrix，而不是复制第二套迁移实现。
- repository-wide `cargo make quality` 与 canonical `scripts/ci.sh` 在当前 Phase 10 worktree 上均通过；当前 coverage lines/functions/regions 为 **83.91% / 83.65% / 81.72%**。
- 尚未关闭的 Phase 10 scope 包括 `CY25-2026.08` / applicable TCK compatibility closure（尤其 PROFILE per-operator counters、EXPLAIN/error oracle 与 SQL Bridge/Native parity 的最终 release matrix）、完整 crash fault matrix、release migration matrix、10M/100M scale gate、cross-platform artifact matrix、final architecture/security review 与 documentation/release closure。因此本 Phase 保持 `in_progress`，以下 Release Acceptance 不提前勾选。

## 5. Release Acceptance

- [ ] Phase 00–09 全部 `done`；
- [ ] `CY25-2026.08` unresolved/partial/skipped = 0；
- [ ] openCypher applicable TCK failure = 0；
- [ ] Graph View cross-surface fixture 覆盖 scan/seek/path/subquery/aggregation/write/full-text/vector/historical read：read/search/historical result 与对应 Snapshot 的物理诱导子图 oracle 一致，read-write query 与逐 clause graph-state oracle 一致，Schema/Constraint 仍按完整 canonical graph 验证，且不存在 visibility/write bypass；
- [ ] fuzz/property suite 无未解决 correctness finding；
- [ ] crash/recovery/migration suite 全通过；
- [ ] format `1 -> 2` migration 保持全部既有 Commit ID、Snapshot 与 history semantics，Commit Data/Tag/Merge Session storage 的 crash/reopen 与 rollback 行为正确；
- [ ] Tag、Commit Data、explicit empty-delta Commit 与 cursor-based DAG History 的 Phase 09 acceptance 在 release matrix 中回归通过；
- [ ] Native explicit transaction 的 C ABI/ownership、multi-execution single-Commit、expected-head mismatch、cross-execution Graph View、`LOAD CSV` rejection、execute/callback/cancel/commit/abort/connection-teardown rollback、transaction/statement clock 与 staged-isolation acceptance 在 release matrix 中回归通过；
- [ ] Merge Session 的 restart recovery、large-conflict pagination、incremental resolution、start expected-head、revision/cursor stale detection、candidate read-only inspection、read-prepare + short-writer finalize、target-head CAS、stale-abort protection、single finalize 与 GC-root acceptance 在 release matrix 中回归通过；
- [ ] 10M/100M benchmark gate 通过；
- [ ] 所有 release artifacts real-load acceptance 通过；
- [ ] cross-platform storage fixture interoperable；
- [ ] final review finding 闭环；
- [ ] docs 与 release behavior 一致；
- [ ] final diff 无 temporary artifact/secret/unrelated change。

## 6. 完成条件

只有 Release Acceptance 全部满足，才允许把 Lithograph 首个完整版本描述为：

> stock SQLite loadable extension，完整实现 `CY25-2026.08` current-graph Profile，并提供基于 immutable Commit DAG、Branch/Tag 与结构化版本操作的 Versioned Property Graph。
