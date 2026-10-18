# Execution Options

适用版本：**v0.3.2**；v0.3.1 不接受新增 readAccess/writeAccess · [产品手册](../guide/README.md) · [接口参考](README.md)

本页另列 **Unreleased / Phase 19** 的 `memoryLimitBytes`，v0.3.2 发行库不支持该 key；实施与验收状态见 [Phase 19](../development/phases/19-query-correctness-resources.md)。

options 是 `lithograph()` / `lithograph_rows()` 第三个参数的 JSON object TEXT；省略等价于 `'{}'`。SQL `NULL`、JSON `null`、数组、malformed JSON、未知 key 或 key 的错误大小写均返回 `INVALID_ARGUMENT`。params 与 options 独立：查询里的 `$name` 从第二个参数读取，options 不用于参数绑定。

<a id="ordinary-execution"></a>

## 普通 execution

| Key | 类型与默认 | 语义 |
| --- | --- | --- |
| `branch` | 非空 STRING；默认 connection active Branch，初始为 `main` | 本次执行的 Branch **name**，不带 `branch/`；不改变 checkout |
| `at` | 非空 STRING；默认无 override | `commit/<id>`、`branch/<name>`、`tag/<name>` Descriptor；开始时解析并 pin 只读 Snapshot |
| `author` | STRING 或 null；默认 null | 新 Commit 的 author，不是账号认证 |
| `message` | STRING 或 null；默认 null | 新 Commit 的 message |
| `readOnlyNoExternalIO` | BOOLEAN；默认 `false` | 同时禁止本次 Cypher 的 mutation 和外部 I/O，完整规则见下节 |
| `memoryLimitBytes` | 正 JSON INTEGER，`1..9223372036854775807`；省略不新增限制；Unreleased | 单次 execution 的引擎计账内存预算，见下文 |
| `readAccess` | object；默认无附加限制；v0.3.2 | query 属性隐藏、返回图对象过滤与读取能力开关，见下文 |
| `writeAccess` | object；默认无附加限制；v0.3.2 | 属性、创建/删除/Label/Schema/版本/维护限制，见下文 |
| `graphView` | object；默认完整图 | `{requireAllLabels:[STRING],excludeAnyLabels:[STRING]}` |
| `mergeSession` | object；默认无 | `{id:"merge-session/<id>",revision:INTEGER}`，revision 为正整数 |

`branch` 与 `at` 互斥。`mergeSession` 与 `branch` / `at` / `author` / `message` 互斥，包括显式给出的 null metadata；它可以与 `graphView`、`readOnlyNoExternalIO` 组合。`branch` / `at` / `graphView` / `mergeSession` 的显式 null 均不表示省略。

指定 `at` 时，即使 Descriptor 是当前 Branch head，写入仍报 `READ_ONLY_SNAPSHOT`。candidate 只能做允许的只读检查，未解决冲突报 `MERGE_CONFLICT`，revision 不匹配报 `MERGE_SESSION_CHANGED`。Descriptor 与 Session 共享结构见 [Versioning Reference](versioning.md)。

<a id="read-only-no-external-io"></a>

## `readOnlyNoExternalIO`

`true` 在 prepare 阶段检查完整的、已通过 semantic validation 的 AST，拒绝不允许的操作，错误 category 为 `INVALID_ARGUMENT`。`false` 或省略保留普通行为。`null`、`0`、`1`、字符串 `"true"` 都不是 Boolean。

| 操作 | `true` 时的行为 |
| --- | --- |
| 普通 MATCH、投影、聚合、只读子查询、语言组合 | 允许；仍受正常语义、Snapshot 和资源限制 |
| `at` 历史读取、Graph View 内读取、current-graph Schema/Index `SHOW` | 允许 |
| Raw Vector `SEARCH` | 允许 |
| 下表列出的只读 Procedure | 允许 |
| Graph / Schema / Index 写入与 version/ref/checkout/maintenance mutation | 拒绝 |
| `CALL ... IN TRANSACTIONS` / concurrent variant | 拒绝，即使 inner query 只读 |
| `LOAD CSV` | 拒绝，包含本地文件、HTTP、参数 URI、零行路径 |
| Managed Semantic create/query/rebuild | 拒绝，即使 Provider cache 已 warm |
| Full-text query Procedure | 拒绝，因为它可调用宿主注册的 tokenizer |

本版本允许的命名 Procedure：

```text
db.labels                        db.propertyKeys
db.relationshipTypes             lithograph.branch.list
lithograph.commit.get            lithograph.tag.list
lithograph.log                   lithograph.diff
lithograph.merge.get             lithograph.merge.list
lithograph.merge.conflicts
```

白名单以 Procedure 名称分类，不能只依据 introspection 的 `mode=READ` 推断允许。其它 Procedure 即使结果只读，也必须经过分类后才能用于该选项。

检查遍历嵌套 CALL、EXISTS/COUNT/COLLECT、WHEN/NEXT/UNION 等完整结构；未进入的分支、空 UNWIND、运行时参数以及 `EXPLAIN` / `PROFILE` 均不能豁免。例如 `EXPLAIN CREATE (:Person)` 仍被拒绝，而不是因 EXPLAIN 不执行而获准。语法本身不合法时可先返回 `PARSE_ERROR` / `SEMANTIC_ERROR`。

这是 query-local 限制，不会改变 Branch checkout 或给连接安装永久只读属性。它允许当前 SQLite database 的正常读取，以及 SQLite 的 TEMP/spill 和引擎内部派生工作；不能替代 SQL 权限控制或 OS sandbox。它不控制同一连接上的其它 SQL、`lithograph_init()` 或单独调用的 transaction lifecycle API。接收不可信 Cypher 的宿主必须自行构造 options、强制为 `true`，并限制用户对原始 SQL surface 的访问。实际流程见 [安全执行指南](../guide/safe-execution.md)。

<a id="memory-limit"></a>

## `memoryLimitBytes`（Unreleased）

用于单次 `lithograph()` / `lithograph_rows()` execution 的工作内存预算，同样适用于 active explicit transaction 内的每次 execution。宿主显式选择字节数，省略时不增加新限制；已有 `range()` 200 万元素上限、SQLite 长度限制和实际资源限制继续生效。只能传 `1..9223372036854775807` 的 JSON 整数；`null`、零、负数、小数、Boolean、字符串、越界值均返回 `INVALID_ARGUMENT`，`67108864.0` 也不是整数配置。

```json
{"readOnlyNoExternalIO":true,"memoryLimitBytes":67108864}
```

该例是 64 MiB 引擎计账预算，不是产品默认值。options 在参数解码前解析；预算计入参数解码临时对象和运行时 Value、List/Map/String/Vector/图值、row/batch、排序/聚合/去重工作缓冲、结果编码与 scalar collector。移动值转移预留、复制值新增预留，同时存活的输入/输出/编码副本同时计账；分配前检查，释放后归还。嵌套查询、分支和 concurrent batch 共享同一次 execution 的预算；独立 execution/cursor 的预算彼此独立，成功、错误、取消和关闭后回收。

达到预算时，适用的 semantic barrier 提前向既有 SQLite TEMP spill；单值或必要工作状态放不下则报 `RESOURCE_ERROR` + `SQLITE_TOOBIG` (18)，不截断结果，也不改成 `SQLITE_NOMEM`。scalar 需要保留完整 envelope，因此可能超预算，而同查询的 rows 完整增量消费可以成功；streaming 已消费并释放的行不累计占用预算。宿主仍应检查真实错误与最终 summary/`SQLITE_DONE`，失败清理见 [Transactions](transactions.md#execution-memory-budget)。

它不是 RSS 或整个进程的 allocator 硬上限：不覆盖宿主已有输入缓冲、Frontend AST/plan、SQLite 页缓存、Provider 内部内存及 allocator 隐含开销。分配计账按容器容量和保守开销计算；需要主服务硬隔离的部署仍由宿主安排进程/OS 资源边界。不能用返回行数、只读选项或 SQLite `hard_heap_limit` 替代这项运行时预算。组合用法见 [应用集成](../guide/integration.md#execution-memory-budget)。

<a id="access-controls"></a>

## `readAccess` / `writeAccess`（v0.3.2）

本节适用于 v0.3.2，**不适用于 v0.3.1 发行库**；v0.3.1 会把这两个 key 报为 `INVALID_ARGUMENT`。上层宿主按业务选择限制，只填写需要禁止的规则。省略 object、`{}`、省略数组或 `[]` 都不额外收紧能力；每个 Boolean 默认不限制，显式 `false` 才禁止。

```json
{
  "readAccess": {
    "query": {"denyProperties": ["私密备注"]},
    "result": {"denyPropertyPrefixes": ["@loreweft:"]},
    "denyFunctions": ["elementId", "id"],
    "allowExternalIO": false
  },
  "writeAccess": {
    "denyProperties": ["创建时间", "私密备注"],
    "denyPropertyPrefixes": ["@loreweft:"],
    "allowDelete": false
  }
}
```

### 属性选择规则

`readAccess.query`、`readAccess.result`、`writeAccess` 都支持以下三个 STRING array，三项按并集生效：

| Key | 匹配对象 | 例子 |
| --- | --- | --- |
| `denyProperties` | 完整精确属性名 | `["私密备注", "创建时间"]` |
| `denyPropertyPrefixes` | 字面前缀 | `["@loreweft:", "private_"]` |
| `denyPropertyPatterns` | 使用 regex 语法完整匹配属性名 | `["audit_[0-9]+"]` 匹配 `audit_1`，不匹配 `my_audit_1` 或 `audit_1_extra` |

大小写敏感，不自动规范化，不按 Label 分组。只匹配图 Node/Relationship 的真实属性；普通 Map key、字符串、alias 不匹配。每项必须是非空 STRING；未知 key、错误 type（含 null）、空字符串项、非法 regex 返回 `INVALID_ARGUMENT`。regex 在一次 execution 内编译一次；空数组表示没有该类限制。

| 阶段 | 命中属性时的行为 | 应用选择 |
| --- | --- | --- |
| `query` | 按不存在处理：读取为 null，keys/properties/图对象不包含；条件、计算和聚合使用受限值 | 字段不能被 Cypher 使用 |
| `result` | 只过滤最终 Node/Relationship/Path 属性，递归处理 List/Map 内的图对象 | 字段允许定位/计算，但返回图对象时不展示 |
| `writeAccess` | 新增/修改/删除均拒绝，包括 null、动态 key、Map 替换隐含删除和相同值显式写回 | 字段必须由可信路径维护 |

只有 result 规则时，显式 `RETURN n.secret AS value`、`properties(n)`、`n{.*}` 仍返回真实值；这些是调用方主动提取 scalar/普通 Map。要连显式读取也禁止，配置 query。result 不删除 structural elementId、Label、Type、端点或 summary.commit，也不按 key 清理 Schema/Commit Data JSON。query 与 result 同时命中时以 query 的不可见值为基础。

### 读取能力

| `readAccess` 成员 | 类型/默认 | 效果 |
| --- | --- | --- |
| `query` / `result` | 属性规则 object，默认 `{}` | 上述两个读取阶段 |
| `denyFunctions` | STRING 数组（项非空），默认 `[]` | 精确禁止规范函数名调用；按 Cypher 的 ASCII 大小写规则规范化比较；未知注册函数名返回 INVALID_ARGUMENT，不解释前缀/regex |
| `allowNamedProcedures` | BOOLEAN，默认不限制 | false 禁止命名 CALL；内联 CALL 子查询不属于此开关 |
| `allowShow` / `allowExplain` / `allowProfile` | BOOLEAN，默认不限制 | false 分别禁止 SHOW / EXPLAIN / PROFILE |
| `allowExternalIO` | BOOLEAN，默认不限制 | false 在文件/HTTP、Provider（含 cache hit）、宿主 tokenizer 调用前拒绝 |

完整 AST 包括嵌套、未进入 branch 和字符串插值均检查。函数禁令只禁止调用；例如禁用 `elementId()` 不会自动从 Node JSON 删除 structural elementId。命名 Procedure 同时按真实 graph/Schema/version/maintenance/external-I/O 效果受其它规则限制，不能只依据 `SHOW PROCEDURES.mode` 判断安全。依赖 query 隐藏属性的 Search、或必须完整返回隐藏属性的 Diff/Patch/conflict read，报 `ACCESS_DENIED`，不返回删减后的伪完整版本对象。

### 写入能力

`writeAccess` 在上述三个属性数组之外只接受以下 BOOLEAN：

| Key | false 的效果 |
| --- | --- |
| `allowCreate` | 禁止创建 Node/Relationship |
| `allowDelete` | 禁止删除 Node/Relationship，含 DETACH 的连带删除 |
| `allowLabelChanges` | 禁止添加/移除 Label，包含 CREATE 的初始 Label |
| `allowSchemaChanges` | 禁止 Graph Type / Constraint / Index 逻辑定义变化 |
| `allowVersionChanges` | 禁止 Branch/Tag/Commit Data、显式 Commit、Merge Session 与版本/ref 操作 |
| `allowMaintenance` | 禁止 GC / Index rebuild 等维护 |

创建、删除、Label 开关按实际图效果检查；MERGE 只匹配既有对象而未创建时允许，零行没有实际创建时也不触发创建禁令。读取能力/函数禁令、Schema command 和已知 Procedure 的固定效果则在完整 AST 准备阶段检查。

显式 `LOAD CSV` 与已知会外联的命名 Procedure 按完整 AST 检查，零行或未进入的 branch 仍拒绝。普通 CRUD 或 Reset 则不会仅因当前 Schema 含 Full-text/Semantic Index 而被 external-I/O 禁令拒绝；只有真正新增/变更相关定义需要外部验证时，才在调用前拒绝。没有定义变化的 IF NOT EXISTS、DROP 不因这条 I/O 禁令失败。Schema 权限等其它限制仍独立生效。

这些开关全部可省略，默认不限制。允许对象 DELETE 时，删除对象自带的属性/Label 清理不再当作独立属性/Label 修改；对象删除仍受 allowDelete、Graph View、完整性和版本规则限制。

Patch、Reset、Revert、Merge、Rebase、Squash 按操作前后目标 Branch 的最终有效图/Schema 变化检查。Squash/Rebase 最终受保护字段不变时允许，不能只因内部 Layer replay 曾写过该字段而拒绝；真正改变保护字段则拒绝。Merge 显式 candidate 修改受保护，finalize 使用本次调用的规则重验，Session 创建时的允许配置不会豁免后续禁令。

<a id="canonical-version-state"></a>

### 版本中的“字段没有改变”

版本操作比较属性的持久类型与 canonical 内容，不用 Cypher 的数值 `=` 判断。Float 的 `+0.0` 与 `-0.0` 是不同的持久状态，因此真正切换正负零仍算保护字段变化；NaN 的不同 payload 统一为一个 canonical NaN，同一 canonical NaN 不应使无关 Revert/Patch 错误报告 before-condition 冲突。List/Point 等包含浮点的持久值同样按各自 canonical 内容判断。这不改变查询里的普通数值比较，也不改变现有 Commit identity 或存储格式。

### 权限组合与失败

所有限制取交集。`true` 不能解除另一个 false、属性禁令、`readOnlyNoExternalIO=true`、只读 Snapshot、Graph View 或现有 transaction boundary。普通查询属性不可见只得到 null/缺少 key；明确能力或写入拒绝为 `ACCESS_DENIED` + SQLite `SQLITE_AUTH` (23)。配置错误仍是 `INVALID_ARGUMENT`。旧 readOnlyNoExternalIO 的 category 保持不变。

规则是每次 execution 的配置，不保存在 connection，也不进入 Commit identity。active transaction 每次使用本次初始 staged state 作校验基线；子查询及独立/并发 batch 继承当前 execution 策略。失败沿用 [事务失败范围](transactions.md#access-controls)：不反向撤销已 durable 的 earlier batch。

这些 options 不限制原始 SQLite SQL、初始化、文件访问权限或独立 tx lifecycle；强制策略必须由可信宿主构造。完整调用示例见 [访问控制指南](../guide/access-controls.md)。

<a id="graph-view"></a>

## `graphView`

两个数组可以各自省略，`{}` 等价完整图。字段只能为 `requireAllLabels`、`excludeAnyLabels`，值只能为 STRING 数组；未知字段、null、非字符串元素返回 `INVALID_ARGUMENT`。

- `requireAllLabels`：Node 必须同时拥有全部 Label。
- `excludeAnyLabels`：命中任意 Label 即不可见。
- 每个数组按精确名称的集合语义去重，不做 case folding 或 Unicode normalization；同一 Label 同时 required/excluded 为非法 selector。
- 不存在的 required Label 使当前 Node 均不可见；不存在的 excluded Label 无效果。解析 selector 不创建 Label。
- Relationship 仅在两端 Node 均可见时可见；没有独立 Relationship selector。
- 后续 clause 按前面 staged writes 后的 graph state 重新计算 visibility。写入越界报 `GRAPH_VIEW_VIOLATION`。

非空 selector 限定 graph-data 查询、修改和 Search；Schema/Constraint/Index definition 和 Version Procedure 不变成 view-local 操作，与非空 Graph View 组合返回 `INVALID_ARGUMENT`。Graph View 不持久化、不命名、不进入 Commit hash，也不是账号权限。完整用法见 [Graph View 指南](../guide/graph-views.md)。

<a id="version-procedure-options"></a>

## Version procedure 的目标与 metadata

| Procedure 类别 | `branch` / author / message |
| --- | --- |
| `commit.create`、`patch.apply`、`squash`、`reset`、`revert` | 目标可由 query branch 指定；创建 Commit 时使用新 metadata |
| `merge.start` | 接受 query branch 确定 target；不接受 author/message，不创建 Commit |
| `merge.finalize` | Session 已固定 target，不接受 branch；接受 author/message，只有实际新建 Commit 才保存 |
| `merge.get/list/conflicts/resolve/abort` | Session 自带目标；不接受 branch/author/message |
| `branch.*`、`tag.*`、`commit.data.*` | 参数或 connection 已指定目标，不接受 query branch |
| `rebase` | 接受 query branch；重写保留各旧 Commit 的 author/message，不能用 execution metadata 覆盖 |
| `index.rebuild` | 只使用 name/version 参数；拒绝 branch/at/author/message/graphView 等不适用上下文 |

Version mutation 与 `at` 组合报 `READ_ONLY_SNAPSHOT`。不要把同一份 options 无差别传给所有 Procedure；精确接口见 [Procedure Reference](procedures.md)。

<a id="transaction-begin-options"></a>

## `lithograph_tx_begin()` options

这是单独的 transaction-level object，仅允许：

| Key | 类型与默认 | 说明 |
| --- | --- | --- |
| `branch` | 非空 STRING；默认 active Branch | 事务开始后目标固定 |
| `expectedHead` | `commit/<id>` STRING；默认不检查 | 取得 writer 后比较目标 head；不一致返回 `BRANCH_HEAD_MOVED`，事务不开始 |
| `author` | STRING 或 null；默认 null | 最终唯一 Commit metadata |
| `message` | STRING 或 null；默认 null | 最终唯一 Commit metadata |

begin 不接受 `at`、`graphView`、`mergeSession`、`readOnlyNoExternalIO`、`readAccess`、`writeAccess`、`memoryLimitBytes`；要限制每次 query，在 execution options 中传入。完整生命周期见 [Transactions Reference](transactions.md)。

<a id="active-transaction-options"></a>

## Active explicit transaction 中的 execution options

begin 后继续调用普通 `lithograph()` / `lithograph_rows()`。v0.3.1 此时仅接受 `graphView`、`readOnlyNoExternalIO`，默认分别为完整图、`false`；v0.3.2 还接受 `readAccess`、`writeAccess`，默认无附加限制。它们作用于当前 staged state，能看到同一事务内前面成功的 execution。

Unreleased 的 `memoryLimitBytes` 也按 execution 指定，不成为整个 transaction 的累计额度；一次成功不为后续 execution 继承预算。v0.3.2 不接受该 key。

`branch` / `at` / `expectedHead` / `author` / `message` / `mergeSession` 和其它 key 均返回 `INVALID_ARGUMENT`；作为 active execution failure，它们也会自动 abort 整个 Lithograph explicit transaction。安全拒绝同样遵循这条 fail-closed 规则。v0.3.1 没有独立 `lithograph_tx_execute()`。

超时、SQL busy handler、线程策略和取消属于宿主 SQLite API；不存在隐含的 `timeout`、`readOnly`、`database` 或任意 Neo4j driver 配置。
