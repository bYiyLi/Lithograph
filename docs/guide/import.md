# 导入参数数据与 CSV

[产品手册](README.md) · [Cypher 语言手册](../cypher/index.md) · [接口参考](../reference/README.md)

本文适用 **v0.3.3**（`CY25-2026.08`，storage format `3`）。目标：选择适合输入规模和失败恢复要求的导入方式，并用业务查询核对最终结果。SQL 示例在独立空库、已加载扩展的 connection 中依次执行；文件例子使用单独的临时库。

<a id="atomicity"></a>

## 1. 选择原子性边界

| 需求 | 做法 | 失败后需要检查什么 |
| --- | --- | --- |
| 输入已在应用内，作为一次逻辑变化导入 | 参数 List + UNWIND | 本次普通 query 整体回滚 |
| 多次独立调用形成一个版本 | Lithograph explicit transaction | 任一步失败自动 abort 全部 staged writes |
| 从文件/HTTP CSV 一次导入 | LOAD CSV | 普通 query 整体回滚；文件读取不可撤销 |
| 大输入按 batch 保留已完成进度 | CALL … IN TRANSACTIONS | earlier durable batch 保留，必须按业务键确认进度 |

Branch、Tag 和完整备份用于演练与恢复；导入不要求另建数据库文件模拟 Branch。需要并发安全的业务主键时先定义 KEY/UNIQUE，`MERGE` 本身不替代约束。

<a id="parameter-import"></a>

## 2. 导入已解析的参数数据

```sql
SELECT lithograph_init();
SELECT lithograph(
  'CREATE CONSTRAINT imported_person_id FOR (p:ImportedPerson) REQUIRE p.id IS NODE KEY'
);
SELECT lithograph(
  'UNWIND $people AS item
   MERGE (p:ImportedPerson {id:item.id})
   SET p.name = item.name, p.age = item.age
   RETURN p.id AS id, p.name AS name ORDER BY id',
  '{"people":[{"id":"alice","name":"Alice","age":30},{"id":"bob","name":"Bob","age":40}]}',
  '{"message":"Import people"}'
);
SELECT lithograph(
  'MATCH (p:ImportedPerson) RETURN p.id AS id,p.age AS age ORDER BY id'
);
```

导入返回 `[["alice","Alice"],["bob","Bob"]]`，核对查询返回 `[["alice",30],["bob",40]]`。整条 UNWIND 写查询形成一个 Commit。真正应用使用 SQLite 参数绑定传递 query、params、options，不拼接业务文本。

对于巨大 List，先在宿主侧按可控批次构造参数，或者使用下面的 CSV/batch 路径；把大量数据装进一个 JSON 参数本身仍会占用应用和解析内存。若多次参数调用最终必须一个 Commit，按 [事务指南](transactions.md) 用 explicit transaction 包裹，并在任何错误后停止后续调用。

<a id="csv-import"></a>

## 3. 从 UTF-8 CSV 导入

先准备教学数据；CSV 数字仍是 String：

```csv
id,name,age
alice,Alice,30
bob,Bob,40
```

下面的 Python 使用应用已经加载扩展并初始化的独立 `db`。临时文件只供演示，不读取业务文件；helper 来自 [python_quickstart.py](examples/python_quickstart.py)：

```python
from pathlib import Path
import tempfile
from python_quickstart import query

with tempfile.TemporaryDirectory(prefix="lithograph-import-") as directory:
    source = Path(directory) / "people.csv"
    source.write_text("id,name,age\nalice,Alice,30\nbob,Bob,40\n", encoding="utf-8")
    result = query(db,
        "LOAD CSV WITH HEADERS FROM $source AS row "
        "MERGE (p:CsvPerson {id:row.id}) "
        "SET p.name=row.name, p.age=toInteger(row.age) "
        "RETURN count(p) AS imported",
        {"source": source.as_uri()})
    assert result["rows"] == [[2]]
assert query(db, "MATCH (p:CsvPerson) RETURN p.id,p.age ORDER BY p.id")["rows"] == [
    ["alice", 30], ["bob", 40]
]
```

本地 source 用完整 `file:///...` URI；也支持 `http://`、`https://`。用 `Path.as_uri()` 处理空格与转义，不把普通 OS 路径直接当 URI。没有 WITH HEADERS 时每行是字段 List；有时是按表头名称访问的 Map。字段分隔、引号、空字段、URI 和解析错误详见 [LOAD CSV](../cypher/load-csv.md)。

`toInteger()` 等转换不能替代输入校验和约束。生产导入应在写入前明确哪些空字段允许、哪些无效值必须拒绝，并通过约束或显式预检查验证；不要悄悄把转换失败后的 null 当有效数据。

<a id="batch-import"></a>

## 4. 让较大导入按 batch 提交

先用不含文件的输入验证 batch 行为：

```sql
SELECT lithograph(
  'UNWIND $records AS item
   CALL (item) {
     MERGE (p:BatchPerson {id:item.id}) SET p.name=item.name
   } IN TRANSACTIONS OF 2 ROWS
   RETURN item.id AS id ORDER BY id',
  '{"records":[{"id":"a","name":"A"},{"id":"b","name":"B"},{"id":"c","name":"C"}]}'
);
SELECT lithograph('MATCH (p:BatchPerson) RETURN count(p) AS imported');
```

返回 `a,b,c` 三行，核对 count 为 `3`；两个 mutating batch 各自形成 Commit。把前缀替换为 `LOAD CSV WITH HEADERS FROM $source AS item` 即可用于 CSV。

必须在 SQLite autocommit mode、没有 active Lithograph explicit transaction 时执行。Python 使用 `isolation_level=None`；不要把 batch query 放入 `BEGIN`。两个 SQL result surface 都支持此形式；stream 提前关闭只撤销尚未 durable 的 current batch，保留 earlier batch。

需要逐项观察失败时可使用 `ON ERROR CONTINUE REPORT STATUS AS status`；完整状态字段、retry/break/fail、并发和 DISJOINT BY 见 [Transaction Subqueries](../cypher/transaction-subqueries.md)。并发 batch 同样受单文件 writer、数据依赖和声明规则约束，先用顺序 batch 验证正确性。

<a id="verify-and-recover"></a>

## 5. 验证失败、恢复和权限

普通 LOAD CSV 遇到 malformed CSV、类型、约束或 I/O error，会回滚本次图写入。batch 模式的后续错误可能保留前面两条已经提交的记录；恢复时先用业务键与进度状态检查，随后只重放适合重试的数据，不把“没有 summary”当作“什么都没导入”。[streaming.py](examples/streaming.py) 用 malformed CSV 实际验证普通 rollback 与 earlier batch 保留。

核对数据总量、业务唯一键、字段类型、引用关系和关键查询结果，再运行完整性检查；count 相等不能证明记录内容全部正确。较大维护窗口中完成备份与恢复验证，见 [运维](operations.md)。

LOAD CSV 继承宿主进程文件/网络权限；宿主需要控制来源和资源，不让任意请求选择内部 URL 或私有路径。`readOnlyNoExternalIO:true` 会在读取文件/网络前拒绝整个 LOAD CSV query，参数 URI 和空输入同样不豁免。开放查询入口见 [安全执行](safe-execution.md)。
