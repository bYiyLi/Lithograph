# 控制查询、返回和写入的字段

适用版本：**v0.3.3**。v0.3.1 发行库不接受 `readAccess` / `writeAccess`；固定版本手册仍见 [v0.3.1](https://github.com/bYiyLi/Lithograph/blob/v0.3.1/docs/guide/README.md)。本页演示宿主如何让调用方用内部键定位人物、返回人物时隐藏内部键，并禁止修改内部维护字段。

调用仍是 `lithograph(query, params, options)` 或 `lithograph_rows(query, params, options)`，只增加第三个参数里的规则。由可信宿主构造 options；允许用户自行删掉规则或直接执行其它 SQLite SQL，就不能强制这些限制。完整字段表见 [Execution Options](../reference/execution-options.md#access-controls)。

<a id="choose-stage"></a>

## 1. 先选择限制发生的位置

| 当前诉求 | 使用配置 | 调用方观察到的结果 |
| --- | --- | --- |
| 字段不能被查询条件、计算或返回使用 | `readAccess.query` | 像不存在一样，读取为 null、枚举没有该字段 |
| 字段能用于定位/计算，但返回图对象时不展示 | `readAccess.result` | Node/Relationship/Path 不带字段；显式标量/普通 Map 提取仍允许 |
| 字段不允许被改动 | `writeAccess` | 新增、更新、移除均报 ACCESS_DENIED，包括写回相同值 |

三个阶段可以同时使用。省略任何阶段或传 `{}` 不增加该阶段的限制；未填写的 Boolean 不改变现有能力。下面演示的是教学数据，先由可信步骤初始化：

```sql
SELECT lithograph_init();
SELECT lithograph(
  'CREATE (:人物 {编号:''p001'', 姓名:''张三'', 年龄:20,
    `@loreweft:key`:''person-p001'', 私密备注:''内部备注'', 创建时间:''2026-10-18''}) FINISH'
);
```

<a id="query-mask"></a>

## 2. 让查询看不到某个字段

```sql
SELECT lithograph(
  'MATCH (p:人物) RETURN p.姓名 AS 姓名,p.私密备注 AS 备注,p[$key] AS 动态备注',
  '{"key":"私密备注"}',
  '{"readAccess":{"query":{"denyProperties":["私密备注"]}}}'
);
```

下面这个独立示例用同一次 execution 创建教学数据再受限读取；options 为 `{"readAccess":{"query":{"denyProperties":["私密备注"]}}}`：

<!-- example: access-query-masked -->
```cypher
CREATE (p:字段示例 {姓名:'示例',私密备注:'内部'})
RETURN p.姓名 AS 姓名,p.私密备注 AS 备注,properties(p) AS 属性
```

返回 `[["示例",null,{"姓名":"示例"}]]`。query 规则不自动禁止可信目标的属性写入；它只决定查询能看到什么，需要保护写入时另加 writeAccess。

结果为 `[["张三",null,null]]`。`WHERE p.私密备注='内部备注'` 也只能使用 null，不能利用真实值命中人物；`keys(p)`、`properties(p)`、`p{.*}` 不会含该属性。普通 Map 的同名 key 保持原值。

<a id="result-mask"></a>

## 3. 用内部键定位，返回人物时隐藏内部键

```sql
SELECT lithograph(
  'MATCH (p:人物) WHERE p.`@loreweft:key`=$key RETURN p AS 人物',
  '{"key":"person-p001"}',
  '{"readAccess":{"result":{"denyPropertyPrefixes":["@loreweft:"]}}}'
);
SELECT ordinal,event,data FROM lithograph_rows(
  'MATCH (p:人物) WHERE p.`@loreweft:key`=$key RETURN {人物:p,列表:[p]} AS 包装',
  '{"key":"person-p001"}',
  '{"readAccess":{"result":{"denyPropertyPrefixes":["@loreweft:"]}}}'
);
```

两种调用都能定位同一个人物；公开图对象的 `properties` 没有 `@loreweft:key`。包装 Map/List 内的图对象同样过滤，structural `elementId`、Label 和 summary.commit 保留。

result 规则只负责图对象展示。下面是刻意允许的显式取值：

```sql
SELECT lithograph(
  'MATCH (p:人物) RETURN p.`@loreweft:key` AS 内部键,properties(p) AS 属性,p{.*} AS 投影',
  '{}',
  '{"readAccess":{"result":{"denyPropertyPrefixes":["@loreweft:"]}}}'
);
```

标量 `内部键` 为 `person-p001`，两个普通 Map 仍包含该键。若业务连这种主动提取也不能接受，把同一规则同时加到 `readAccess.query`。仅按返回 alias 或普通 Map key 过滤会误伤业务字段，也不能实现这里的两种语义。

<a id="controlled-write"></a>

## 4. 允许改业务字段，保护内部维护字段

```sql
SELECT lithograph(
  'MATCH (p:人物) WHERE p.`@loreweft:key`=$key SET p.姓名=$name RETURN p AS 人物',
  '{"key":"person-p001","name":"李四"}',
  '{"readAccess":{"query":{"denyProperties":["私密备注"]},
      "result":{"denyPropertyPrefixes":["@loreweft:"]},"allowExternalIO":false},
    "writeAccess":{"denyProperties":["创建时间","私密备注"],
      "denyPropertyPrefixes":["@loreweft:"],"allowDelete":false}}'
);
```

这次更新成功并形成正常 Commit，返回人物显示 `李四`，不含内部键和私密备注。内部键仍可定位；私密备注既不能取真实值，也不能修改；创建时间可以读取但不能修改。要再禁止创建、Label、Schema、版本或维护，只填写对应 false 开关；不必把所有未使用能力显式写成 true。

下面是可独立验证的拒绝例子。它先尝试创建人物再写保护字段，整次执行失败后连新人物也回滚。执行时的 options 为 `{"writeAccess":{"denyPropertyPrefixes":["@loreweft:"]}}`：

<!-- example: access-write-protected -->
```cypher
CREATE (p:人物 {姓名:'张三'}) SET p.`@loreweft:key`='other' RETURN p.姓名 AS 姓名
```

结果为 `ACCESS_DENIED`，SQLite code 为 `SQLITE_AUTH` (23)，人物和 Branch head 均保持不变。`SET p[$key]=null`、`REMOVE`、Map 合并/替换、显式写回原值都适用同一规则。允许 DELETE 时，删除对象自带的属性清理不被当作另一次属性写入；是否可以删除仍由 allowDelete 等规则决定。

<a id="patterns-and-capabilities"></a>

## 5. 使用名称规则和操作开关

| 规则 | 例子 | 匹配 |
| --- | --- | --- |
| 精确名 | `denyProperties:["创建时间"]` | 只匹配该属性名 |
| 字面前缀 | `denyPropertyPrefixes:["@loreweft:"]` | 匹配所有以该文本开头的属性 |
| 完整名称正则 | `denyPropertyPatterns:["audit_[0-9]+"]` | 匹配 audit_1，不匹配 my_audit_1 或 audit_1_extra |

规则区分属性名大小写，三项取并集；不会匹配普通 Map key、alias 或字符串内容。空数组没有限制，空字符串项和非法 regex 报 INVALID_ARGUMENT。`denyFunctions` 使用注册的规范函数名精确匹配，名称按 Cypher 的 ASCII 大小写规则比较；未知函数名报 INVALID_ARGUMENT，避免拼写错误使规则失效。

以下调用禁止文件/HTTP、Provider 与宿主 tokenizer，并禁止标识符函数；普通图读取仍可用：

```sql
SELECT lithograph(
  'MATCH (p:人物) RETURN p.姓名 AS 姓名',
  '{}',
  '{"readAccess":{"denyFunctions":["elementId","id"],"allowExternalIO":false}}'
);
```

它不自动禁止所有写入；要整体只读并禁止外部 I/O，使用已有 [readOnlyNoExternalIO](safe-execution.md)。能力限制始终取交集，true 不能覆盖其它禁令。命名 Procedure 受 allowNamedProcedures 与各自实际效果共同限制；内联 CALL 子查询可按其实际内容继续执行。

<a id="transactions-and-versions"></a>

## 6. 每次事务执行带自己的规则

```sql
SELECT lithograph_tx_begin('{"message":"trusted staged work"}');
SELECT lithograph('CREATE (:待处理 {内容:''新数据'',私密备注:''本次暂存''}) FINISH');
SELECT lithograph(
  'MATCH (p:待处理) RETURN p.内容 AS 内容,p.私密备注 AS 备注',
  '{}', '{"readAccess":{"query":{"denyProperties":["私密备注"]}}}'
);
SELECT lithograph_tx_abort();
SELECT lithograph('MATCH (p:待处理) RETURN count(p) AS 剩余');
```

受限读取看到 `[["新数据",null]]`，abort 后剩余为 `0`。规则属于每次 execution，begin 不接受 readAccess/writeAccess；本次写入以 execution 开始时已有 staged state 为校验基线。若改为对保护字段写入并被拒绝，整个 explicit transaction 自动 abort，此前 staged writes 也回滚；不能继续 commit 失败事务。

版本入口同样可用这些配置。Patch、Reset、Revert、Merge、Rebase、Squash 按操作前后目标 Branch 的有效 graph/Schema 变化检查。Squash/Rebase 最终受保护属性没变时允许；Reset/Merge 真正改变保护字段时拒绝。Merge finalize 每次用本次规则重验，不因为 Session 创建时没有规则而获准。这里的“最终没变”按属性已保存的类型与内容判断；例如正零/负零是不同持久状态，同一个规范 NaN 则不应让无关 Revert 失败，详见 [版本字段状态](../reference/execution-options.md#canonical-version-state)。

[access_controls.py](examples/access_controls.py) 提供完整调用及断言：scalar/rows 过滤、允许业务写入、拒绝后属性/Branch head 不变、explicit transaction 自动 abort、Squash 最终状态不变成功、Reset 改保护字段失败，以及正负零 Squash 和保留 NaN 的无关 Revert。运行方式：

```sh
python3 -B docs/guide/examples/access_controls.py /absolute/path/lithograph.dylib
```

Cypher 独立/并发 batch 继承同一次 execution 策略，earlier durable batch 保留；后续拒绝/取消不代表整次导入没有副作用。完整规则见 [Transactions Reference](../reference/transactions.md#access-controls)。
