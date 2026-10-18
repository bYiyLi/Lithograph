# 版本边界、类型与配置限制

适用版本：**v0.3.2** · [产品手册](../guide/README.md) · [接口参考](README.md)

本页维护跨主题的平台与资源边界，以及各参数限制的查阅入口；完整配置表由对应语言/API 专题维护。内部 cache budget、性能实测和机器容量不构成通用硬上限。

<a id="platform-and-storage"></a>

## 平台与存储

SQLite 最低 **3.45.0**，必须允许 extension loading 并支持 FTS5。一个 SQLite `main` database 对应一个 graph repository；ATTACH 的其它 schema 不参与，操作另一个图必须把该文件作为另一连接的 main 打开。单文件的多个 Branch 共享 SQLite writer。

预编译包提供 Linux/macOS/Windows x64/arm64；宿主 OS、libc、CPU 架构和 SQLite binding 必须匹配，发布资产的存在不代表任意运行环境均可加载。部署前执行实际 `.load`、version/init 和 SQL smoke；环境步骤见 [安装](../guide/installation.md)。

fresh/current storage format 为 **3**，扩展 min/max 与数据库 current 以 `lithograph_version()` 为准。v0.2.0/v0.2.1 曾使用的 format4 是历史 release contract，当前不提供 format4 migration/compatibility；高于当前范围的库须用对应 binary/tag 处理。不得手改 marker、改变宿主 `PRAGMA user_version` 或直接修改保留的 `_lithograph_*` namespace。

<a id="names-descriptors-json"></a>

## 名称、Descriptor 与 JSON

Branch/Tag name 是大小写敏感 UTF-8，1–255 bytes。禁止 NUL/ASCII control、开头或结尾 `/`、空路径段、`.` / `..` 路径段；main 不可删除。这个限制不自动适用于 Cypher Label、Relationship Type 或 Property identifier。

Descriptor、Session revision、分页 cursor、Patch 等完整字段规则见 [Versioning Reference](versioning.md)。query `branch` 接受 name，不接受 `branch/` Descriptor；`at` 接受完整 Descriptor，见 [Execution Options](execution-options.md)。

INTEGER 为 signed 64-bit，JSON safe range 外使用 Integer tag。SQL scalar 总 envelope、stream 的单行/summary TEXT 均受宿主 `SQLITE_LIMIT_LENGTH` 和实际资源限制；streaming 不免除单个值必须可编码的要求。类型、嵌套 tag 和宿主精度规则见 [Values](values.md)。

<a id="execution-resources"></a>

## 执行与资源

- 一个 connection 的 active execution cursor 必须及时完整消费或关闭；慢速 side-effect consumer 会延长 writer hold。
- `ORDER BY`、DISTINCT、聚合等 semantic barrier 可使用 bounded memory 和 TEMP/disk spill；配置宿主临时目录与磁盘空间时需计入这部分成本。
- SQLite busy timeout、取消、线程模式和单个 statement 的资源限制属于宿主配置；query options 不提供任意 timeout/readOnly 配置。
- Lithograph explicit transaction 从 begin 起取得 writer，纯读长任务优先使用普通 pinned read；事务与连接池规则见 [Transactions](transactions.md)。
- `readOnlyNoExternalIO` 限制本次 Cypher 的 mutation/外部 I/O，允许当前 SQLite database、TEMP/spill 等正常工作；它不创建 OS sandbox。精确 allowlist 见 [安全选项](execution-options.md#read-only-no-external-io)。

<a id="full-text-index-config"></a>

## Full-text indexConfig

Full-text DDL 的 `fulltext.analyzer`、`fulltext.eventually_consistent` 默认值、类型、FTS5 tokenizer specification 规则由 [Index 语言参考](../cypher/indexes.md) 维护。query string、query options、analyzer 运行要求和 score 见 [Full-text Procedure](procedures/fulltext.md)。

第三方 tokenizer 必须在**每个实际 SQLite connection** 注册；数据库只版本化 specification，不持久化 native 实现。Derived cache/eventual-consistency 不改变历史 Snapshot 的结果正确性要求。FTS5 tokenizer 名称不等同于 Neo4j analyzer 名称，不能依赖自动 alias。

<a id="vector-index-config"></a>

## Vector indexConfig

Vector 支持 I8/I16/I32/I64/F32/F64，dimension 为 1–4096，编码见 [Vector value](values.md#temporal-point-uuid-vector)。DDL 的 dimensions、similarity、quantization、expansion、HNSW 参数完整范围与优先级见 [Index 语言参考](../cypher/indexes.md)，SEARCH 的参数与执行规则见 [Search 语言参考](../cypher/search.md)。

Index OPTIONS 与 query options 是不同输入，不能互换。索引参数不承诺延迟 SLA；维度、过滤条件、数据分布、Recall 与内存需求由应用实际 workload 验证。

<a id="managed-semantic-limits"></a>

## Managed Semantic 限制

Managed Semantic 只接受单个 STRING source Property，按 exact UTF-8 bytes 工作，不自动 trim/lower/concat/chunk/truncate；输出为 finite FLOAT32。Core 不提供 persistent embedding result cache；execution-local 去重与 TEMP work 不替代 Provider cache。

| 查阅内容 | 唯一完整参考 |
| --- | --- |
| create/query/rebuild 参数、dimensions、limit/skip、历史与事务边界 | [Semantic Procedure](procedures/semantic.md) |
| endpoint/auth、batch、timeout/retry、redirect/proxy、独立 cache | [OpenAI-compatible Provider](openai-compatible-provider.md) |
| SPI、semantic identity、内存所有权、取消和错误 | [Embedding Provider SPI](embedding-provider-spi.md) |
| 加载、配置和完整查询流程 | [Provider 指南](../guide/embedding-providers.md) |

实际 semantic query/rebuild 即使 cache 已 warm 仍要求目标 Provider 当前可用。Provider config 中直接写入的 secret 会进入 versioned history，环境变量引用只保存变量名；配置前按 Provider Reference 选择 credential 输入。安全选项启用时禁止 Managed Semantic，不因 cache hit 而豁免。

<a id="不属于此产品表面的能力"></a>
<a id="product-surface"></a>

## 产品表面与兼容范围

Lithograph 作为标准 SQLite loadable extension 使用。独立 Server、Bolt/HTTP 图服务、账号角色管理、APOC 或远程图同步不属于本产品表面。随附 OpenAI-compatible Provider 是独立 SQLite extension，通过 `EmbeddingProviderV1` 连接，不是 Kernel 内嵌模型服务。

Cypher 支持范围和排除范围见 [Compatibility](cypher-compatibility.md)。当前缺陷与规避见 [Known Issues](known-issues.md)；宿主未启用 extension loading 属于环境前提，不把它记录为产品缺陷。
