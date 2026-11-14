<a id="lithograph-developer-documentation"></a>

# Lithograph 开发者手册

本手册面向将 Lithograph 嵌入自己产品的开发者，适用版本为 **v0.3.6**。从安装、图查询到版本管理、搜索与维护，公开能力的使用说明均可在本手册内查到。旧版本行为见对应 [Release Notes](../releases/) 和 tag；本版本变化见 [v0.3.6发布说明](../releases/v0.3.6.md)。

v0.3.6新增查询成本规划、有界Join/流式执行和自动checkpoint等性能改进，继承v0.3.5的安全谓词下推与Node Element ID Seek、v0.3.4的Graph View `requireAnyLabels`及既有访问控制。公开SQL、Cypher Profile、storage format 3和Provider SPI V1保持；章节中标明v0.3.4等版本的功能起点和历史兼容边界继续有效。实际发布及制品验收见[Phase 22发布记录](../development/phases/22-high-performance-query-optimizer.md#v036-release-delivery)。

<a id="v030-基线"></a>

## 版本与运行要求

| 项目 | 本版本 |
| --- | --- |
| Cypher Profile | 冻结的 `CY25-2026.08` |
| 应用执行入口 | SQLite SQL：`lithograph()` / `lithograph_rows()` |
| 多 execution 单 Commit | `lithograph_tx_begin()` → 普通 execution → `lithograph_tx_commit()` / `lithograph_tx_abort()` |
| 安全执行 | 既有 `readOnlyNoExternalIO`；v0.3.2 新增 readAccess/writeAccess，缺省不额外限制 |
| Embedding Provider SPI | `EmbeddingProviderV1` |
| Lithograph storage format | 读 1–3；新建/迁移目标 3 |
| SQLite | 3.45.0+，支持 loadable extension、FTS5 与线程安全 |

Lithograph 是嵌入式 SQLite extension。一个 SQLite connection 的 `main` database 承载一个版本化 Property Graph；Branch/Commit 表示同一图的不同状态。宿主普通 SQL 表可以共存，`_lithograph_*` 是保留命名空间。

<a id="阅读路径"></a>

## 三个阅读入口

- **按任务学习**：从本页的指南开始，沿步骤完成一个可检查的结果。
- **查完整语言**：[Cypher 25 使用手册](../cypher/index.md) 提供语法、表达式、类型、Path、Schema、搜索与全部函数。
- **查准确接口**：[Reference](../reference/README.md) 提供 SQL、options、编码、事务、Procedure、Provider、错误和限制。

第一次接入：**[安装](installation.md) → [快速入门](getting-started.md) → [应用集成](integration.md)**。

## 按任务查找

| 使用目标 | 指南 |
| --- | --- |
| 选择制品、加载、自检与升级 | [安装](installation.md) |
| 从空数据库创建图并读取历史 | [快速入门](getting-started.md) |
| 学习图模型和基本 Cypher | [Graph 与 Cypher](graph-and-cypher.md) |
| 参数绑定、流式消费、取消、连接池、Python/C | [应用集成](integration.md) |
| 显式限制 execution 工作内存（v0.3.3） | [内存预算接入](integration.md#execution-memory-budget) |
| 普通执行、外层 SQL、多 execution 单 Commit 与 batch 的选择 | [事务与并发](transactions.md) |
| 限定查询和修改可见子图 | [Graph View](graph-views.md) |
| 多 Label OR 并集筛选（v0.3.4） | [Graph View 多 Label 并集](graph-views.md#label-union) |
| 强制只读并阻止外部 I/O | [安全执行](safe-execution.md) |
| 控制字段查询、图对象返回和写入权限（v0.3.2） | [访问控制](access-controls.md) |
| Schema、Constraint、Graph Type 与 Index | [Schema 与 Index](schema-and-indexes.md) |
| Full-text、Raw Vector、Managed Semantic | [Search](search.md) |
| 参数化导入、CSV、batch 和错误检查 | [数据导入](import.md) |
| Branch/Tag/Data/History、Diff/Patch 和历史改写 | [版本管理](versioning.md) |
| 创建 Session、分页解决冲突、验证候选图和 CAS | [Merge](merge.md) |
| 加载和配置随附 Embedding Provider | [Embedding Provider 使用](embedding-providers.md) |
| 构建自定义 Provider | [Provider 开发](provider-development.md) |
| 备份恢复、完整性、rebuild、GC、Provider cache | [部署与维护](operations.md) |
| 按错误现象定位并恢复 | [排障](troubleshooting.md) |

## Execution 选择

小/中等结果使用 `lithograph()` 获取完整 `{columns,rows,summary}`；大结果使用 `lithograph_rows()` 按 `columns → row* → summary` 消费。stream 同样可以执行 mutation、LOAD CSV、Managed Semantic 和 transaction subquery；内部 finalize-success 前关闭普通 side-effecting stream 会回滚未发布工作，详见 [生命周期](../reference/transactions.md)。

多条 Cypher 需要共同形成一个 Commit 时，在同一 connection 上使用 Lithograph explicit transaction。`CALL … IN TRANSACTIONS` 则通过普通执行入口在 SQLite autocommit mode 下使用，拥有独立 batch 提交语义。

<a id="示例"></a>
<a id="版本与历史"></a>

## 可运行示例与固定版本

[Examples](examples/README.md) 包含 Python quickstart、完整版本工作流、SQL 事务、streaming 和 SQLite C host 示例，并说明验证方式与环境要求。接口细节和当前限制分别见 [Reference](../reference/README.md) 与 [Known Issues](../reference/known-issues.md)。

v0.3.6 tag的固定入口为 [产品手册](https://github.com/bYiyLi/Lithograph/blob/v0.3.6/docs/guide/README.md)、[Cypher 手册](https://github.com/bYiyLi/Lithograph/blob/v0.3.6/docs/cypher/index.md)、[接口参考](https://github.com/bYiyLi/Lithograph/blob/v0.3.6/docs/reference/README.md)。固定 tag 的正文与二进制版本对应；主分支代表当前开发内容，实际 tag/发布进度由 [Phase 22](https://github.com/bYiyLi/Lithograph/blob/main/docs/development/phases/22-high-performance-query-optimizer.md#v036-release-delivery) 维护。

v0.3.0 起应用执行统一 SQL-only，Provider 自己维护 persistent embedding cache。v0.2.0/v0.2.1 的 Native query ABI、专用 tx_execute 和 format 4 属于历史版本；切换旧数据库前阅读 [升级边界](installation.md)。许可见 [LICENSE](../../LICENSE) 与 [Commercial License](../../COMMERCIAL-LICENSE.md)。
