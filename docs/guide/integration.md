# 应用集成

[产品手册](README.md) · [Cypher 语言手册](../cypher/index.md) · [接口参考](../reference/README.md)

本文适用 **v0.3.2**（`CY25-2026.08`，storage format `3`）。

v0.3.1 的 **application-facing execution 统一走 SQLite SQL**。应用不需要也不应解析 Lithograph shared library 的 query symbols；只需要使用自己的 SQLite driver/`sqlite3*` 加载 extension 并执行 SQL。

| 应用需要 | 使用入口 |
| --- | --- |
| 普通读写、Schema、Search、版本管理 | `lithograph()` |
| 增量结果 / 大结果 | `lithograph_rows()` |
| 多次 execution 最终一个 Commit | `lithograph_tx_begin()` + 普通 execution + `commit/abort` |
| `CALL ... IN TRANSACTIONS` | 普通 `lithograph()` / `lithograph_rows()`，SQLite autocommit mode |
| 自定义 Embedding implementation | `EmbeddingProviderV1` SPI（Provider 作者使用，不是 application query API） |

<a id="python"></a>

## Python

```python
import json
import sqlite3

db = sqlite3.connect("graph.db", isolation_level=None)
db.enable_load_extension(True)
try:
    db.load_extension("/absolute/path/lithograph.dylib")
finally:
    db.enable_load_extension(False)
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

<a id="streaming"></a>

### Streaming

`columns` 只描述结果结构；随后到来的 `row` 仍可能是未提交结果。必须消费到 `summary` 才能把普通 execution 认定为成功；处理了部分 row 后报错时，下游也要能撤销、丢弃或标为失败，不能把这些 row 当成已提交事实。

```python
cursor = db.execute(
    "SELECT ordinal,event,data FROM lithograph_rows(?, ?, ?)",
    ("MATCH (p:Person) RETURN p.name ORDER BY p.name", "{}", "{}"),
)
summary = None
try:
    for ordinal, event, data in cursor:
        payload = json.loads(data)
        if event == "columns":
            columns = payload
        elif event == "row":
            print(dict(zip(columns, payload)))
        elif event == "summary":
            summary = payload
    if summary is None:
        raise RuntimeError("execution did not reach its success summary")
finally:
    cursor.close()
```

需要以 `summary` 作为 execution success evidence 时，不要在外层 SQL 把它过滤掉。`WHERE event='row'` 适合只消费 row payload，但会隐藏 summary；`LIMIT 1` 会在 columns 后关闭 stream，因此 side-effecting query 不会继续执行。

要提前停止时在 `finally` 中 `close()`。普通写 stream 在 finalize-success 前 close 会回滚本次 execution；active explicit transaction 内的 incomplete stream 会 abort 整个 transaction。已经提交的 transaction-subquery batch 则保持提交。不要在一个尚未结束的写 cursor 内，再从同一 connection 启动另一个 graph/version execution；先消费完成或 close，再发下一条。

完整可运行的 [streaming.py](examples/streaming.py) 覆盖 scalar/rows 混用、完整消费、提前关闭、晚期错误、事务自动 abort、安全选项和 `expectedHead` 冲突。使用实际支持 SQLite 3.45.0+ 和扩展加载的 Python 执行：

```sh
python3 docs/guide/examples/streaming.py /absolute/path/lithograph.dylib
```

该程序只创建临时演示库，失败以非零状态退出。运行前先用 `python3 -c 'import sqlite3; print(sqlite3.sqlite_version)'` 核对实际 Python runtime。

<a id="cancellation-and-outcomes"></a>

### 取消与失败后的结果确认

应用可以用 driver 提供的 `interrupt()`（C 为 `sqlite3_interrupt()`）取消执行；耗时 operator 在检查点停止，返回 SQLite interrupt。跨线程调用必须遵守实际 binding 与 SQLite threading mode。取消后 close/finalize 对应 cursor，再检查事务是否已经自动 abort。

Unreleased / Phase 19 将中断检查传入长表达式和函数循环，包括嵌套 `reduce()`；可分块的长文本也分段检查。第三方不可拆分调用只保证调用前后检查，阻塞中的单次调用仍受其已有 timeout 合同约束。宿主使用自己的请求期限调用 `interrupt()`，不存在 query `timeout` option。

真实中断可能使 caller-owned SQLite transaction 整体回滚，包含此前尚未提交的宿主业务写入。Phase 19 的普通潜在写与 active explicit execution 会确认 SQLite 已恢复到 autocommit；无法证明清理完成时隔离 connection 并返回 `INTERNAL_ERROR`，应关闭丢弃。应用 finalize/close 后检查实际事务状态，不把收到 code 9 理解成只撤销本次调用的内部 SAVEPOINT；连接池复用条件见 [取消与连接池](../reference/transactions.md#cancellation)。

Lithograph 会在普通 execution 的 finalize-success 前预检自己的 JSON 编码和长度限制，已检测到的失败会走回滚。SQLite 最终交付结果或宿主后续处理结果时仍可能失败，调用方有时无法确认是否已经成功；遇到这种不确定状态，先通过 Branch head、业务键和 history 核对结果，再决定是否重放。batch 查询的最终 envelope/stream 即使失败，也可能已提交 earlier batch；参见 [事务生命周期](../reference/transactions.md)。

<a id="execution-memory-budget"></a>

### 设置 execution 内存预算（Unreleased）

以下能力属于 Unreleased 变更，当前实施与验收状态见 [Phase 19](../development/phases/19-query-correctness-resources.md)；**v0.3.2 发行库不接受 `memoryLimitBytes`**。宿主使用包含本功能的构建时，在原有受控 options 中增加正整数预算；下例继续使用已加载扩展、初始化完成的 `db`，且只允许客户端传 query/params：

```python
def budgeted_read(db, cypher, params):
    options = {
        "readOnlyNoExternalIO": True,
        "memoryLimitBytes": 64 * 1024 * 1024,
    }
    cursor = db.execute(
        "SELECT lithograph(?, ?, ?)",
        (cypher, json.dumps(params, allow_nan=False), json.dumps(options)),
    )
    try:
        return json.loads(cursor.fetchone()[0])
    finally:
        cursor.close()
```

64 MiB 是此宿主示例的选择，不是 Lithograph 默认值。省略该字段保持原有行为；不要让客户端覆盖或在失败重试时移除配额。预算按每次 execution 独立指定，包含参数、中间值及结果编码/收集；active explicit transaction 内也要每次传入，不能放在 `tx_begin`。

例如 `RETURN size(range(1,500000)) AS c` 只返回一个数字，但仍需构造中间 List；包含本功能的构建在 16 MiB 下返回 `RESOURCE_ERROR` / `SQLITE_TOOBIG`，在 256 MiB 下返回 `500000`。结果越大时优先选择 rows 接口，使用完全相同的受控 options，完整消费到 summary 和 `SQLITE_DONE`；scalar 需要保留完整 envelope，更容易达到预算。预算失败遵循现有回滚规则，已 durable batch 仍可能存在。

这项预算不是进程 RSS 硬上限，不覆盖 AST/plan、SQLite 页缓存、Provider 内存、宿主已有输入缓冲或 allocator 隐含开销；主服务的硬隔离、并发和期限仍由宿主管理。范围与错误精确说明见 [Execution Options](../reference/execution-options.md#memory-limit)，实际验收状态见 [Phase 19](../development/phases/19-query-correctness-resources.md)。

<a id="c-host"></a>

## C / C++

C 应用同样使用标准 SQLite API：`sqlite3_load_extension`、`sqlite3_prepare_v2`、`sqlite3_bind_*`、`sqlite3_step`、`sqlite3_finalize`。Lithograph 当前不导出 application-facing Cypher execution/transaction C ABI。示例见 [native_transaction.c](examples/native_transaction.c)，它只是 C 语言的 SQLite SQL host 示例。

在仓库根目录准备 C11 compiler，以及支持 FTS5、extension loading 和线程安全的 SQLite **3.45.0+** 开发头文件与共享库。头文件和链接库必须取自同一套 SQLite 安装；不要混用 macOS SDK header 与另一个前缀的 library。下面的程序打开内存演示库，不读取业务数据库。

macOS 使用 Homebrew SQLite，`brew --prefix sqlite` 会给出当前安装前缀，兼容 Apple Silicon 与 Intel 的不同默认路径：

```sh
lithograph_sqlite_prefix="$(brew --prefix sqlite)"
"$lithograph_sqlite_prefix/bin/sqlite3" --version
mkdir -p target/developer-docs
cc -std=c11 -Wall -Wextra -Werror \
  -I"$lithograph_sqlite_prefix/include" \
  docs/guide/examples/native_transaction.c \
  -L"$lithograph_sqlite_prefix/lib" \
  -Wl,-rpath,"$lithograph_sqlite_prefix/lib" -lsqlite3 \
  -o target/developer-docs/native_transaction
target/developer-docs/native_transaction /absolute/path/lithograph.dylib
```

把最后一行替换为实际可信扩展的绝对路径；例如本地 Cargo 构建为 `target/debug/liblithograph.dylib`，发布包文件为 `lithograph.dylib`。成功输出 `PASS: C host used SQL-only Lithograph execution`；非零退出时不要把该 binary 当成已经验证的接入环境。

Linux 已安装 SQLite development package 与 `pkg-config` 时，从同一个 `sqlite3.pc` 获取 include/lib 路径；不要假定多架构系统的 library 一定在 `/usr/lib`：

```sh
pkg-config --modversion sqlite3
lithograph_sqlite_include="$(pkg-config --variable=includedir sqlite3)"
lithograph_sqlite_lib="$(pkg-config --variable=libdir sqlite3)"
mkdir -p target/developer-docs
cc -std=c11 -Wall -Wextra -Werror \
  -I"$lithograph_sqlite_include" \
  docs/guide/examples/native_transaction.c \
  -L"$lithograph_sqlite_lib" -Wl,-rpath,"$lithograph_sqlite_lib" -lsqlite3 \
  -o target/developer-docs/native_transaction
target/developer-docs/native_transaction /absolute/path/lithograph.so
```

若 pkg-config 显示版本低于最低要求，先选择符合要求的 SQLite 安装。自行安装的 SQLite 可通过 `PKG_CONFIG_PATH` 指向该安装的 pkgconfig 目录后重复上述检查；include/lib 必须仍来自同一份 metadata。Windows 使用同样源文件和目标 SQLite 的 header/import library 构建对应架构程序，运行时提供同一套 SQLite DLL；这里的 `cc` 命令只适用于 macOS/Linux。

<a id="connection-pool"></a>

## Connection pool

- 每个 connection 加载 extension；使用 Managed Semantic 的 connection 还要加载所需 Provider extension。
- active explicit transaction 必须始终留在同一 connection，直到 commit/abort。
- 未完成 streaming cursor 要么持续消费到 terminal，要么显式 close/finalize；side-effecting stream 的 writer lifetime 与消费速度相关。
- cleanup-failure quarantine 后关闭并丢弃 connection，不放回 pool。
- 不要把来自另一份 SQLite runtime 的 handle 或 allocator 混入当前 connection。

连接归还前应确认已无未完成 cursor，并用 `sqlite3_get_autocommit()`（Python 为 `not db.in_transaction`）确认不在 SQLite transaction 中；如果仍有本请求拥有的 active transaction，先按其 lifecycle abort/rollback。应用自行改变过 checkout 时还要恢复并验证池约定的基线 Branch。无法证明清理成功时丢弃 connection，重新打开并加载扩展。原始 SQL 权限与 options 构造由宿主控制；开放不可信 Cypher 的接入流程见 [安全执行](safe-execution.md)。

详细 SQL 合同见 [SQL API](../reference/sql-api.md)、[Execution Options](../reference/execution-options.md) 与 [事务生命周期](../reference/transactions.md)。
