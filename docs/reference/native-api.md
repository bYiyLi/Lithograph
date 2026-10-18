# Native / FFI Surface

适用版本：**v0.3.2**。上级：[接口参考](README.md) · 应用接入：[Integration](../guide/integration.md)。

<a id="sqlite-entrypoint"></a>

## SQLite loadable extension

Lithograph 的 C ABI 入口是 `sqlite3_lithograph_init(sqlite3 *db, char **pzErrMsg, const sqlite3_api_routines *pApi)`，由 SQLite 的 `sqlite3_load_extension()` 或宿主 driver 的等价功能调用。最低 SQLite 版本为 3.45.0；应用还需启用 extension loading 与 FTS5。所有 SQLite 调用使用该宿主 API table。

应用通过标准 SQLite API 加载 Lithograph 后，调用 [SQL API](sql-api.md) 的 `lithograph()`、`lithograph_rows()` 和 `lithograph_tx_*()`。C/C++ 示例见 [native_transaction.c](../guide/examples/native_transaction.c)，它使用 SQLite C API 与 SQL execution surface。

<a id="embedding-spi"></a>

## Embedding Provider SPI

`EmbeddingProviderV1` 是独立 Provider extension 与 Lithograph 之间的 C-compatible SPI，公开声明位于 [lithograph_embedding_provider.h](../../include/lithograph_embedding_provider.h)。Provider 在同一 connection 上注册本地 validation、embedding 与 release callbacks，具体布局、status、ownership 和生命周期见 [Embedding Provider SPI](embedding-provider-spi.md)。

随附 `lithograph-openai-compatible` 是第二个平级 SQLite extension；其 entrypoint 为 `sqlite3_extension_init` / `sqlite3_lithographopenaicompatible_init`，注册名为 `openai-compatible`。使用步骤见 [Embedding Providers](../guide/embedding-providers.md)，完整配置见 [Provider Reference](openai-compatible-provider.md)。自定义实现沿用同一 SPI，见 [Provider Development](../guide/provider-development.md)。

<a id="application-boundary"></a>

## 应用边界与旧版本

v0.3.1 不提供 application-facing Cypher execution、validation 或 explicit-transaction C API。Provider SPI 不赋予宿主第二套 query entrypoint。旧版本的 query C ABI 只适用于其对应 release tag / notes，不能用于当前动态库。

Release artifact checks 同时验证 legacy query symbols 未导出、Provider SPI 与真实双扩展加载。检查发布状态应查 [Release Notes](../releases/)，不能从头文件存在推断某个平台已经发布。
