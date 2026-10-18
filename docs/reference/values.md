# Value 与 Lithograph JSON v1

适用版本：**v0.3.2** · [产品手册](../guide/README.md) · [接口参考](README.md)

SQL scalar envelope、rows event payload 和 params 共用 Lithograph JSON v1。它规定 SQLite/JSON 边界编码；Cypher 的类型、可存储 Property、null 与运算规则由 [语言类型](../cypher/types.md)、[null 与排序](../cypher/null-and-ordering.md) 说明。

<a id="basic-values"></a>

## 基本值与容器

| Cypher 值 | JSON 编码 |
| --- | --- |
| null / BOOLEAN / STRING | 普通 JSON null / boolean / string |
| INTEGER，位于 ±9,007,199,254,740,991 范围内 | 普通 JSON integer |
| 其它 signed 64-bit INTEGER | `{"$type":"Integer","value":"9223372036854775807"}` |
| 有限 FLOAT | 普通 JSON number |
| NaN / ±Infinity | `{"$type":"Float","value":"NaN"}`，value 也可为 `"Infinity"` / `"-Infinity"` |
| LIST | JSON array，元素递归编码 |
| MAP | 不含 `$type` 时为 JSON object；包含时用 Map wrapper |

params 顶层必须是 object，键是参数名（不带 `$`），值递归接受合法参数值。比如查询 `RETURN $name,$config` 对应 `{"name":"Alice","config":{"enabled":true}}`。params 本身为 null/list/string 均报 `INVALID_ARGUMENT`，参数值 null 则合法。

普通 JSON number 按数值解码：能够表示 signed 64-bit 的整数成为 INTEGER，其它可表示数字可成为 FLOAT。需要精确 INTEGER64 时必须在宿主失去精度**之前**构造 Integer tag；不要把 `9223372036854775807` 先放进 JavaScript Number 再 JSON.stringify。

<a id="map-wrapper"></a>

## 保留 `$type` 与 Map wrapper

解码器把任何带 `$type` 的 object 解释为 tagged value，tag 区分大小写，未知 tag 或非字符串 tag 报 `INVALID_ARGUMENT`。要保留业务 Map 的 `$type` 字段，使用：

```json
{"$type":"Map","entries":{"$type":"application-record","name":"example"}}
```

wrapper 的 `entries` 必须为 object，其键当作 Map key 解码，因此这里的业务 `$type` 不再次当作 tag；entries 的各个值仍递归解码。嵌套 Map 如需保留自己的 `$type`，也需要自己的 wrapper。

所有 tag 都要求**准确字段集合**：不能遗漏必需字段、增加其它字段或用 null 替代必需 STRING/array。普通 Map 没有这种固定 key 集合限制。编码返回的 object key 顺序不构成 API 合同。

<a id="graph-elements"></a>

## 图元素

以下 ID 用于展示结构：

```json
{"$type":"Node","elementId":"n:1","labels":["Person"],"properties":{"name":"Alice"}}
```

```json
{"$type":"Relationship","elementId":"r:1","type":"KNOWS","start":"n:1","end":"n:2","properties":{"since":2020}}
```

```text
{"$type":"Path","nodes":[<Node>,...],"relationships":[<Relationship>,...]}
```

Node 的 elementId/labels/properties 分别是 STRING、STRING array、Map；Relationship 的 type/start/end 是 STRING，start/end 对应端点 elementId。Path 两个数组按 traversal order，nodes 数量恰好比 relationships 多一；可以遍历逆向关系，Relationship 的 start/end 仍是存储方向。长度零的有效 Path 有一个 Node，不能把两个空数组当成有效 Path。

**Node、Relationship、Path 只能作为运行时结果，不能作为 params 传回**，无论在顶层值、List 或 Map 内都会被参数校验拒绝。需要再次查找时传业务键或 elementId STRING，并在查询中匹配；返回结构不会为其它 Snapshot 创建相同元素，也不绕过 Graph View。

<a id="temporal-point-uuid-vector"></a>

## Temporal、Point、UUID、Vector

表中 JSON 展示一个完整的参数值；实际 params 要再包参数名，例如 `{"day":{...}}`。

| Tag | 必需字段与值示例 | 边界 |
| --- | --- | --- |
| `Date` | `{"$type":"Date","value":"2026-10-15"}` | canonical date text |
| `LocalTime` | `{"$type":"LocalTime","value":"12:30:00"}` | 无 offset 的 local time |
| `Time` | `{"$type":"Time","value":"12:30:00Z"}` | time 带 offset |
| `LocalDateTime` | `{"$type":"LocalDateTime","value":"2026-10-15T12:30:00"}` | 无 zone 的 datetime |
| `ZonedDateTime` | `{"$type":"ZonedDateTime","value":"2026-10-15T12:30:00Z","zone":"Z"}` | value 和精确 zone 都保留；不要只保存 offset 后丢掉 zone |
| `Duration` | `{"$type":"Duration","value":"P1D"}` | canonical duration text，不自动换算成秒 |
| `Point` | `{"$type":"Point","crs":"cartesian","coordinates":[1.0,2.0]}` | CRS 决定坐标顺序和维度 |
| `UUID` | `{"$type":"UUID","value":"123e4567-e89b-42d3-a456-426614174000"}` | 输出 lowercase canonical text |
| `Vector` | `{"$type":"Vector","coordinateType":"F64","dimension":2,"values":[1.0,0.0]}` | 1–4096 维，与 values 长度一致 |

Temporal tag 不能用普通字符串代替后期待比较保持类型；例如 `"2026-10-15"` 仍是 STRING。文本解析、时区/DST 与算术见 [Temporal 函数](../cypher/functions/temporal.md)。

Point 的 `crs` 支持 `cartesian` / `cartesian-3d` / `wgs-84` / `wgs-84-3d`，分别要求 2/3/2/3 个 finite number。地理坐标顺序为 longitude、latitude、可选 height；latitude 必须在 −90..90，超出范围报错；longitude 按地理坐标规则归一化。不要将 `[latitude,longitude]` 误当合法业务位置。

Vector 输入 coordinateType 接受下列名称，输出总是 canonical 短名：

| 输出短名 | 接受的输入别名 |
| --- | --- |
| `I8` | `INTEGER8` |
| `I16` | `INTEGER16` |
| `I32` | `INTEGER32` |
| `I64` | `INTEGER`、`INTEGER64` |
| `F32` | `FLOAT32` |
| `F64` | `FLOAT`、`FLOAT64` |

整数坐标必须是对应类型范围内的 INTEGER，I64 坐标可用 Integer tag 防止宿主精度丢失；FLOAT 坐标接受数值，但转换后必须 finite，F32 超出 finite range 报错。输出 F32 数值可能显示转换后的 binary32 精度；不要要求 JSON 文本与输入逐字相同。Vector 不接受 NaN/Infinity 坐标，即使 Float tag 本身可表示这些标量。

<a id="parameter-round-trip"></a>

## 绑定和结果解码

以下 SQL 中 params 由绑定传入时使用相同结构，这里只展示确定的静态值：

```sql
SELECT lithograph_init();
SELECT lithograph(
  'RETURN $big AS big,$day AS day,$embedding AS embedding,$map AS map',
  '{"big":{"$type":"Integer","value":"9223372036854775807"},"day":{"$type":"Date","value":"2026-10-15"},"embedding":{"$type":"Vector","coordinateType":"FLOAT64","dimension":2,"values":[1.0,0.0]},"map":{"$type":"Map","entries":{"$type":"application-record"}}}'
);
```

columns 为 `big,day,embedding,map`；row 保留 Integer、Date、Map wrapper，Vector coordinateType 规范化为 `F64`。宿主先 JSON.parse，再按 tag 解码；不要把 envelope 整个当作 Cypher Map，也不要把日期 tag 或 graph object 仅通过 `str()` 显示后丢失字段。

JSON 标准不允许裸 NaN/Infinity。Python 示例使用 `json.dumps(...,allow_nan=False)`；这些值显式用 Float tag。SQL TEXT 与字符串内的 UTF-8 必须由 SQLite driver 正常编码，不手工拼 query literal。

<a id="runtime-value-versus-property"></a>

## Runtime value 不等于可存储 Property

参数可合法用于表达式计算，并不保证可保存成 Property。Property 类型规则、null 赋值与 List 元素约束见 [Cypher Types](../cypher/types.md)。参数结构错误发生在解码阶段，通常为 `INVALID_ARGUMENT`；已成功解码的值用于不兼容运算或属性赋值，可得到 `TYPE_ERROR` / `CONSTRAINT_ERROR`。通过 JSON decoder 不等于通过整次 query。

单个值、单行 data 与 scalar 总 envelope 仍受 SQLite `SQLITE_LIMIT_LENGTH` 和实际资源限制；streaming 减少总结果物化，并不免除单行编码限制。详见 [Limits](limits.md) 与 [Errors](errors.md)。
