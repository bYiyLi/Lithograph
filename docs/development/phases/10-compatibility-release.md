# Phase 10：Compatibility Closure and Release Hardening

**状态：`done`**

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

Phase 10 的 compatibility、recovery、scale、release-artifact、cross-platform matrix 与 final review 已全部闭合：

- `phase10_hardening` 使用 deterministic generated/property fixtures 覆盖 parser fail-closed、recursive Lithograph JSON value round-trip、Layer canonicalization/hash permutation、`layer_between` replay equivalence、public diff/patch forward+inverse algebra，以及 checkpoint materialization 与 Snapshot equivalence；当前 **6/6** 通过。
- `phase10_compatibility` 当前 **4/4** 通过：mutating / Version Procedure `EXPLAIN` 完成 validation + logical/physical planning 且不产生 graph/ref side effect；`PROFILE` 与普通 execution 返回相同 rows，并通过 shared serializer 暴露 query-level 与 per-operator `rows` / `dbHits` / time counters；parse/semantic/type/constraint/BUSY taxonomy 与 source position/SQLite primary code 保持稳定。当前 executable inherited openCypher runner 执行 **3,897** 个 scenarios，其中 **3,777/3,777 applicable passed、0 failed**，其余 120 个全部有 machine-readable Cypher-25 supersession 或 product-boundary reason；`CY25-2026.08` matrix 已无 `planned/partial` family。
- `phase10_recovery` 当前 **10/10** 通过，覆盖 process-death pre/post SQLite COMMIT、Branch CAS fault、checkpoint rebuild fault、Commit Data/Tag rollback/reopen，以及 Merge Session abort/finalize fault rollback；所有 reopen 结果只出现合法 pre/post transaction state。
- real-extension format `1 -> 2` probe 当前 **6/6** 通过：descendant format-1 history migration 保留 frozen Root/descendant Commit ID、Snapshot 与 parent DAG edge，并覆盖 migration failure rollback、finalize fault rollback、corrupt immutable history refusal、too-new rejection 与 Merge Session restart/adapters。
- Release runtime gate 固定为 SQLite **3.45.0 minimum + 3.53.4 current**。commit `5386ddd` 的 GitHub Actions Release Matrix run `34953984529` 已成功完成 shared storage fixture、Linux x64/arm64、macOS x64/arm64、Windows x64/arm64 全部六目标：每个目标构建真实 release artifact，执行 Phase 09 release-mode version regressions、real SQLite `.load`、Phase 01–09 extension probes、artifact/symbol/ABI inspection、Native C ABI、SQLite runtime smoke 与同一个 Linux-generated storage-format interoperability fixture verification；六个目标 artifact 均成功产出。Windows Phase 09 regression 使用 smoke 中按固定 checksum 构建的 SQLite 3.45.0 link/runtime environment，避免依赖 runner 私有 SQLite。对应 CI run `34953984492` 同时通过 repository-wide quality、Linux/macOS/Windows Phase 01 gates。macOS arm64 本地 release closure 另外验证过 SQLite 3.51.0 runtime。
- Scale hardening 已关闭本轮暴露的 release blocker：Label scan 复用已知存在/Label membership，checkpoint property page 改为 set-based 读取，standard-index TEMP cache 只建立当前 Index kind 需要的 secondary indexes，first-parent Diff 使用 touched Layer 而不是 materialize 整图，empty-delta Commit 不再解析无变化的大图 Snapshot，mutation schema validation 只对 Layer 实际影响的 Graph Type/Constraint domain 执行原有 canonical validation。上述优化都保持 checkpoint/index 为 derived data、Commit/Layer/Schema immutable、Branch CAS 与完整 constraint semantics 不变。
- 10M Node / 100M Relationship canonical fixture 与 derived checkpoint 已真实生成并完成完整 release workload：checkpoint 包含 **10,000,000 Nodes、100,000,000 Relationships、10,002,000 Properties**，fixture database 约 26GB；label scan、indexed equality/range seek、low/high-degree one-hop、variable path、full-text、vector SEARCH、historical query、Tag、Commit Data、branch Diff、cursor History 与 1,000-Node write/single-Commit 全部正确完成，无 OOM 或 unintended full-graph write validation。最终 baseline 写入本地 generated artifact `target/phase10-scale-release/baseline.json`；该文件不进入仓库真源。
- 最终 worktree 重新通过 10,000-conflict Merge Session：256/page 共 40 页、40 轮 incremental resolution、candidate inspection、single finalize 与 Tag GC-root preservation；同一 worktree 也在 26GB database 上通过 Native explicit transaction 的两次 staged execution + staged read + exactly-one final Commit。
- repository-wide `cargo make quality` 已通过，包括 format、all-target/all-feature Clippy、rustdoc、生产代码复杂度、dependency audit、duplication gate、full tests、coverage 与 3,777/3,777 applicable TCK；`scripts/ci.sh` 已通过真实 SQLite `.load`、SQLite 3.45.0 minimum、Phase 01–09 probes、Native ABI 与 artifact inspection。最终 architecture/security diff review 未发现 task-affecting finding：没有 SQLite fork/private API、第二 mutable truth、ABI/Extension surface 或新依赖变更；Graph View 仍在 Snapshot access path 执行；standard-index cache 仍为可删除重建的 TEMP derived data；Commit Data/Tag、Native transaction 与 Merge Session 边界保持既有合同。
- Release Acceptance 已全部满足，Phase-level review 没有剩余 task-affecting finding；Phase 10 状态正式关闭为 `done`。该状态表示开发与 release acceptance 完成，不等同于已经执行对外 Release 发布。

## 5. Release Acceptance

- [x] Phase 00–09 全部 `done`；
- [x] `CY25-2026.08` unresolved/partial/skipped = 0；
- [x] openCypher applicable TCK failure = 0；
- [x] Graph View cross-surface fixture 覆盖 scan/seek/path/subquery/aggregation/write/full-text/vector/historical read：read/search/historical result 与对应 Snapshot 的物理诱导子图 oracle 一致，read-write query 与逐 clause graph-state oracle 一致，Schema/Constraint 仍按完整 canonical graph 验证，且不存在 visibility/write bypass；
- [x] fuzz/property suite 无未解决 correctness finding；
- [x] crash/recovery/migration suite 全通过；
- [x] format `1 -> 2` migration 保持全部既有 Commit ID、Snapshot 与 history semantics，Commit Data/Tag/Merge Session storage 的 crash/reopen 与 rollback 行为正确；
- [x] Tag、Commit Data、explicit empty-delta Commit 与 cursor-based DAG History 的 Phase 09 acceptance 在 release matrix 中回归通过；
- [x] Native explicit transaction 的 C ABI/ownership、multi-execution single-Commit、expected-head mismatch、cross-execution Graph View、`LOAD CSV` rejection、execute/callback/cancel/commit/abort/connection-teardown rollback、transaction/statement clock 与 staged-isolation acceptance 在 release matrix 中回归通过；
- [x] Merge Session 的 restart recovery、large-conflict pagination、incremental resolution、start expected-head、revision/cursor stale detection、candidate read-only inspection、read-prepare + short-writer finalize、target-head CAS、stale-abort protection、single finalize 与 GC-root acceptance 在 release matrix 中回归通过；
- [x] 10M/100M benchmark gate 通过；
- [x] 所有 release artifacts real-load acceptance 通过；
- [x] cross-platform storage fixture interoperable；
- [x] final review finding 闭环；
- [x] docs 与 release behavior 一致；
- [x] final diff 无 temporary artifact/secret/unrelated change。

## 6. 完成条件

只有 Release Acceptance 全部满足，才允许把 Lithograph 首个完整版本描述为：

> stock SQLite loadable extension，完整实现 `CY25-2026.08` current-graph Profile，并提供基于 immutable Commit DAG、Branch/Tag 与结构化版本操作的 Versioned Property Graph。
