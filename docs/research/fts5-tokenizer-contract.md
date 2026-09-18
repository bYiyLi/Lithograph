# SQLite FTS5 Tokenizer Contract 研究证据

核验记录：2026-09-16 UTC；仓库检查基线 `2cbca18`。本页保存外部合同、实际观察和适用限制，不定义 Lithograph 产品行为；设计真源见 [Full-text](../design/full-text.md)。

## 1. 权威来源

| 标识 | 来源 | 本次用途 |
| --- | --- | --- |
| S1 | [SQLite FTS5 §4.3 Tokenizers](https://www.sqlite.org/fts5.html#tokenizers) | `tokenize` specification、内层引号和内置 tokenizer |
| S2 | [SQLite FTS5 §7 Extending FTS5](https://www.sqlite.org/fts5.html#extending_fts5) | 当前 connection 的 API、tokenizer 查找/注册和 API version guard |
| S3 | [SQLite FTS5 §7.1 Custom Tokenizers](https://www.sqlite.org/fts5.html#custom_tokenizers) | constructor arguments、DOCUMENT/QUERY/PREFIX/AUX、synonym、错误和释放 |
| S4 | [Neo4j Cypher 25 Full-text indexes](https://neo4j.com/docs/cypher-manual/25/indexes/semantic-indexes/full-text-indexes/) | 公共 DDL/config/procedure、analyzer 与 score 的含义 |

网页会更新。本次保留相关语义摘要和核验日期，不把当前页面的新 API 自动当成 SQLite 3.45.0 已具备的能力，也不把 Neo4j 当前页面的后续 additions 自动并入冻结的 `CY25-2026.08`。

## 2. FTS5 specification 是已有配置格式

S1 规定 `tokenize` 选项值可用 SQL text literal 包裹；该字符串内部是由空白分隔的 FTS5 bareword 或单引号文本项。首项选择 tokenizer，后续项作为有序字符串数组交给 tokenizer constructor。内层引号与外层 SQL literal 是两次独立编码；不能简单按空格拆开。

`porter unicode61` 中 Porter 是 tokenizer wrapper，使用后面的 tokenizer 先分词，再做 Porter 处理。`unicode61` 是 FTS5 的默认 tokenizer。`remove_diacritics` / `tokenchars` 等由具体内置 tokenizer 定义，不是所有 tokenizer 通用的参数。

`content`、`detail`、`prefix` 等是其它 FTS5 table options，不属于 tokenizer arguments 的通用透传配置。某 extension 注册了不同 virtual-table module，也不意味着它可被 FTS5 `tokenize` 选中。

## 3. 注册、生命周期和 API 限制

S2 的 `fts5_api` 属于具体 SQLite database connection；可通过 `SELECT fts5(?1)` 与 `sqlite3_bind_pointer` 获取。`xCreateTokenizer` 注册、`xFindTokenizer` 查找，后者可以支撑 SQLite Porter 一类的委托 wrapper。所列 API 没有通用 tokenizer 全量枚举、语义版本号或词典内容指纹接口。

S2/S3 同时记录 legacy 与 v2 API。v2 查找/注册方法只能在 `fts5_api.iVersion >= 3` 时访问；低版本直接访问属于未定义行为。两种注册 API 的 tokenizer 可通过查找 API 取得，legacy API 不携带 locale。最低 SQLite gate 与实际宿主能力必须分别验证，不能仅按编译 header 作判断。

S3 明确区分 DOCUMENT、QUERY、QUERY|PREFIX 与 AUX 调用。回传 token 包含 byte length、offsets 和可标记同位置 synonym 的 flags；将 query 插成 document 后读取去重词集合不能代表任意 tokenizer 的 query behavior。每个成功 constructor 都需要对应一次 destructor；callback 失败需要传播。

同名注册会替换原 tokenizer。注册名本身不是稳定算法身份；数据库文件保存配置，不会附带原 native implementation 或第三方词典。固定资源、避免 live replacement、管理 connection 生命周期是应用集成需要承担的边界，不能用一次 FTS table 构建成功证明长期可复现。

## 4. Cypher surface 与 provider behavior 不可混为一谈

S4 提供 `CREATE FULLTEXT INDEX`、`OPTIONS/indexConfig`、`fulltext.analyzer`、`fulltext.eventually_consistent` 和 Node/Relationship query procedures。Neo4j 的 analyzer 字符串选择其 analyzer catalog 中的名称，不是一个携带 FTS5 参数的规范。

因此使用相同配置 key 不证明 FTS5 specification 能原封不动移植到 Neo4j。特别是 Neo4j 的 `english` 包含 stop-word 行为，不能把 SQLite `porter unicode61` 称为同一算法；Neo4j 默认 analyzer 名称也不是 FTS5 注册名。

S4 允许查询和文档使用不同 analyzer，并按相关性降序返回 score；全文查询还包含短语、布尔和属性限定。其相关性数值与后台 eventual-refresh 时序不能直接等同于 FTS5。验证应分别覆盖公共查询结构与明确的 backend binding，不能通过修改 oracle 预期把差异计成兼容通过。

## 5. 当前仓库证据与缺口

| 位置 | `2cbca18` 的观察 |
| --- | --- |
| `query/schema/command/index_config.rs` | 默认 `standard-no-stop-words`，只接受它与 `english` |
| `storage/schema_state.rs` | FullText configuration 已保存 `analyzer: String` 和 BOOLEAN，可容纳完整 specification |
| `query/schema/show.rs` | SHOW options 已从目标 definition 输出 analyzer |
| `query/semantic_index.rs` | 两个硬编码映射；TEMP FTS cache key 已含 Snapshot identity 和完整 IndexDefinition |
| `query/schema/execute.rs` | Schema 发布后处理 Standard Index；Full-text 没有 constructor probe |
| `query/semantic_index.rs` 的 override 路径 | query 被插为文档，经 `SELECT DISTINCT term` 与 vocab 匹配；结构上不保留短语次序/布尔组合或 QUERY flag |
| `tests/phase08_search_ingestion.rs` | 已有 Node/Relationship、multi-property、历史、DROP、cache 删除重建、visibility-before-pagination 和一个简单 analyzer override；没有任意注册 tokenizer/参数完整验收 |

上表代码路径相对 `crates/lithograph-core/src/`；最后一行测试路径相对 `crates/lithograph-core/`。它们是设计输入，不是 Phase 12 已实现证据。

## 6. 本次轻量探针与未验证项

在 Yi Mac 的 Python SQLite **3.51.0**、纯 `:memory:` database 上，使用 specification `unicode61 remove_diacritics 0 tokenchars '-_'`，按外层 SQL 单引号转义创建 FTS5 表，并以绑定参数写入/查询 `alpha-beta`，实际得到预期命中。此探针仅证明该 quoting 示例在该 SQLite runtime 可执行，不证明 Lithograph 已能接收该配置。

现有本地 debug/release 二进制均报告 extension `0.0.0`，不作为当前源码或 v0.1.0 发布制品的证据；补充查询探针遇到 `LITHOGRAPH_BUSY`，没有获得 override 的运行结果。本次对 override 缺口的判断以代码路径与 S3/S4 合同对照为依据，Phase 12 必须在受控测试中补足复现和修复验证。未在本次文档任务重新编译引擎或执行全套 Rust/ABI/compatibility gates。

不固定真实 Jieba 插件依赖，不声称任何项目都注册名为 `jieba` 或接受 `search`。Phase 12 使用最小 synthetic SQLite tokenizer extension 检查 names/args/flags/失败路径；真实第三方词典质量和算法兼容性不属于该机制测试的证明范围。
