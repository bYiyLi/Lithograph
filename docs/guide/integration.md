# 应用集成

v0.3.0 的 **application-facing execution 统一走 SQLite SQL**。应用不需要也不应解析 Lithograph shared library 的 query symbols；只需要使用自己的 SQLite driver/`sqlite3*` 加载 extension 并执行 SQL。

| 应用需要 | 使用入口 |
| --- | --- |
| 普通读写、Schema、Search、版本管理 | `lithograph()` |
| 增量结果 / 大结果 | `lithograph_rows()` |
| 多次 execution 最终一个 Commit | `lithograph_tx_begin()` + 普通 execution + `commit/abort` |
| `CALL ... IN TRANSACTIONS` | 普通 `lithograph()` / `lithograph_rows()`，SQLite autocommit mode |
| 自定义 Embedding implementation | `EmbeddingProviderV1` SPI（Provider 作者使用，不是 application query API） |

## Python

```python
import json
import sqlite3

db = sqlite3.connect("graph.db", isolation_level=None)
db.enable_load_extension(True)
db.load_extension("/absolute/path/lithograph.dylib")
json.loads(db.execute("SELECT lithograph_init()").fetchone()[0])

result = json.loads(
    db.execute(
        "SELECT lithograph(?, ?, ?)",
        (
            "MATCH (p:Person) WHERE p.name=$name RETURN p.name",
            json.dumps({"name": "Alice"}, allow_nan=False),
            "{}",
        ),
    ).fetchone()[0]
)
```

始终使用 SQLite 参数绑定，不用字符串拼接用户输入。`params` / `options` 省略时等价于 `{}`；显式 SQL `NULL` 不是“省略”。

### Streaming

```python
cursor = db.execute(
    "SELECT ordinal,event,data FROM lithograph_rows(?, ?, ?)",
    ("MATCH (p:Person) RETURN p.name ORDER BY p.name", "{}", "{}"),
)
for ordinal, event, data in cursor:
    payload = json.loads(data)
    if event == "columns":
        columns = payload
    elif event == "row":
        handle_row(payload)
    elif event == "summary":
        summary = payload
```

需要以 `summary` 作为 execution success evidence 时，不要在外层 SQL 把它过滤掉。`WHERE event='row'` 适合只消费 row payload，但会隐藏 summary；`LIMIT 1` 会在 columns 后关闭 stream，因此 side-effecting query 不会继续执行。

## C / C++

C 应用同样使用标准 SQLite API：`sqlite3_load_extension`、`sqlite3_prepare_v2`、`sqlite3_bind_*`、`sqlite3_step`、`sqlite3_finalize`。Lithograph 当前不导出 application-facing Cypher execution/transaction C ABI。示例见 [native_transaction.c](examples/native_transaction.c)，它只是 C 语言的 SQLite SQL host 示例。

## Connection pool

- 每个 connection 加载 extension；使用 Managed Semantic 的 connection 还要加载所需 Provider extension。
- active explicit transaction 必须始终留在同一 connection，直到 commit/abort。
- 未完成 streaming cursor 要么持续消费到 terminal，要么显式 close/finalize；side-effecting stream 的 writer lifetime 与消费速度相关。
- cleanup-failure quarantine 后关闭并丢弃 connection，不放回 pool。
- 不要把来自另一份 SQLite runtime 的 handle 或 allocator 混入当前 connection。

详细 SQL 合同见 [SQL API](../reference/sql-api.md)。
