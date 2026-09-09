# Phase 01：SQLite Extension Boundary

**状态：`done`**

## 1. 目标

交付可被 stock SQLite 真实加载的 Lithograph shared library，并固定 SQL Bridge、Native C ABI、初始化、内部 format metadata 与迁移 boundary。

## 2. 依赖

- Phase 00 `done`。

## 3. Design Inputs

- `docs/design.md` 第 4、13–16 节。

## 4. Features

### Feature 01.1 Loadable extension ABI

- `sqlite3_lithograph_init`；
- 使用 `sqlite3_api_routines` host API table 初始化 Rust SQLite boundary；
- `rusqlite::Connection::extension_init2` 或等价 `libsqlite3-sys` loadable-extension API 完成 connection wrapper；
- Extension artifact 不链接/打包私有 SQLite runtime；
- macOS/Linux/Windows shared-library build；
- 只使用 SQLite public extension API；
- `.load` 只注册功能，不创建内部表。

### Feature 01.2 Initialization and format metadata

实现：

```sql
SELECT lithograph_init();
SELECT lithograph_version();
```

- `_lithograph_meta`；
- storage format version；
- idempotent init；
- transactional migration scaffold；
- `FORMAT_TOO_NEW`；
- 不使用 `PRAGMA user_version`。

Phase 01 只拥有 format metadata 和 migration boundary。Root Commit、`main` Branch 及完整 canonical internal schema 由 Phase 02 建立；在 Phase 02 完成前，`lithograph_init()` 的 `root` / `branch` 字段保留为 `null`，不得用伪造 Commit/Branch ID 提前满足最终产品 contract。

### Feature 01.3 SQL Bridge

注册：

```text
lithograph(...)
lithograph_rows(...)
lithograph_validate(...)
lithograph_integrity_check()
```

`lithograph_rows` 使用 eponymous-only virtual table + hidden input columns，并在本 Phase 固定 planner/filter/cursor 生命周期边界；真实 row streaming 由 Phase 04 接入。

所有 side-effect scalar invocation 建立唯一内部 SAVEPOINT；失败必须 rollback 到 invocation 开始状态，再把 error 返回 SQLite。不得依赖宿主 SQL statement 自动回滚递归 UDF 写入。

Phase 01 固定 adapter/module ABI、cursor 生命周期和 side-effect SAVEPOINT primitive，但不伪造 Cypher parser/executor。成功的 `lithograph_validate()` 由 Phase 03 接入，read result / `lithograph_rows` streaming 由 Phase 04 接入，mutating invocation 与 `READ_ONLY_ADAPTER` query classification 由 Phase 05 及对应后续能力 Phase 接入。在这些 owning Phase 完成前，已初始化 database 上的 Cypher execution/validation surface 返回稳定 `SEMANTIC_ERROR`，不得通过字符串 special-case 生成假结果。

### Feature 01.4 Native C ABI v1

实现 versioned symbols：

```text
lithograph_v1_execute
lithograph_v1_validate
lithograph_v1_free
```

- 接收 existing `sqlite3*`；
- callback ABI、payload lifetime 与 ownership boundary；
- structured error；
- ABI tests 验证 C caller。

Phase 01 固定 symbol、参数、ownership、panic boundary、structured error allocation/free 与 C caller compatibility。成功 query 的 `COLUMNS -> ROW* -> SUMMARY` streaming/cancel 语义随 Phase 04 real executor 接入；write cancel rollback 随 Phase 05 write path 验证。

### Feature 01.5 Safety flags

- mutating/executing entrypoints 使用 direct-only boundary；
- 无副作用 information function 才允许 innocuous；
- trigger/view 不得隐式触发 Cypher write/init/migration。

## 5. Acceptance

- [x] macOS 当前开发机通过真实 `sqlite3 .load ./lithograph`；
- [x] SQLite 3.45.0 minimum fixture 与当前开发 SQLite 都通过真实 `.load` + init smoke；
- [x] artifact dependency/symbol inspection 证明没有 bundled/private SQLite runtime，SQLite calls 通过 extension host API table；
- [x] `.load` 前后 user schema 不发生变化；
- [x] 未初始化 database 调用 graph API 返回 `NOT_INITIALIZED`，`lithograph_version()` 仍可读取 Extension/format support metadata；
- [x] `lithograph_init()` 创建 internal metadata 并幂等；
- [x] init 中途故障 fixture rollback，无 half-schema；
- [x] 所有 canonical metadata SQL 显式限定 `main` schema；TEMP 同名 `_lithograph_meta` 不能截获 init/read，关闭该 connection 后持久 database 仍保持已初始化；
- [x] higher format fixture 返回 `FORMAT_TOO_NEW`；
- [x] SQL scalar/rows/validate/version/integrity symbols 可调用；
- [x] Phase 01 可产生的 init/version/integrity/error JSON shape 与 Design 固定 contract 一致；Phase 03 接入 validate success shape；
- [x] 可读 metadata 损坏返回 `lithograph_integrity_check() -> {ok:false,...}` 与结构化 errors；schema/marker/row-count corruption 不得 false-positive 为 `ok:true`，`lithograph_version()` 不把损坏静默伪装成未初始化；
- [x] SQL Bridge 的 query/params/options 类型与 JSON argument failure 统一进入 `LITHOGRAPH_INVALID_ARGUMENT`，不泄漏 rusqlite filter/function parameter error；
- [x] `lithograph_rows` 是 eponymous-only virtual table，query/params/options 使用 hidden columns，cursor 不建立 full-result materialization contract；真实 row streaming acceptance 由 Phase 04 完成；
- [x] 在 Phase 03–04 real frontend/executor 接入前，已初始化 database 的 Cypher execution/validation surface 统一返回稳定 `SEMANTIC_ERROR`，不存在 query-text special case 或 fake result；
- [x] 预先存在 `_lithograph_*` object 但无有效 magic marker 的 fixture 返回 `STORAGE_ERROR` 且不覆盖 user object；
- [x] `lithograph_init()` 在 metadata DDL 后、marker write 前故障时，内部 SAVEPOINT 清除该 invocation 的全部变化；caller-owned outer transaction rollback 同样能撤销成功 init；
- [x] internal SAVEPOINT `ROLLBACK TO` / `RELEASE` 故障不会静默丢弃 cleanup failure，也不会在 `ROLLBACK TO` 失败后继续 `RELEASE`；无法完成 invocation-local cleanup 时按 Design fail closed 执行 full `ROLLBACK` 并返回 `INTERNAL_ERROR`，autocommit 恢复且 caller-owned outer transaction fallback 行为通过 fault fixture；
- [x] C ABI smoke 通过；
- [x] Native ABI 的 versioned symbols、misuse handling、structured error allocation/free lifecycle 通过 C caller test；
- [x] `lithograph_rows` virtual-table callback 在 Lithograph boundary 内捕获 Rust panic 并转换为 `INTERNAL_ERROR`，不依赖 rusqlite vtab wrapper 提供 unwind boundary；Native pointer/length UTF-8 输入在 ABI call 内复制为 owned value，不产生跨调用借用；
- [x] trigger/view direct-only negative test 通过；
- [x] Linux/Windows cross-build 通过。

以下最终 Design acceptance 保留在其 owning Phase，不在 Phase 01 重复实现第二套临时语义：

- Root Commit / `main` Branch 与完整 integrity：Phase 02；
- `lithograph_validate()` success semantics 与 JSON：Phase 03；
- scalar read envelope、`lithograph_rows` 真 streaming、Native read event order/cancel：Phase 04；
- mutating scalar per-invocation SAVEPOINT、autocommit/outer transaction composition、rows 对 mutation 的 `READ_ONLY_ADAPTER`、write cancel rollback：Phase 05；
- rows 对 `LOAD CSV` / transaction-owning side effect 的 `READ_ONLY_ADAPTER`：Phase 08；
- rows 对 checkout / GC / version side effect 的 `READ_ONLY_ADAPTER`：Phase 09。

## 6. Review

重点检查 ABI lifetime、SQLite allocator/API 使用、panic 跨 FFI、connection-local state cleanup、reentrancy、error ownership 和 migration atomicity。

## 7. 完成条件

Acceptance 全部通过且 review 闭环；Phase 02 转 `ready`。
