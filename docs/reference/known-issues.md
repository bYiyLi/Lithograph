<a id="v010-已知问题与接入规避"></a>

# 已知问题、版本适用范围与规避

适用版本：**v0.3.3**（含明确标注的旧版本历史） · [产品手册](../guide/README.md) · [接口参考](README.md)

v0.3.2 保留 Procedure metadata 与 createStatement 完整性限制；Provider cache 首次并发初始化已修复，并通过真实多进程、最终完整源码门禁与本机包验收，见 [发布说明](../releases/v0.3.2.md)。下文保留 v0.3.1 实际制品证据与适用规避；v0.3.2 的远端及发布状态见 [Phase 18](https://github.com/bYiyLi/Lithograph/blob/main/docs/development/phases/18-query-access-controls.md#v032-release-delivery)。

## 当前版本

v0.3.3 保留以下 Procedure metadata 与 createStatement 限制。execution 预算不等于 RSS 硬上限；transaction inner batch 仍先形成完整 RowSet，超预算时拒绝当前 batch，已 durable batch 保留，省略预算时工作集仍可能增长，详见 [v0.3.3 发布说明](../releases/v0.3.3.md) 与 [执行预算](execution-options.md#memory-limit)。

`SHOW PROCEDURES` 的参数/结果 metadata 仍不完整：v0.3.1 中，`lithograph.branch.list` 的 argumentDescription 为空，active 被描述为 STRING，但实际返回 Boolean。范围与规避见 [DOC-V010-03](#procedure-metadata)。接口名称/signature 仍可用于发现；不要仅凭错误 metadata 自动生成返回类型绑定。

旧 checkout adapter 问题已在 v0.3.0 修复，application Native query ABI 已在 v0.3.0 删除；这两项不属于 v0.3.1 当前缺陷。本页保留历史复现，避免把旧版本限制误套到当前接口。

<a id="index-create-statement"></a>

## SHOW INDEXES 的 createStatement 完整性

v0.3.1 的 FULLTEXT/VECTOR `createStatement` 只输出基础创建形式，不能独立作为多 Label/Type、additional property 和 OPTIONS 的完整导出。复制 definition 时应同时保存 `labelsOrTypes`、`properties`、`options` 等实际字段，按 [索引参考](../cypher/indexes.md) 重新构造；需要保留完整版本化定义时使用公开 Diff/Patch 对象。不能只复制这个字符串后假定索引配置完全相同。

<a id="provider-cache-first-use"></a>

## v0.3.1：Provider cache 首次并发初始化

**影响 v0.3.1 随附的 OpenAI-compatible Provider；v0.3.2 已修复。** 仅当 `cache.enabled=true`，多个进程首次同时打开同一个尚未初始化的 cache path 时，一个请求可能返回 Provider `IO_ERROR`（code 2），message 为 `cache database operation failed`。两方都可能先观察到空库；取得初始化 writer 后没有重新检查另一方刚创建的 Schema，后一方创建同名表失败。cache 已初始化后的正常并发读写不涉及这个首次创建窗口。

v0.3.2 的真实多进程永久回归、最终 quality/CI 与本机 macOS arm64 包均通过 SQLite 3.45.0 / 3.51.0 / 3.53.4 的适用验收。v0.3.1 tag、Provider 二进制与下面的历史证据保持不变，下面的串行预热规避继续适用于 v0.3.1。

该问题在下载的 macOS arm64 v0.3.1 Provider / SQLite 3.51.0 上用两个进程、临时空 cache 和 150 ms 初始化锁定向复现 5/5：一个请求成功，另一个失败，最终只有成功请求的 entry；未观察到 cache 内容损坏。其它平台共享相关源码，此处不将源码推断写成逐平台复现。v0.3.0 与 v0.3.1 的 cache 实现源码未变，但本条制品证据只针对 v0.3.1。

**规避：先串行预热，再启用并发。** 启动 worker/连接池并发工作前，由单个进程使用目标 Provider 配置和同一 cache path 完成一次有效 embedding/Semantic 请求，等待成功后再启动其余进程。仅创建目录或空文件不等于完成 Provider 初始化。已经遇到此错误时，先让初始化请求结束并核对路径/权限，再按业务幂等与重试策略处理失败请求；不要盲目重放整个 mutating Cypher。也可以保持默认的 `cache.enabled=false`。

不要手工创建或修改 cache marker/内部表，也不要将 graph main 作为 cache。配置与 I/O 失败语义见 [Provider Reference](openai-compatible-provider.md#persistent-cache)，使用步骤见 [Provider 指南](../guide/embedding-providers.md#enable-cache)。

## 历史制品证据

下列历史问题最初在**实际已发布 v0.1.0 macOS arm64 制品**复现。v0.2.1 release gate 重新确认过部分旧行为。历史 Native 操作只适用于对应旧 tag。

验证制品：`lithograph-macos-arm64.tar.gz`，SHA-256 `71a50eb3d7b745dc5a616a12ad1c4bc06578b0ee17a4f7f7e1d4c0c4f7c0e278`。SQL 入口在 SQLite 3.45.0 / 3.51.0 上复现；Native 入口使用 SQLite 3.51.0。其他平台共享相关代码，但本次没有逐个平台复现，不将推断写成实测。

<a id="historical-checkout"></a>

## DOC-V010-01：branch.checkout 被 adapter 的事务边界拒绝

预期：在没有外层事务的 connection 上调用 checkout，改变 active Branch。实际：SQL scalar 与普通 Native execute 都返回 `TRANSACTION_BOUNDARY_REQUIRED: Branch checkout requires SQLite autocommit mode`。

最小复现：空库加载扩展后，不执行 BEGIN，依次运行：

```sql
SELECT lithograph_init();
SELECT lithograph('CALL lithograph.branch.create(''probe'')');
SELECT lithograph('CALL lithograph.branch.checkout(''probe'')');
```

最后一条是**旧版本预期复现错误**，不是当前成功教程。需要复现 Native 路径时请 checkout 对应 v0.1.0/v0.2.1 tag；当前 `native_transaction.c` 已改为 SQL-only C host 示例。

历史代码证据属于旧 tag；v0.3.0 源码已移除 `crates/lithograph-extension/src/native.rs`，不能用当前源码路径解释旧 binary。

**规避：每次 query 显式传 `options:{"branch":"probe"}`。**读历史用 `at`，SQL / Native explicit transaction 在 begin 中指定 branch。无需 checkout 就能创建、读写、合并目标 Branch。不要直接改内部 connection state 或库表来绕过错误。

<a id="historical-native-null"></a>

## DOC-V010-02：Native NULL JSON 不自动使用空 object

预期：可省略的 params/options 支持默认空 object。实际：`NULL,0` 被解码为空字符串，不能作为有效 JSON；普通 execute 的 params/options 分别返回 `INVALID_ARGUMENT`，tx_execute 的空 params 也复现了同类失败并自动 abort。

**规避：传入 `"{}",2`**，长度是两个 UTF-8 bytes。tx_begin 不需要自定义 options 时也显式传 `{}`，不要依赖空指针默认行为。非空对象同样使用准确 bytes length。

普通 Native execute 的最小复现仅保留在旧 tag；当前 SQL surface 省略第二/第三参数时使用 `{}` 默认值，不存在该 Native NULL decoder。

<a id="procedure-metadata"></a>

## DOC-V010-03：Procedure introspection 参数/结果 metadata 不完整

**仍影响 v0.3.1。** `SHOW PROCEDURES YIELD *` 的 `returnDescription[*].type` 在 v0.1.0 全部写成 `STRING`，当前实现也保留该问题；`argumentDescription` 也为空。实际返回值并非全是字符串，例如 `branch.list.active` 是 BOOLEAN、`commit.get.parents` 是 LIST、`diff.patch` 是 MAP、全文 score 是 FLOAT。

下面两个查询用于对比真实元信息和真实值；假定数据库已初始化：

```sql
SELECT lithograph(
  'SHOW PROCEDURES YIELD name,argumentDescription,returnDescription
   WHERE name = ''lithograph.branch.list''
   RETURN name,argumentDescription,returnDescription'
);
SELECT lithograph('CALL lithograph.branch.list()');
```

源码证据：[introspection builder](../../crates/lithograph-core/src/query/completeness/execute/helpers.rs) 固定使用 STRING 生成每个输出描述。函数/Procedure 的名称与 signature inventory 仍可用于发现接口，但不能从不完整 metadata 生成可信类型绑定。

**规避：使用公开 signature 与本手册的 [实际返回值类型](procedures.md)，按照 JSON 原始类型/tag 解码。**生成的 Procedure Inventory 仅复制输出列名，不把错误类型复制成用户合同。该问题不把实际 BOOLEAN/MAP/INTEGER 结果变成 STRING，也不表示相关查询本身不可执行。

## 宿主构建问题不是 Lithograph release bug

本机系统 Python 缺少 `enable_load_extension`；macOS SDK 的 SQLite header 不公开此次编译所需加载入口。手册因此要求选择明确支持 extension loading 的 Python/SQLite 构建，C 编译同时指定同一安装前缀的 include/lib。这些环境前提见 [安装](../guide/installation.md) 和 [集成](../guide/integration.md)。

当前 metadata 问题有明确规避路径；后续修复应针对对应版本制品重新验证后再更新状态。历史问题记录只说明其对应版本，不覆盖当前 SQL-only 接入合同。
