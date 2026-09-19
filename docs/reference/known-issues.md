# v0.1.0 已知问题与接入规避

这些问题最初在**实际已发布 v0.1.0 macOS arm64 制品**上复现，不是新的设计要求。v0.2.1 release-prep source build 重新确认 `branch.checkout` 仍返回 transaction-boundary error，Procedure introspection 仍为空 argument metadata / STRING return type；Native input decoder 仍把 `NULL,0` 解码为空字符串而不是默认 `{}`。因此 v0.2.1 继续保留以下规避路径。

验证制品：`lithograph-macos-arm64.tar.gz`，SHA-256 `71a50eb3d7b745dc5a616a12ad1c4bc06578b0ee17a4f7f7e1d4c0c4f7c0e278`。SQL 入口在 SQLite 3.45.0 / 3.51.0 上复现；Native 入口使用 SQLite 3.51.0。其他平台共享相关代码，但本次没有逐个平台复现，不将推断写成实测。

## DOC-V010-01：branch.checkout 被 adapter 的事务边界拒绝

预期：在没有外层事务的 connection 上调用 checkout，改变 active Branch。实际：SQL scalar 与普通 Native execute 都返回 `TRANSACTION_BOUNDARY_REQUIRED: Branch checkout requires SQLite autocommit mode`。

最小复现：空库加载扩展后，不执行 BEGIN，依次运行：

```sql
SELECT lithograph_init();
SELECT lithograph('CALL lithograph.branch.create(''probe'')');
SELECT lithograph('CALL lithograph.branch.checkout(''probe'')');
```

最后一条是**预期复现错误**，不是成功教程。Native 复现可编译 [C 示例](../guide/examples/native_transaction.c) 后运行 `native_transaction <extension> --probe-checkout`。

代码证据：adapter 对 write 使用 [内部 savepoint](../../crates/lithograph-extension/src/execution.rs)，普通 Native [同样包装](../../crates/lithograph-extension/src/native.rs)，执行器又要求 [checkout 时 autocommit](../../crates/lithograph-core/src/query/stream/write.rs)。这个组合造成入口行为与设计不一致。

**规避：每次 query 显式传 `options:{"branch":"probe"}`。**读历史用 `at`，SQL / Native explicit transaction 在 begin 中指定 branch。无需 checkout 就能创建、读写、合并目标 Branch。不要直接改内部 connection state 或库表来绕过错误。

## DOC-V010-02：Native NULL JSON 不自动使用空 object

预期：可省略的 params/options 支持默认空 object。实际：`NULL,0` 被解码为空字符串，不能作为有效 JSON；普通 execute 的 params/options 分别返回 `INVALID_ARGUMENT`，tx_execute 的空 params 也复现了同类失败并自动 abort。

**规避：传入 `"{}",2`**，长度是两个 UTF-8 bytes。tx_begin 不需要自定义 options 时也显式传 `{}`，不要依赖空指针默认行为。非空对象同样使用准确 bytes length。

普通 execute 最小复现由 C 示例的 `--probe-null-json` 输出，分别检查 params 和 options；源码入口为 [decode_native_execute_inputs](../../crates/lithograph-extension/src/native.rs)。SQL Bridge 省略第二/第三参数的 `{}` 默认值不受此问题影响。

## DOC-V010-03：Procedure introspection 参数/结果 metadata 不完整

`SHOW PROCEDURES YIELD *` 的 `returnDescription[*].type` 在 v0.1.0 全部写成 `STRING`；`argumentDescription` 也为空。实际返回值并非全是字符串，例如 `branch.list.active` 是 BOOLEAN、`commit.get.parents` 是 LIST、`diff.patch` 是 MAP、全文 score 是 FLOAT。

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

上述发布问题均有明确规避路径；v0.2.1 未宣称修复。后续发布若修复，应针对新制品重新验证、更新该版本文档，不把 v0.1.0 的原始证据静默抹掉。
