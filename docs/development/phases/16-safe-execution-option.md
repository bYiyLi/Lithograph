# Phase 16：只读且禁止外部 I/O 的 Cypher execution option

**状态：`done`**

## 目标与范围

在 Phase 15 已完成的 SQL-only 共享 execution core 上，为调用方增加一次 query 的可选安全约束。正式行为只由 [Interfaces Query Options](../../design/interfaces.md#query-options) 定义；本 Phase 不改变默认 Cypher、`CY25-2026.08` Profile、storage format、Provider SPI 或既有 Phase 00–15 的完成状态。只修改 Lithograph。

## 实施顺序与验收

| 工作 | 验收 | 状态 |
| --- | --- | --- |
| Query Options 与 SQL explicit transaction 转发 | 缺省/`false` 保持原行为；`true` 在两个 SQL execution surface 与 active explicit transaction 内生效；错误 JSON 类型失败 | 已验证：Core 选项测试与真实 SQLite SQL surface smoke |
| Core prepare 安全分类 | 完整 AST 的 graph/schema/index/version/maintenance write、transaction subquery、`LOAD CSV`、Managed Semantic 和可能调用宿主 tokenizer 的 Full-text Procedure 在 runtime work 前拒绝；嵌套/零行/参数/`EXPLAIN`/`PROFILE` 不能绕过 | 已验证：Core targeted tests 覆盖零行 `UNWIND`、嵌套 `CALL`、参数、`EXPLAIN`/`PROFILE` |
| 只读正向与副作用证据 | 普通/历史/组合 query 与允许的 Version read 成功；拒绝后 Branch head/Commit 数不变；默认写入和 Provider query 仍成功 | 已验证：Core tests、SQL smoke 与 synthetic Provider probe |
| 真实扩展与质量门禁 | SQLite `.load` 的 scalar/rows/explicit transaction、synthetic Provider dual-extension；`cargo fmt --check`、Clippy、target tests、`cargo make quality`、`scripts/ci.sh` 适用门禁 | 2026-10-16 完整 quality 与 CI 均通过；SQLite 3.45.0/3.51.0/3.53.4、SQL 两入口与 synthetic Provider 均有实测证据 |
| Review 与文档同步 | 检查所有已注册 Procedure、执行入口、错误边界、diff、链接及空白；修复影响本合同的问题 | 独立 review 闭合 Procedure 白名单、完整 AST/字符串插值、SQL 两入口与 explicit transaction；文档、链接与空白通过 |

本项是 execution option / adapter 安全边界，不新增 Cypher grammar、TCK 场景或 `docs/development/cypher25-compatibility.md` 的语言 coverage 分母。最终证据以本工作树运行结果为准；旧 Phase 15 CI/Release Matrix 不能证明新选项。

## 本工作树验收记录（2026-10-12，Yi Mac）

- `cargo test --locked -p lithograph-core --test safe_execution`：3/3 通过，包含完整 AST 的 prepare 阶段拒绝、只读正向与默认 `false`。
- SQLite 3.51.0：`scripts/sql-surface-smoke.sh target/debug/liblithograph.dylib` 通过，实际 load 扩展并覆盖 scalar、rows、显式事务、拒绝后 Commit 数不变。
- `lithograph-phase13` 双扩展实测通过 `safe-execution-blocks-provider-before-embedding`；安全选项在 scalar/rows 入口均未调用 synthetic Embedding Provider，默认查询仍调用 Provider。
- `cargo fmt --all -- --check`、`cargo clippy --locked --workspace --all-targets --all-features`、`cargo test --locked -p lithograph-extension` 通过。`cargo test --locked -p lithograph-core -p lithograph-test-support -- --skip load_csv_http_and_io_failures_preserve_atomic_rollback` 通过。
- 未排除测试的 Core/test-support 全量运行在既有 `load_csv_http_and_io_failures_preserve_atomic_rollback` 的本地 HTTP fixture 绑定处失败：`PermissionDenied (Operation not permitted)`。该测试未获得通过证据。
- `cargo make quality` 已执行。首次因当前 Python 3.14 缺少 `lizard` 模块停下；使用本机已有 lizard 的 `PYTHONPATH` 后，vendor、golden、file budget、format、Clippy、rustdoc、complexity、duplicates、unused dependencies 通过，随后 `cargo deny check` 无法在只读 `/Users/yi/.cargo/advisory-dbs/db.lock` 取得锁。Supply-chain 与后续 coverage 未通过，完整 quality gate 保持阻塞；`scripts/ci.sh` 未执行。

该次验收未满足完成标准，因此当时保持 `in_progress`；以上记录保留为历史证据，不代表当前仍受权限阻塞。本选项限制 Lithograph 内本次 Cypher execution 的文件/HTTP/Provider 调用，不构成 OS sandbox。

## 完成验收（2026-10-16，Yi Mac）

- `cargo test --locked -p lithograph-core --test safe_execution`：4/4 通过。review 补齐字符串插值中的子查询递归检查，普通、`EXPLAIN`、`PROFILE` 的嵌套/未执行分支在 prepare 阶段均被拒绝；合法插值仍可使用。
- `scripts/ci.sh` 完整退出 0，未排除 HTTP fixture 或其它必要测试。真实 SQLite 3.45.0 与 3.53.4 的扩展加载/SQL/Provider smoke，以及本机 3.51.0 的 scalar/rows/explicit transaction smoke 全部通过。C smoke 新增 scalar/rows 插值拒绝；检查拒绝后 Commit 数未变、explicit transaction 自动 abort。
- synthetic Provider 的 `safe-execution-blocks-provider-before-embedding` 验证 scalar/rows 在调用 embedding 前拒绝，计数为零；默认 `false` 查询仍调用 Provider。Provider SPI 与双扩展门禁均通过。
- 独立检查全部注册 Procedure 与 11 项只读白名单、Raw Vector SEARCH 的纯 Vector 路径、active transaction options 转发与 fail-closed cleanup；没有未闭环 finding。
- 完整 `cargo make quality` 退出 0：format、Clippy、rustdoc、复杂度/重复/依赖、供应链及真实扩展 coverage 全部通过；regions 83.56%、functions 84.38%、lines 85.43%。旧 lizard 环境问题使用隔离 venv 解决，advisory cache 读写正常；rustls 更新至 0.23.46 后供应链检查通过。
- 完整 inherited TCK 为 3,777 applicable passed、0 failed；18 个冻结 Cypher fixtures 均通过。此选项不增加语言 coverage 分母。
- 使用手册、精确 options/事务参考、安全指南、自动执行示例、链接/稳定锚点与最终 diff 检查完成。提交/远端/正式发布由 [Phase 17](17-developer-documentation-release.md) 单独记录。
