# Spatial Point 函数

适用版本：**v0.3.1** · Cypher Profile：`CY25-2026.08`。

[Cypher 手册](../index.md) · [函数索引](../../reference/functions.md) · [值类型](../types.md) · [宿主 JSON 编码](../../reference/values.md)

Point 是带 CRS 和维度的 Cypher 值，可以作为属性。Cartesian 与地理坐标具有不同单位；传给空间函数之前应确认 CRS。空间索引的建立和使用见[索引](../indexes.md)。

<a id="fn-point"></a>

## `point()`

<!-- function-signatures: point -->
```text
point(input :: MAP) :: POINT
```
<!-- /function-signatures -->

使用坐标 Map 构造 `POINT`。至少提供 `x/y` 或 `longitude/latitude`，可以增加 `z` 或 `height` 表示第三维。坐标为 Integer/Float，存为浮点坐标。可通过 `crs` String 或 `srid` Integer 指定坐标系；二者不能同时提供。

| CRS | SRID | 坐标与单位 |
| --- | --- | --- |
| `cartesian` | 7203 | 二维 x/y，单位由应用定义 |
| `cartesian-3d` | 9157 | 三维 x/y/z，同一单位 |
| `wgs-84` | 4326 | 经度/纬度，度数 |
| `wgs-84-3d` | 4979 | 经度/纬度及以米计的 height |

未指定 CRS 时，x/y 推断 Cartesian，longitude/latitude 推断 WGS-84；第三坐标决定 3D。显式 WGS-84 CRS 也可配合 x/y，但经纬度命名不能配合 Cartesian。纬度必须在 −90 到 90 度之间，经度超出 −180 到 180 度会环绕归一；坐标数必须满足相应 CRS 的维度，所有坐标必须有限。不能混用 x/y 与 longitude/latitude，也不能同时给 z 和 height。

返回 Point 可读取 `crs`、`srid`、`x/y`，3D 可读 `z`，WGS-84 可读 longitude/latitude，WGS-84-3d 可读 height；读取当前 Point 不具备的维度或地理字段产生 `TYPE_ERROR`。整个 input 或任一已识别构造字段为 null 返回 null。非 Map、非数值、冲突 CRS/坐标或维度错误产生 `TYPE_ERROR`；缺少坐标产生 `SEMANTIC_ERROR`。

<!-- example: fn-point -->
```cypher
RETURN point({longitude:121.5,latitude:31.2}).crs AS crs,
       point({x:3,y:4,z:5}).srid AS srid
```

预期 `['crs', 'srid']` 对应一行 `["wgs-84", 9157]`。

<a id="fn-point-distance"></a>

## `point.distance()`

<!-- function-signatures: point.distance -->
```text
point.distance(from :: POINT, to :: POINT) :: FLOAT
```
<!-- /function-signatures -->

计算两个 Point 的非负 Float 距离，两个参数必需。Cartesian 使用二维/三维 Euclidean 距离，单位与坐标相同。WGS-84 返回米；二维使用球面大圆距离，三维还考虑平均高度与高度差。

两个 Point 必须有相同 CRS（包含维度），否则返回 null；任一参数为 null 也返回 null。非 Point 输入产生 `TYPE_ERROR`。距离适合 `WHERE point.distance(n.location,$origin) <= $radius`，可以在满足属性类型条件时使用 Point Index。

<!-- example: fn-point-distance -->
```cypher
RETURN point.distance(point({x:0,y:0}),point({x:3,y:4})) AS distance
```

预期 `['distance']` 对应一行 `[5.0]`。

<a id="fn-point-withinbbox"></a>

## `point.withinBBox()`

<!-- function-signatures: point.withinBBox -->
```text
point.withinBBox(point :: POINT, lowerLeft :: POINT, upperRight :: POINT) :: BOOLEAN
```
<!-- /function-signatures -->

检查 point 是否处于 lowerLeft 与 upperRight 定义的闭合 bounding box 中，边界包含在内，返回 Boolean。三个 Point 必需且 CRS/维度相同；不匹配返回 null，任一参数 null 也返回 null，非 Point 输入产生 `TYPE_ERROR`。

Cartesian 各轴以及地理纬度/高度都必须落在闭区间内；lower 值大于 upper 时不匹配。WGS-84 经度是例外：lower 经度大于 upper 表示跨越反子午线，接受大于 lower 或小于 upper 的经度。不能用两端点的直线距离替代 bbox 判断。

<!-- example: fn-point-withinbbox -->
```cypher
RETURN point.withinBBox(
  point({longitude:180,latitude:5}),
  point({longitude:170,latitude:0}),
  point({longitude:-170,latitude:10})
) AS inside
```

预期 `['inside']` 对应一行 `[true]`。

## null、CRS 与失败示例

<!-- example: fn-spatial-null-crs -->
```cypher
RETURN point({x:null,y:1}) AS missing,
       point.distance(point({x:0,y:0}),point({longitude:0,latitude:0})) AS mixedCRS
```

预期 `['missing', 'mixedCRS']` 对应一行 `[null, null]`。

<!-- example: fn-spatial-missing-coordinate -->
```cypher
RETURN point({x:1}) AS value
```

预期错误类别：`SEMANTIC_ERROR`。

标准语义参考：[Neo4j Cypher 25 spatial functions](https://neo4j.com/docs/cypher-manual/25/functions/spatial/)。
