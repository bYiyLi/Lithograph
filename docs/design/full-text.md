# Full-text / FTS5

[设计入口](../design.md) · [开发状态与验收](../development/README.md)

本文件拥有 Full-text 的 FTS5 specification、Schema validation、query-time analyzer 与 cache/历史合同。公共 Schema/Index 规则见 [Schema / Index](schema-and-indexes.md)，adapter/transaction 限制见 [接口合同](interfaces.md) 与 [Storage](storage.md#transactions-and-concurrency)。


<a id="scope"></a>

## 职责与边界

SQLite FTS5 是唯一 Full-text backend。Lithograph 负责 Node/Relationship Full-text DDL、versioned IndexDefinition、multi-label/type、multi-property、query procedures、score、Graph View、历史 Snapshot 和 derived cache 生命周期；真正的分词算法与 tokenizer 参数解释由当前 SQLite connection 上注册的 FTS5 tokenizer 实现负责。

应用可以在同一 connection 上加载 Lithograph 与第三方 SQLite tokenizer extension，或通过 SQLite FTS5 API 注册 tokenizer。只要求相关执行发生前已经注册，不规定两个 extension 的加载先后。Lithograph 不安装、查找或自动加载第三方动态库，不启用宿主的 extension-loading 权限；不预先调用 Jieba 把文本改造成词序列后再写入 FTS5，也不建立通用 tokenizer/backend plugin system。

本节只开放 FTS5 原生 tokenizer contract。独立全文 virtual-table engine、FTS3/4 tokenizer、普通 SQL 分词函数，以及 FTS5 的自定义 rank/auxiliary function、`content`、`detail`、`prefix`、`locale` 等建表选项，不因此成为 Lithograph 新的公共配置。Vector/HNSW、`SEARCH` 与 KG OS 不在本次范围。

<a id="fts5-specification"></a>

## Cypher 配置与 FTS5 specification

保留既有 Cypher 25 入口，不增加 `TOKENIZER` clause、`fulltext.tokenizer` 或独立 arguments key：

```cypher
CREATE FULLTEXT INDEX doc_text
FOR (d:Doc) ON EACH [d.title, d.text]
OPTIONS {
  indexConfig: {
    `fulltext.analyzer`: 'unicode61',
    `fulltext.eventually_consistent`: false
  }
}
```

创建索引后，单独执行查询；两者不是一条复合 DDL：

```cypher
CALL db.index.fulltext.queryNodes('doc_text', $query, {skip: 0, limit: 10})
YIELD node, score
RETURN node, score
```

Relationship 使用 `CREATE FULLTEXT INDEX ... FOR ()-[r:TYPE]-() ON EACH [...]` 与 `db.index.fulltext.queryRelationships`，返回 `relationship, score`。多 Label/Type 和 Property 的含义不变。

| 配置 | 类型 / 省略时默认 | 目标含义 |
| --- | --- | --- |
| `fulltext.analyzer` | STRING / `'unicode61'` | 完整 FTS5 tokenizer specification；按 SQLite 的规则读取 name 和有序参数 |
| `fulltext.eventually_consistent` | BOOLEAN / `false` | 继续版本化保存；不放松目标 Snapshot correctness，也不新增异步 worker |

`eventually_consistent` 的两个值都沿用按需构建 cache、按选定 Snapshot 同步取得正确查询结果的机制；目前它只是 versioned metadata，不选择另一条刷新路径。它不允许返回其它 Commit 或过期 cache 的内容，也不是 Neo4j 后台刷新时序的实现承诺。

以下是 FTS5 specification 的例子，不是新增 Cypher 语法：

| analyzer 的字符串值 | FTS5 含义 |
| --- | --- |
| `unicode61` | 使用 SQLite Unicode tokenizer |
| `porter unicode61` | 使用 Porter wrapper，并把 `unicode61` 作为参数交给它 |
| `unicode61 remove_diacritics 0` | 把有序参数交给 `unicode61` |
| `jieba` | 仅当宿主实际注册了这个名字时，使用对应第三方 tokenizer |

第三方名字、支持的参数和含义以该 extension 的实际注册和文档为准。`jieba search` 仅在某个插件明确接受 `search` 参数时成立，不是 Lithograph 或 SQLite 定义的 Jieba 通用配置。内置 tokenizer 的可用范围同样取决于受支持的宿主 SQLite runtime。

STRING 的类型检查、NUL 拒绝和非空检查由 Lithograph 完成；FTS5 specification 语法、名字解析、参数接收和构造失败由 SQLite/tokenizer 验证。拒绝空字符串和只含 FTS5 分隔空白的 specification，不允许把它们当成省略配置，也不以 Unicode trim 擅自改变合法名称或参数。未知 Index OPTIONS/indexConfig key、错误类型和 `null` 继续明确拒绝；不把整个 options map 作为任意 FTS5 建表参数透传。DDL 继续使用当前支持的 literal OPTIONS 值，不顺带扩展参数化 DDL。

**安全编码规则**：先完成 Cypher 字符串解码，得到 specification 原文；再把这整个值编码为一个 SQL text literal，内部单引号必须成对转义。不得将未转义输入拼入 `execute_batch`，不得使用 shell quoting、简单 `split_whitespace` 或自行剥离 FTS5 内层引号。Property text 与 MATCH query 使用绑定参数；表名和内部列名由 Engine 生成，与 tokenizer 输入隔离。

例如 Cypher 使用双引号包围带 FTS5 内层单引号的值：

```cypher
OPTIONS {indexConfig: {
  `fulltext.analyzer`: "unicode61 tokenchars '-_'"
}}
```

得到的内部建表片段是 `tokenize='unicode61 tokenchars ''-_'''`。引号、空格、重复参数、Unicode 和参数顺序必须按 FTS5 contract 保留；它们既不是 SQL 指令，也不是 Lithograph 要理解的业务参数。需要瞬态解析以调用原生 API 时，解析规则必须与 FTS5 specification grammar 一致，并以直接 FTS5 建表作差分验证。

<a id="versioned-definition"></a>

## Versioned definition 与破坏升级

Canonical Full-text configuration 只保留 `analyzer: String`（完整 specification）和 `eventually_consistent: bool`。默认值在新建 definition 时显式写入；`SHOW FULLTEXT INDEXES` / `SHOW INDEXES` 的 options 返回目标 Snapshot 中保存的字符串。无需再保存第二份 name/arguments、插件路径、运行时 handle 或 tokenizer registry。

保存的是 Cypher 解码后的原始字符串，不做大小写折叠、参数排序或等价拼写归一化。即使两种拼写在某个 tokenizer 下等价，不同字符串仍是不同 definition；这允许保守地重建 cache，避免错误合并第三方配置。参数值、参数顺序或 tokenizer 名称变化，都必须被 definition equality、Schema hash、Diff/Patch 与 Merge 的既有 Index slot 识别。修改配置通过标准 DROP/recreate 或既有版本操作完成，不新增 ALTER FULLTEXT INDEX 方言。

本次允许破坏升级：删除 Lithograph 的 `standard-no-stop-words → unicode61`、`english → porter unicode61` 映射，不保留 legacy variant、双模型或 alias fallback。普通分词改写为 `unicode61`，原有 Porter 行为改写为 `porter unicode61`；后者并不等于 Neo4j/Lucene 的完整 `english` analyzer。旧名称只有作为真实 FTS5 tokenizer 注册名、并使用合法 specification quoting 时才可能可用；例如带连字符的名字需要 FTS5 内层引号，不能靠旧 Cypher STRING 的外层引号替代。Lithograph 不再赋予它们特殊含义。

已有 STRING 编码可容纳新 specification，本次不要求改变物理 storage format、重算既有 Commit/Schema ID 或增加迁移层。旧版本 analyzer 定义的可继续检索性不属于本次兼容承诺；不原地重写其历史配置、不自动删库或修补旧 Commit。应用可以使用新配置重建当前索引；这不会让旧 Commit 中的旧配置自动变成新配置。v0.1.0 Release Notes 和固定版本使用文档继续描述当时事实。

<a id="schema-validation"></a>

## Schema 执行验证与原子失败

普通 CREATE、SQL explicit transaction 中的 staged DDL，以及 Patch/Merge/Rebase/Revert 等创建新 canonical 状态并引入或更改 Full-text definition 的路径，都必须在发布新 Schema/Branch 状态之前，在实际执行的宿主 SQLite connection 上验证对应 specification。只验证相对本次目标 Branch base 新增/改变的定义，不为无关 Schema/graph 写入重新构造所有 tokenizer。纯 ref 移动（包括仅移动到已有 Commit 的 fast-forward）、历史 Schema inspection、DROP 和真正没有改变 definition 的 `IF NOT EXISTS` 不以 tokenizer 可用为前提；配置类型/结构验证仍不能被 `IF NOT EXISTS` 绕过。

最低成本的原生验证是：在当前 invocation/savepoint 内创建只有一个文本列的临时 FTS5 table，使用安全编码后的 specification，成功后删除探针。它检查实际 FTS5 解析、注册名与 tokenizer `xCreate`，不构建整个 graph corpus。缺失 tokenizer、无效 specification 或 tokenizer 报告的参数错误使本次 Schema mutation 失败，不能留下新的 durable Commit、Schema 引用、Branch move 或不完整探针。SQL explicit transaction 继续遵守任一 execution 失败整体 abort 的既有合同。

`xCreate` 成功只证明构造成功，不证明该插件会拒绝所有不认识的参数，也不证明任意文本的 `xTokenize` 永不失败。后续构建/查询遇到 tokenizer、I/O、资源或中断错误必须传播；不可把之前的探针成功当作允许返回部分结果或 fallback 的理由。

`EXPLAIN`、纯 prepare/validation 和 `SHOW` 不创建 FTS table、不执行 tokenizer 构造器、不隐式加载动态库；它们可以完成静态配置检查，但不宣称证明运行时可用性。执行时的探针不能被移入无副作用的 planning 路径。只做历史/reset 等 ref 操作允许指向当前不能检索的索引定义；真正检索时再按[运行环境、失败与可复现性](#runtime-and-failures)失败。

<a id="cache-and-history"></a>

## Cache 与历史 Snapshot

FTS physical content 继续放在 connection-local TEMP derived cache，不新增持久化全文 backend 或第二套 canonical storage。Cache key 覆盖完整 Snapshot state identity、完整 IndexDefinition 和 FTS provider cache encoding version；staged Snapshot 必须包含自身 revision，不能与已提交状态混用。定义同名但 tokenizer/参数不同，不得复用旧 cache；不以 current Branch head 的 definition 重建历史 Snapshot。

缓存只有完整构建成功并写入 connection-local readiness marker 后才可复用。构建中断或 tokenizer 文本处理失败必须先撤销 readiness；若当前宿主 SQLite statement 允许清理，则立即清空或删除本次 TEMP 中间态。真实 SQL scalar 执行中，SQLite 可能因为外层 statement 仍持有该 FTS virtual table 而对 DROP/DELETE 返回 `SQLITE_LOCKED`；此时允许保留**没有 readiness marker 的 quarantined root**，但它不属于可读 cache，后续任何使用都必须先成功 reset/rebuild，绝不能读取其中的部分内容。这样 cleanup 的物理回收可以延后，但 correctness 不依赖回收成功。Cache 被删除、connection 关闭或数据版本改变后，从目标 Snapshot 的 canonical graph 和 definition 重建。重建不创建 Commit、不移动 Branch、不写 `main`，也不使用额外 connection 来绕开注册或只读限制。宿主不允许所需 TEMP 操作时明确失败，不以忽略 tokenizer 的扫描代替。

节点/关系和多 Property 的既有文本抽取行为继续保留。创建 cache 直接把原文本交给 FTS5，分词由 FTS5 调用 tokenizer；不能通过存储预分词文本替换 canonical Property。读 cache 之前解析目标版本是否存在该 Index，DROP 后当前版本不再可用，但保留该 definition 的历史 Snapshot 仍按历史配置尝试重建。

<a id="query-time-analyzer"></a>

## Query-time analyzer 与结果语义

`db.index.fulltext.queryNodes` / `queryRelationships` 继续支持 `{skip, limit, analyzer}`。省略 query-time analyzer 时使用目标 IndexDefinition 的完整 specification；显式 analyzer 同样是 FTS5 specification STRING，仅影响本次查询文本的分词，不能改写 definition、产生 Commit 或用查询 tokenizer 重新解释已索引文档的 token。

`skip` 默认 0，省略 `limit` 不额外施加结果数限制；二者显式提供时必须为非负 INTEGER，拒绝 `null` 与错误类型。显式 analyzer 遵守[Cypher 配置与 FTS5 specification](#fts5-specification)的 STRING/非空/NUL 规则，不将空值当作“使用默认”。对实际执行的 procedure 调用，空图、空查询字符串或 `limit:0` 不能跳过配置有效性/可用性检查；配置有效的空查询与零 limit 返回空结果。没有输入行而根本没有执行 procedure 的情况，仍遵守既有 Cypher clause 执行规则。

FTS5 正常 MATCH 路径必须保留 tokenizer 的 DOCUMENT、QUERY、QUERY|PREFIX 调用区别、token 次序、byte offsets 与 colocated synonym；不能把 query 当作一篇文档插入临时索引，再通过 `DISTINCT term` / vocab 交集近似执行。`queryString` 仍是既有全文查询表达式，而非 tokenizer specification；短语、布尔组合、Property 限定和前缀等已支持语义不能因更换 tokenizer 或 analyzer override 而丢失。

当 query specification 与 index specification 相同时直接使用普通 FTS5 cache。当它们不同时，采用限定在 Full-text provider 内的 FTS5 原生委托适配：FTS5 发起 DOCUMENT/AUX tokenization 时委托 index tokenizer，发起 QUERY/QUERY|PREFIX 时委托本次 query tokenizer；token、位置、flags、callback 返回码原样转交，分词算法仍由外部 tokenizer 实现。Adapter instance 固定绑定 index specification 与 connection-local handle；query child tokenizer 只在一次 MATCH 的作用域内替换，作用域结束 exactly-once 恢复并释放。嵌套/重入 override 必须按 LIFO 保存和恢复上一层 child，不得因为另一个 analyzer 已在作用域中就串配置或使用 process/connection 全局“当前 analyzer”。适配器只有内部固定身份，不提供用户注册/安装 API，不可覆盖宿主同名 tokenizer，也不能递归选择自身。

不同 analyzer 的执行可从同一 Snapshot 原文本构建一份额外 TEMP FTS corpus，文档始终由 index tokenizer 分词，MATCH 期间由 scoped query child 分词。由于真实 SQL scalar 外层 statement 可能在内部 MATCH 返回后仍锁住参与执行的 virtual table，不能要求每次 procedure 调用结束时立即 DROP 该表；因此 corpus identity 只覆盖 `(Snapshot state identity, IndexDefinition, provider encoding version)`，**不包含 query analyzer 或 query string**。同一 Snapshot/IndexDefinition 的后续 override 复用这份 connection-local derived corpus，只重建本次 query child tokenizer；不同 analyzer 不产生无限增长的 TEMP table。corpus 删除、Snapshot/definition 改变或 connection 关闭后按 canonical graph 重建，不能跨 Snapshot/definition 复用，也不变成持久化 backend。普通无 override 的 warm query 不依赖该额外 corpus，也不承担其重建成本。

原生 API 使用宿主 FTS5 API，保留 SQLite 3.45.0 最低 runtime；v2 方法只能在 `fts5_api.iVersion >= 3` 时访问，不假设宿主和编译 headers 同版本。本次不暴露 locale 配置；可使用满足本次无 locale 合同的原有 API。需要的 FFI 只封装在小范围 provider adapter，覆盖构造部分失败、exactly-once 释放、callback error/panic containment 与 connection 生命周期，不放宽 workspace-wide unsafe 规则。

普通与 override 路径都通过 FTS5 MATCH/`bm25` 产生候选，以现有非负 score 映射按相关性降序排列，同分按 element identity 稳定排序。Graph View 的 Node/Relationship（包括 endpoint）visibility 必须先于 `skip`/`limit` 计数；不得让 hidden result 消耗分页名额。分数保持 FLOAT，不承诺与 Lucene 数值一致或可跨 query/不同 tokenizer 比较；修复旧 override 的词集合近似，不保留其错误评分算法作为兼容层。

<a id="runtime-and-failures"></a>

## 运行环境、失败与可复现性

Tokenizer 注册是 connection-local runtime capability，不在数据库文件中持久化。连接池的每个实际 connection、reopen 后的 connection、读取历史 Snapshot 的 connection，都由宿主负责注册所需 tokenizer。另一个 connection 注册成功不构成当前 connection 可用的证据。

缺少 tokenizer 或必要外部资源时，使用该配置的 CREATE、cache build/rebuild、实际 query 明确失败，绝不静默改用 `unicode61`；成功的零结果也不能掩盖已执行调用的配置错误。Schema 读取、`SHOW INDEXES` 和不检索全文的普通 graph read 不要求 tokenizer 当前可用。

| 失败位置 | 对外错误和状态要求 |
| --- | --- |
| DDL 配置形状/未知 key、FTS5 specification/名称或构造参数错误 | `SCHEMA_ERROR`；不发布该次 Schema/Commit/Branch 变化 |
| 查询 options 形状/未知 key、不可用 analyzer 或 FTS5 query 表达式错误 | `SEMANTIC_ERROR`；不退化为空结果或另一种 tokenizer |
| SQLite busy、I/O、只读、资源耗尽或中断 | 保留现有 `BUSY` / `IO_ERROR` / `RESOURCE_ERROR` 与 SQLite code；不统一伪装成“未注册” |
| 清理失败、adapter invariant 失败 | `INTERNAL_ERROR` 或现有更具体错误；遵守 fail-closed cleanup，不继续使用不确定的中间态 |

消息至少说明 Full-text Index 和失败阶段；不得为了诊断把完整第三方 arguments、私有词典路径或源文本无条件返回。第三方 extension 本身是宿主信任的 native code，Lithograph 不能 sandbox 它的文件访问、外部副作用或算法错误；SQLite rollback 只覆盖本次数据库状态。

Specification 本身会进入可由 SHOW/历史读取的 Schema，应用不应把 secret 放进 tokenizer arguments；错误脱敏不会把已主动写入历史的配置变成秘密。

Versioned specification 固定的是**名称与参数**，不是第三方二进制、词典文件或实现版本。要跨机器/reopen 重现同一 Commit 的全文结果，应用必须固定兼容的 SQLite runtime、tokenizer 实现及其外部资源。同一名字下热替换 tokenizer 或原地修改词典不在运行中 cache 的支持合同内；应用需结束相关 cursor、关闭并重新建立 connection。需要同时保留两种分词行为时，由应用使用不同注册名或插件支持的稳定参数表达，并保留历史所需资源。Lithograph 不伪造不存在的插件版本指纹/注册代次 API，也不承诺检测宿主背后的所有实现变化。

<a id="rationale-and-evidence"></a>

## 设计取舍与证据

采用一个 versioned specification STRING，而非新 Cypher keys、持久化 name/arguments 双模型或 analyzer 白名单：解决任意已注册 FTS5 tokenizer 和其参数可达性，代价是 analyzer 值不具 Neo4j 跨 backend 可移植性，且应用负责依赖版本。保留 query-time override 的原生委托边界，是为修复现有词集合近似不能保留查询结构的问题；不是给尚不存在的插件需求增加框架。

SQLite specification quoting、connection-local API、tokenizer flags、v1/v2 边界与 Neo4j analyzer 对照见 [FTS5 tokenizer 研究证据](../research/fts5-tokenizer-contract.md)。实施与全部验收见 [Phase 12](../development/phases/12-fulltext-tokenizer.md)；本节不以设计完成代替 runtime/test 证据。
