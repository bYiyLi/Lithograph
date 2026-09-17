# 版本边界、类型与配置限制

**版本：v0.1.0。** 本页列出应用可依赖的入口约束，不把内部 cache budget、性能实测或机器容量当作通用硬上限。

## 平台与存储

SQLite 最低 3.45.0，必须可加载扩展并支持 FTS5。仅 connection 的 main 承载 graph repository；单文件多 Branch 仍共享 writer。预编译包提供 Linux/macOS/Windows x64/arm64，不等于保证任意 Linux libc、任意旧 OS 或所有 SQLite binding 都相容；必须在实际部署运行时 smoke-test。

v0.1.0 读取支持格式 1–3，新建使用 3。旧格式写入前需要显式 init 迁移；无法将 format 3 自动降级。内部 `_lithograph_*` namespace 保留，不插入、删除、重命名或加 trigger/index。

## 名称、Descriptor 与 JSON

Branch/Tag name 是大小写敏感 UTF-8，1–255 bytes。禁止 NUL / ASCII control、开头或结尾的 `/`、空路径段、`.` / `..` 路径段。main 不可删除。不要把这一名字限制自动套用于所有 Cypher Label / Property identifier。

Commit Descriptor 为 `commit/<64 hex>`；branch/tag Descriptor 为对应前缀加 name。query `branch` 只接受 name。revision/分页 limit 必须为正整数，cursor 原样传回同一查询起点/Session revision。

INTEGER 是 signed 64-bit；跨 JSON 的大整数用 [Integer tag](values.md)。SQL scalar 总 envelope 与每个单独 SQLite value 受宿主 `SQLITE_LIMIT_LENGTH` 等限制；没有一个与运行时配置无关的“无限 JSON 返回值”。过大结果应投影必要字段、分页或使用 streaming，但单行仍需要可编码。

Vector 支持 I8/I16/I32/I64/F32/F64 坐标；输入也接受相应 `INTEGER8/16/32/64`、`FLOAT32/64` 等别名。输出 canonical coordinateType 为 `I8` 等短名，不保证保留输入的别名拼写。Vector dimension 必须与 values 长度一致，坐标必须在对应类型允许范围内。

## Full-text indexConfig

| Key | 默认 | 允许值 |
| --- | --- | --- |
| `fulltext.analyzer` | `unicode61` | 非空、无 NUL、且当前 SQLite connection 能由 FTS5 成功构造的完整 tokenizer specification |
| `fulltext.eventually_consistent` | `false` | BOOLEAN |

通过 `OPTIONS {indexConfig:{...}}` 声明。specification 直接遵守 FTS5 tokenizer grammar，例如 `porter unicode61` 或 `unicode61 remove_diacritics 0 tokenchars '-_'`；Lithograph 不再把 `standard-no-stop-words` / `english` 映射到内置实现。第三方 tokenizer 由宿主 SQLite extension 在**每个实际 connection** 上注册；数据库只版本化 specification 字符串，不持久化 native 实现。派生缓存和 eventual-consistency metadata 不改变历史 Snapshot 的正确性要求。

## Vector indexConfig

| Key | 默认 | 允许值 / 范围 |
| --- | --- | --- |
| `vector.dimensions` | 未显式固定 | INTEGER 1–4096 |
| `vector.similarity_function` | `cosine` | `cosine`、`euclidean` |
| `vector.quantization.type` | `binary` | `none`、`scalar`、`binary` |
| `vector.quantization.enabled` | 未给出 | legacy BOOLEAN；未给 type 时 true→scalar、false→none |
| `vector.default_search_expansion_factor` | none:1.0；scalar:1.5；binary:3.0 | 有限数值，1.0–10000.0 |
| `vector.hnsw.m` | 16 | INTEGER 1–512 |
| `vector.hnsw.ef_construction` | 100 | INTEGER 1–3200 |

提供 type 时以 type 为准；新应用避免同时提供 legacy quantization flag。Full-text / Vector OPTIONS 只接受 `indexConfig` Map，其中仅允许各 family 的已知 key，值必须为 DDL 支持的 literal。不要把 Query options 放进 Index OPTIONS，反之亦然。

上述参数决定索引配置，不是延迟 SLA。合理维度、过滤条件、数据分布、Recall 与内存取舍应由应用自己的 workload 验证。

## 不属于此产品表面的能力

没有独立 Server、远程图同步协议、Neo4j Bolt/HTTP 服务、账号角色管理、APOC 安装、内置 embedding provider 或 SDK 包安装承诺。Graph View 是执行子图边界，不是安全授权。

更多兼容范围见 [Cypher Compatibility](cypher-compatibility.md)。依据：[Index config parser](../../crates/lithograph-core/src/query/schema/command/index_config.rs)、[Value types](../../crates/lithograph-core/src/cypher/value.rs)、[设计](../design.md)。
