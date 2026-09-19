# Phase 14：SQL Explicit Transaction Adapter

**状态：`done`**

## 1. 目标与范围

在 Phase 09 已完成的 connection-scoped explicit transaction 上增加普通 SQLite SQL scalar 封装，使用调用方已有的 SQLite driver 即可执行 `begin -> execute* -> commit | abort`，不需要额外绑定 `lithograph_v1_tx_*` C ABI。产品合同只由 [SQL Bridge](../../design/interfaces.md#sql-bridge) 与 [Explicit Transaction](../../design/storage.md#native-explicit-transaction) 定义；本计划只安排实现顺序、验收和状态。

本 Phase 新增 `lithograph_tx_begin(options_json)`、`lithograph_tx_execute(query [, params_json [, options_json]])`、`lithograph_tx_commit()` 与 `lithograph_tx_abort()`；复用现有 parser/planner/executor、transaction state、staged storage、Commit finalize 和 error mapping，保持 C ABI 不变。

本 Phase **不**改变普通 `lithograph()`、`lithograph_rows()`、caller-owned SQLite transaction、Cypher `IN TRANSACTIONS`、Commit/Branch/Schema 语义、storage format 或 `CY25-2026.08` grammar/Profile；不新增自动 squash/Commit 合并机制。发布动作不能替代本计划的验收门禁。

## 2. 前置条件

- Phase 00–13 已完成；直接依赖 Phase 01 SQL/Native adapter boundary、Phase 05 transaction -> Commit 基础和 Phase 09 Native explicit transaction。
- 现有 C ABI 已覆盖 staged read/write、expectedHead、metadata、empty-delta、fail-closed abort 与 connection teardown；本 Phase 不重新设计这些语义。
- SQLite minimum 保持 3.45.0；新 SQL 入口必须在真实 loadable-extension callback 中完成 begin/commit/rollback，不能以 Rust 直接调用或 Native-only 测试替代。

## 3. Feature 顺序

```text
14.1 design + SQL registration/result adapter
 -> 14.2 shared transaction core + fail-closed cleanup
 -> 14.3 real extension lifecycle/negative tests
 -> 14.4 minimum SQLite + Native regression + docs/review closure
```

### Feature 14.1 SQL surface 与结果适配

- 按 `SQLITE_UTF8 | SQLITE_DIRECTONLY` 注册四个有副作用 scalar，不声明 deterministic/innocuous；
- 实现 exact arity、TEXT/NULL/JSON 校验和省略 params/options 的 `'{}'` default；
- `tx_execute` 将 `COLUMNS -> ROW* -> SUMMARY` 转成完整 scalar envelope，保留 tagged value、列位置、summary 与 SQLite length limit contract。

### Feature 14.2 共享 transaction core 与 cleanup

- SQL / C adapter 直接复用同一 connection-local transaction state 与 begin/execute/commit/abort 实现，不复制 storage/version 逻辑；
- `tx_begin` 在返回前的可检测 result failure 自动 rollback；`tx_execute` 任何参数/execution/event/result failure 自动 rollback；`tx_commit` 在 SQLite COMMIT 前完成结果构造/长度检查；
- 保持 outer transaction/repeated begin/no-active/expectedHead/metadata/Graph View/allowed Cypher 的已有稳定 error category 和 SQLite primary code。

### Feature 14.3 真实 SQL 扩展验收

- 通过真实 `.load` 后的 `SELECT lithograph_tx_*` 验证多次 write 只一个 Commit、staged read、read-only transaction 和 empty-delta；
- 验证 explicit abort、execute failure、连接未 terminal 即 close 的 graph/history 无残留；
- 验证 outer SQLite transaction、duplicate begin、no-active execute/commit/abort、expectedHead mismatch、非 TEXT/NULL/malformed JSON/unknown option/wrong arity；
- 用 `sqlite3_get_autocommit()` 和另一 connection 的可见性断言，证明 SQL callback 内部真正 begin/commit/rollback，而不是只调用 Native unit helper。

### Feature 14.4 回归、文档与 review

- 运行 extension unit/integration、Phase 09、Native ABI、SQLite 3.45.0 与 current frozen runtime real-load regression；
- 在 Linux/macOS/Windows x64/arm64 Release Matrix 上加载实际制品，Windows 在 minimum/current SQLite 上直接编译并运行同一 SQL transaction C smoke；
- 同步 SQL API reference、transaction guide、可运行 example、development roadmap/record，不复制 Design 合同；
- 执行 format/clippy/tests、`git diff --check`、最终 diff review，关闭本轮 correctness、storage-integrity、ABI 和文档 finding。

## 4. Acceptance Matrix

| ID | 必须验证的场景 | 验收证据 / 判定 | 状态 |
| --- | --- | --- | --- |
| TX14-01 | 四个 SQL function 注册 | 真实 extension `.load` 后均可通过 `SELECT` 调用，flags 不含 deterministic | `done` |
| TX14-02 | 多 execution 一 Commit | 两次写入 + staged read + commit，log 只新增一个 Commit | `done` |
| TX14-03 | read-only / empty-delta | 纯读不新增 Commit；有 write intent 但 final net-zero 产生唯一 empty-delta Commit | `done` |
| TX14-04 | abort / execution failure / close cleanup | graph、Schema、Layer、Commit 和 Branch head 均无残留 | `done` |
| TX14-05 | boundary 与参数负例 | outer tx、duplicate begin、no-active、expectedHead、NULL/type/JSON/options/arity 稳定返回无污染错误 | `done` |
| TX14-06 | SQL callback 真正拥有 transaction | autocommit state 与 cross-connection visibility 证明 begin/commit/rollback 在 callback 内完成 | `done` |
| TX14-07 | 现有行为不变 | 普通 `lithograph()` / outer SQLite tx 继续 per-query Commit；Native ABI regression 通过 | `done` |
| TX14-08 | minimum/current SQLite 与 repository gates | SQLite 3.45.0/current real-load、targeted Rust、Native ABI、format/clippy/diff checks 通过 | `done` |
| TX14-09 | 用户文档与可运行示例 | SQL reference、transaction guide、example 与 development record 与真实行为一致 | `done` |
| TX14-10 | 六目标 release artifact | Linux/macOS/Windows x64/arm64 hosted Release Matrix 通过，Windows minimum/current SQLite 都执行真实 SQL tx smoke | `done` |

## 5. 完成条件

TX14-01–10 全部闭合。实现 revision `2f617f13e007ce713bd48078396c40bf0a963c7c` 的 repository CI [`35448157779`](https://github.com/bYiyLi/Lithograph/actions/runs/35448157779) 与六目标 Release Matrix [`35448157766`](https://github.com/bYiyLi/Lithograph/actions/runs/35448157766) 均通过；Windows x64/arm64 在 SQLite 3.45.0 与 3.53.4 上各自编译并执行同一真实 C SQL transaction smoke。Release review 发现的 canonical-history 回滚断言、Windows SQL smoke 与 C complexity gate 缺口均已修复，Phase 14 状态为 `done`。
