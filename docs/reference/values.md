# Value 与 Lithograph JSON v1

**版本：v0.3.0。** SQL scalar envelope 与 `lithograph_rows()` event payload 使用同一 JSON value encoding；params 的值也接受同一 tagged encoding。它是 SQLite/JSON 边界编码，不是新的 Cypher 类型系统。

## 基本值与容器

| Cypher 值 | JSON 编码 |
| --- | --- |
| null / BOOLEAN / STRING | 普通 JSON null / boolean / string |
| INTEGER，位于 ±9,007,199,254,740,991 范围内 | 普通 JSON integer |
| 其他 signed 64-bit INTEGER | `{"$type":"Integer","value":"9223372036854775807"}` |
| 有限 FLOAT | 普通 JSON number |
| NaN / Infinity / -Infinity | `{"$type":"Float","value":"NaN"}`，value 可为上述三种字符串 |
| LIST | JSON array，元素递归编码 |
| MAP | 普通 JSON object；包含保留 `$type` key 时用 Map wrapper |

普通业务 Map 需要保留 `$type` 时：

```json
{"$type":"Map","entries":{"$type":"application-record","name":"example"}}
```

不要把带 `$type` 的对象直接假定为用户 Map；未知 tag 报 `INVALID_ARGUMENT`。已知 tag 的字段有严格类型/形状要求。JavaScript 等宿主不要先把超大整数转成不精确的浮点数，再指望数据库恢复原值。

## 图元素

以下 ID 仅用于说明结构：

```json
{"$type":"Node","elementId":"n:1","labels":["Person"],"properties":{"name":"Alice"}}
```

```json
{"$type":"Relationship","elementId":"r:1","type":"KNOWS","start":"n:1","end":"n:2","properties":{"since":2020}}
```

```json
{"$type":"Path","nodes":[],"relationships":[]}
```

Path 的两个数组按 traversal order 编码；上面只是字段形状，不是创建一个有效路径的输入示例。**Node、Relationship、Path 属于结构化运行时结果，不能作为 Cypher params 传回**，即使外形符合 tagged encoding 也会被参数校验拒绝。要查找已知元素，使用业务键或 `elementId(n) = $id` 并传字符串；查询仍受当前 Snapshot 与 Graph View 约束。

## Temporal、Point、UUID、Vector

| Tag | 必需字段 / 示例 |
| --- | --- |
| `Date` | `value:"2026-09-16"` |
| `LocalTime` | `value:"12:30:00"` |
| `Time` | 带 offset 的 `value:"12:30:00Z"` |
| `LocalDateTime` | `value:"2026-09-16T12:30:00"` |
| `ZonedDateTime` | `value` 和精确 `zone`，例如 `value:"2026-09-16T12:30:00Z",zone:"Z"`；保留 canonical 文本 |
| `Duration` | `value`，例如 `"P1D"` |
| `Point` | `crs`、`coordinates`，例如 `"cartesian"` 与 `[1.0,2.0]` |
| `UUID` | canonical lowercase `value` |
| `Vector` | `coordinateType`、`dimension`、`values` |

Vector 参数完整示例：

```json
{"embedding":{"$type":"Vector","coordinateType":"FLOAT64","dimension":2,"values":[1.0,0.0]}}
```

也可在 Cypher 内构造 `date('2026-09-16')`、`point({x:1.0,y:2.0})`、`vector([1.0,0.0],2,FLOAT64)` 等。Vector 输出 coordinateType 使用 canonical 短名（例如 `F64`），不保留输入 `FLOAT64` 别名拼写。优先保留数据库返回的 tagged values，避免自行重建时丢失时区、坐标类型或精度。

## Runtime value 不等于可存储 Property

查询可以计算 Map、Node、Relationship、Path 等值，但不代表它们可以原样保存为 Property。Graph element / Path / Map 不能作为持久 Property；List 也必须满足 Cypher Property value 的合法性约束。需要连接两个对象时使用 Relationship，不把 Node object 塞入 Property。

Property 赋值 null 表示移除该 Property，不是保存一个存在但 null 的 Property。参数可以为 null，读取不存在 Property 也返回 null，应用要区分结果语义与存储存在性。

Primitive float 的标准 JSON 不接受 NaN / Infinity；示例使用 `json.dumps(...,allow_nan=False)`，需要这些值时显式使用 Float tag。单行和总 result envelope 还受宿主 SQLite 长度/资源限制。

依据：[JSON encoder/decoder](../../crates/lithograph-core/src/cypher/json.rs)、[types](../../crates/lithograph-core/src/cypher/types.rs)、[Lithograph JSON](../design/interfaces.md#lithograph-json)。
