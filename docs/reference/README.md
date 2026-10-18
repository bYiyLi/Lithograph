<a id="lithograph-reference"></a>

# Lithograph 接口参考

适用版本：**v0.3.2**；v0.3.1 不接受新增 readAccess/writeAccess。本目录提供公开接口的精确说明；操作流程见 [产品手册](../guide/README.md)，语言规则见 [Cypher 手册](../cypher/index.md)。

主分支另列 **Unreleased / Phase 19** 的 [memoryLimitBytes](execution-options.md#memory-limit) 和长表达式取消改进；v0.3.2 发行库不接受新 option。实际交付状态见 [Phase 19](../development/phases/19-query-correctness-resources.md)。

| 查找内容 | 正文 |
| --- | --- |
| SQL 入口、result envelope、summary、stream event | [SQL API](sql-api.md) |
| execution/begin/transaction 内 options、Graph View、安全选项、Unreleased 内存预算 | [Execution Options](execution-options.md) |
| JSON tagged values、整数精度、参数和结果编码 | [Values](values.md) |
| transaction/execution/cursor 生命周期与失败清理 | [Transactions](transactions.md) |
| Descriptor、Commit/Data、History、Patch 与 Merge 对象 | [Versioning](versioning.md) |
| 全部函数及 overload、分类与用法 | [函数索引](functions.md) |
| 全部命名 Procedure 的准确接口 | [Procedures](procedures.md) · [运行时清单](procedure-inventory.md) |
| 随附 Provider 的完整配置、HTTP 和独立 cache | [OpenAI-compatible Provider](openai-compatible-provider.md) |
| 自定义 Provider 注册、callback、ownership 和错误 | [Embedding Provider SPI](embedding-provider-spi.md) |
| SQLite entrypoint 与保留 FFI 边界 | [Native Surface](native-api.md) |
| Cypher Profile、支持/排除范围和后端差异 | [Cypher Compatibility](cypher-compatibility.md) |
| 错误 code、分类与 SQLite 映射 | [Errors](errors.md) |
| 平台、资源限制和参数范围入口 | [Limits](limits.md) |
| 当前版本缺陷与历史问题 | [Known Issues](known-issues.md) |

应用通过标准 SQLite SQL 调用 Lithograph；Provider SPI 面向 Provider 作者。SQL options、Cypher 参数和 Procedure 的 options 是不同输入，以各自正文为准。每个接口说明的限制与事务语义也适用于清单中已登记的名称。

固定版本入口：[v0.3.2 Reference](https://github.com/bYiyLi/Lithograph/blob/v0.3.2/docs/reference/README.md)；tag 与发布进度见 [Phase 18](https://github.com/bYiyLi/Lithograph/blob/main/docs/development/phases/18-query-access-controls.md#v032-release-delivery)。
