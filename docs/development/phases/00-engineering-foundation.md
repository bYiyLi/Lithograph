# Phase 00：Engineering Foundation

**状态：`done`**

## 1. 目标

建立后续所有 Engine Phase 共享的 Rust、SQLite extension、compatibility 和 test 工程基线。Phase 00 不实现 graph product behavior。

## 2. Design Inputs

- [Cypher 25 兼容合同](../../design/compatibility.md)、[Deployment 与 Runtime Boundary](../../design/runtime.md#deployment)、[Large-scale Invariants](../../design/runtime.md#large-scale-invariants)；
- `docs/development/cypher25-compatibility.md`；
- `docs/research/reference-baseline.md`。

## 3. Features

### Feature 00.1 Rust workspace

- 通过官方 `rustup` 安装/提供 Rust toolchain；当前冻结 toolchain 为 **Rust 1.98.1**；
- Rust 2024 Edition workspace；
- 首个 SQLite boundary dependency 固定 `rusqlite 0.40.1`，启用 `loadable_extension`、function 与 virtual-table 所需 features；禁止 `bundled` SQLite feature；
- core crate、SQLite extension crate、test-support crate 的最小结构；
- `rust-toolchain.toml` 明确 pin `1.98.1`，CI 与本地都使用该版本；
- `Cargo.lock` 纳入仓库；
- 不建立没有当前 owner 的抽象 crate。

### Feature 00.2 Quality gates

建立 canonical commands：

```text
cargo fmt --check
cargo clippy --locked --workspace --all-targets --all-features
cargo test --locked --workspace
cargo make quality
```

CI 默认执行同一 gate，避免本地/CI 两套标准；除 `cargo fmt` 外的 Cargo build/test/run gate 使用 `--locked`，保证 `Cargo.toml` 与仓库 `Cargo.lock` 不一致时直接失败而不是静默改写 lockfile。通用质量分析优先使用标准工具，不在仓库内重复实现第二套静态分析器；仅 Rust source physical-line budget 属于 Lithograph 项目级 policy，由统一任务编排层直接执行轻量阈值检查。

`Makefile.toml` 使用 `cargo-make 0.37.24` 作为统一质量任务入口，并固定执行：

- `cargo-llvm-cov 0.8.7`：coverage profile 由 Rust unit tests 与真实 instrumented SQLite `.load` / Phase 01 / Native ABI smoke 共同产生；排除仅做命令编排的 `lithograph-test-support/src/bin/` 和 extension 单元测试源文件后，workspace line coverage 不低于 80%、function coverage 不低于 75%、region coverage 不低于 80%，且每个进入报告的 source file line coverage 不低于 50%。Extension 单元测试仍独立执行，但生产 extension coverage 以真实 host SQLite execution path 为主要证据；
- `cargo-deny 0.20.2`：对当前支持的 macOS arm64/x86_64、Linux arm64/x86_64、Windows x86_64 target graph 执行 RustSec advisory、license allowlist、wildcard dependency、registry/git source policy；重复 dependency version 当前作为 warning 暴露，不因无法由 Lithograph 直接控制的 transitive split 阻塞开发；
- `cargo-machete 0.9.2`：拒绝 unused Cargo dependency；
- `Lizard 1.24.0`：production Rust cyclomatic complexity `CCN <= 15`、函数 physical length `<= 100`、参数 `<= 10`（允许冻结的 C ABI surface）；test-support 与 Native ABI C smoke 使用 `CCN <= 20`、函数 physical length `<= 150`、参数 `<= 10`；普通 Rust 函数的 7 参数上限继续由 Clippy hard gate 负责；
- `jscpd-rs 0.1.12`：production Rust 以至少 5 行 / 50 token 的 clone 为检测单元，duplicate line ratio 达到 1% 即失败；
- Clippy：函数最多 100 行、参数最多 7 个、cognitive complexity threshold 25、type complexity threshold 250，并拒绝 `dbg!`、`todo!`、`unimplemented!`、无理由 lint suppression 和没有 safety rationale 的 unsafe block；
- Rust lint：workspace 默认拒绝 unsafe code，只有 SQLite/Native ABI owner `lithograph-extension` 在 crate boundary 明确说明理由后允许；该 crate 内 `unsafe_op_in_unsafe_fn` 仍为 hard error；production `lithograph-core` / `lithograph-extension` 额外拒绝 `unwrap`、`expect` 和 `panic!`；
- Rust source file physical-line budget：1000 行开始 warning，1400 行 hard failure；
- `RUSTDOCFLAGS=-D warnings cargo doc --workspace --all-features --no-deps`：public Rust documentation 必须无 warning。

### Feature 00.3 SQLite fixture harness

- ephemeral file database；
- in-memory database；
- extension-load fixture；
- disposable corruption/crash fixture；
- deterministic seed / cleanup；
- SQLite version/compile-option inspection helper。

### Feature 00.4 Compatibility harness

- vendor/pin Apache-2.0 openCypher TCK feature data 与 NOTICE，Lithograph 自己实现/适配 runner，不依赖 JVM runtime 才能执行默认 compatibility gate；
- 建立 Cypher 25 fixture format；
- fixture 可声明 query、params、initial graph、expected rows/error、profile source；
- 结果比较保留 row multiplicity、ordering requirement 和 typed values；
- 产生 machine-readable compatibility report。

### Feature 00.5 Reference fixtures

固定首批 oracle fixtures，覆盖：

- parse/error location；
- integer/null/string/list/map；
- basic `MATCH/RETURN`；
- Cypher 25 `WHEN/NEXT/FOR` parser fixtures；
- `VECTOR`、Graph Type、`SEARCH`、UUID、string interpolation parser fixtures。

这些 fixtures 允许未实现，但必须能被 harness 表达并报告 `planned/failing`，不能因未实现而从 inventory 消失。

## 4. Acceptance

- [x] workspace 在 clean checkout 构建；
- [x] `rustc --version` / `cargo --version` 与 pinned toolchain 一致；
- [x] `cargo tree -e features` 证明 Extension dependency graph 未启用 bundled SQLite；
- [x] format/clippy/test canonical commands 通过；
- [x] fixture database 可重复创建/销毁；
- [x] compatibility runner 可以执行至少一个 passing fixture 和一个 intentional failing fixture，并准确报告差异；
- [x] openCypher TCK scenario 可以被 harness 读取；
- [x] pinned TCK revision、LICENSE 与 NOTICE 可从仓库复现；
- [x] `CY25-2026.08` fixture metadata 可以记录 source/version/family；
- [x] CI 与本地 gate 使用同一 canonical scripts/commands；
- [x] complexity、file/function size、coverage、dependency advisory/license/source、unsafe 与 lint-bypass policy 均有自动化 hard gate；
- [x] 没有 product behavior 被 mock 后声称实现。

## 5. Review

检查：

- workspace 是否为未来假设拆出多余 crate；
- test harness 是否丢失 duplicates/type/error-location；
- external TCK fixture 是否被修改成迎合实现；
- benchmark/corruption fixture 是否只使用 disposable data。

## 6. 完成条件

所有 Acceptance 通过，review finding 闭环，Phase 00 状态同步为 `done`，Phase 01 转为 `ready`。

## 7. 完成证据

- Rust toolchain 由 `rust-toolchain.toml` 固定为 `1.98.1`，workspace 使用 Rust 2024 Edition，`Cargo.lock` 已纳入仓库；CI 显式执行 `rustup toolchain install 1.98.1`，不依赖 rustup 已弃用的 implicit auto-install；
- workspace 仅包含 `lithograph-core`、`lithograph-extension`、`lithograph-test-support` 三个当前有明确职责的 crate；
- Extension 使用 `rusqlite 0.40.1` 的 `loadable_extension`、`functions` 与 `vtab` feature，feature tree 已验证不存在 bundled SQLite，artifact dependency inspection 也未发现 private SQLite runtime linkage；
- `scripts/ci.sh` 是本地与 CI 共享 gate，并验证 SQLite `3.45.0+`、FTS5、thread-safe 与 loadable-extension runtime requirement；所有会解析 dependency graph 的 Cargo build/test/run gate 使用 `--locked`，已用 stale `Cargo.lock` 负向探针确认会直接失败而不会静默改写 lockfile；
- `cargo make quality` 是维护性与 supply-chain quality 的统一入口：Clippy + Lizard 负责 lint、cognitive/cyclomatic complexity、函数体量与 unsafe discipline，`jscpd-rs` 负责 production clone/duplicate gate，`cargo-llvm-cov` 把 unit test 与真实 SQLite `.load` / Phase 01 / Native ABI path 合并为 production coverage baseline，`cargo-deny` 负责 advisory/license/source policy，`cargo-machete` 负责 unused dependency，Rustdoc warnings 与 Rust file budget 进入同一 gate；仓库不维护第二套通用静态分析实现；
- 当前开发机使用支持 `.load` 的 SQLite 3.51.0 对真实 `lithograph` shared library 完成 load smoke；系统自带 `OMIT_LOAD_EXTENSION` 的 SQLite 不作为合格 runtime；
- file / memory / corruption / crash fixture、runtime inspection、compatibility result comparator 与 machine-readable report 均有自动化测试；fixture/report boundary 会拒绝空 inventory、duplicate ID、mixed profile、row width mismatch、矛盾 parse expectation 与非法 error location，而当前 `CY25-2026.08` 11 个 fixture 已通过同一 validation；
- openCypher TCK 固定为 upstream `2024.3` / `677cbafabb8c3c5eed458fd3b1ec0daec8d67d23`，vendored `LICENSE`、`NOTICE`、feature 与 graph data 已与 upstream 逐字节核对；`.gitattributes` 禁止该目录的 Git text normalization，`MANIFEST.sha256` + `scripts/check-vendor.py` 在默认 gate 中同时验证固定 source/tag/commit/content/license revision metadata 和 227 个 upstream 文件；错误 commit metadata 的负向探针会被明确拒绝；
- Rust TCK adapter 对 pinned corpus 可读取 220 个 feature、1615 个 scenario definition、276 个 Scenario Outline、7104 个 step、2370 个 DocString、1687 个 DataTable 与 276 个 Examples block，并展开为 3897 个 executable scenario；Gherkin DataTable 的 `\\n` / `\\|` / `\\\\` escaping、Background、tag 与 outline substitution 均有 targeted tests；
- TCK 结构与展开结果使用临时安装的 Cucumber 官方 `gherkin-official` parser/compiler 做了独立交叉验证：全部 220 个 feature 的结构逐项一致，全部 3897 个 executable scenario 的 name/tag/step/DocString/DataTable 展开内容逐项一致，mismatch 为 0；该工具仅用于 review，不进入 Lithograph dependency；
- 首批 `CY25-2026.08` fixture inventory 已覆盖本 Phase 要求的 parser、value、`MATCH/RETURN` 与 Cypher 25 新能力入口；Graph Type positive fixture 已按官方 `ALTER CURRENT GRAPH TYPE SET { ... }` / element-type 语法校正，parse-error fixture 固定官方可观察的 line/column；全部 capability 仍保持 `planned`，没有伪造 Engine 实现状态；
- 最终验收使用当前工作树建立临时 Git commit，再从该 commit 真实 `git clone`；clone 工作区 clean、`target` 初始不存在、vendored CRLF 原始字节保持不变，并通过 `cargo build --workspace --locked` 与完整 `scripts/ci.sh` gate。
