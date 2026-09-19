# 应用集成

适用 v0.1.0。Lithograph 的交付物是 SQLite loadable extension，不要求另起 Server。选择入口时先判断需要的是普通查询、流式读取，还是多 execution 单 Commit。

| 应用需要 | 使用入口 |
| --- | --- |
| 普通读写、Schema、Search、版本管理 | SQL `lithograph()` |
| 大量只读行，不把全部结果放入一个 JSON | SQL `lithograph_rows()` |
| 直接接收 COLUMNS/ROW/SUMMARY callback | `lithograph_v1_execute` |
| 多次标准 Cypher 操作最后形成一个 Commit | Native `tx_*` |
| `CALL ... IN TRANSACTIONS` 导入 | 普通 Native execute，idle/autocommit connection |

## Python：完整可运行示例

只使用标准库，无专用 SDK。先检查 Python 实际链接的 SQLite：

```sh
python3 -c "import sqlite3; print(sqlite3.sqlite_version); print(hasattr(sqlite3.Connection, 'enable_load_extension'))"
```

需要 SQLite 3.45.0+ 且 loading 为 True；否则更换或构建带扩展加载支持的 Python 运行时。单独安装另一个 SQLite CLI 不会自动替换 Python 链接的 SQLite。

在仓库根目录执行，扩展路径替换成下载并校验过的绝对路径：

```sh
python3 -B docs/guide/examples/python_quickstart.py /absolute/path/lithograph.dylib
python3 -B docs/guide/examples/version_workflow.py /absolute/path/lithograph.dylib
```

第一个程序在内存中演示创建、参数绑定、流式读取、Tag 和 time-travel。第二个使用自动清理的临时目录，覆盖持久 Merge Session、resolution、candidate、Diff/Patch、Commit Data、Rebase/Squash/Reset/Revert、GC 与一致性备份。成功时输出 PASS，失败会抛错并返回非零；不会删除已有数据库。

核心调用方式如下；完整 imports、版本检查、连接关闭和异常处理保存在 [python_quickstart.py](examples/python_quickstart.py)：

```python
payload = (
    "MATCH (p:Person) WHERE p.name=$name RETURN p.name AS name",
    json.dumps({"name": "Alice"}, allow_nan=False),
    json.dumps({"branch": "main"}, allow_nan=False),
)
cursor = db.execute("SELECT lithograph(?, ?, ?)", payload)
try:
    result = json.loads(cursor.fetchone()[0])
finally:
    cursor.close()
```

Cypher 参数和 SQL 参数绑定都要保留，不能用 f-string 拼接用户输入。得到 JSON 后保留 `$type` wrappers；不要无条件把 Node 改成普通 Property Map，或把大整数改成 float。

示例的 `open_graph(...,initialize=True)` 只适合主动创建/迁移的场景。打开已初始化业务库时传 `initialize=False`，在专门的 provisioning / migration 步骤中执行 init，不要每个请求都迁移或 integrity scan。

Python `sqlite3` 不公开可安全直接交给 C ABI 的 `sqlite3*`。不要用 ctypes 猜测 Python connection 的内部内存地址来调用 Native tx。仅通过 SQL Bridge 接入时，使用单条复合 Cypher 表达原子操作；确需多 execution 单 Commit，选择拥有原生 handle 的集成层。

## Native C：多条语句，一个 Commit

示例 [native_transaction.c](examples/native_transaction.c) 使用 POSIX `dlopen`，适用于 macOS/Linux。Windows 发布制品支持相同 ABI，但加载符号需使用 Windows loader；本示例不是已经验证过的 Windows 构建脚本。

在 macOS、仓库根目录中编译。先令 `SQLITE_PREFIX` 指向支持 extension loading 的 SQLite 安装前缀；例如本轮验证使用 `/opt/homebrew/opt/sqlite`，不是 macOS SDK 自带 SQLite：

```sh
SQLITE_PREFIX=/opt/homebrew/opt/sqlite
cc -std=c11 -Wall -Wextra -Werror -Iinclude \
  -I"$SQLITE_PREFIX/include" -L"$SQLITE_PREFIX/lib" \
  docs/guide/examples/native_transaction.c -lsqlite3 \
  -o /tmp/lithograph-native-demo
/tmp/lithograph-native-demo /absolute/path/lithograph.dylib
```

Linux 编译时额外链接 `-ldl`，使用 `.so` 扩展。开发机需要 SQLite header / library；自定义 SQLite 安装添加对应 `-I` / `-L`，确保运行时动态链接的是同一套 host SQLite。

不取源码也可以下载这个 C 示例和 [v0.1.0 header](https://raw.githubusercontent.com/bYiyLi/Lithograph/v0.1.0/include/lithograph.h)，把编译命令的源文件/header 目录替换成保存位置。

程序先在 `sqlite3*` 上通过 `sqlite3_load_extension` 完成注册，再用 dynamic loader 解析 Native symbol。只调用 `dlopen` 不够。v0.1.0–v0.2.1 的 params/options 必须显式传 `"{}",2`，不能用 `NULL,0` 代替默认 object，见 [已知问题](../reference/known-issues.md)。两次 staged CREATE 的 SUMMARY 中 commit 为 null；tx_commit 返回最终 Commit；检查 log 只有 Root + 一个新 Commit。

示例还检查 callback cancellation 会自动 abort，并区分 SQLite 分配的错误（`sqlite3_free`）与 Lithograph 分配的 JSON（`lithograph_v1_free`）。生产程序应把示例的失败即退出改成所属应用的结构化错误路径，避免在有未完成事务时继续复用 connection。

## 其他语言和连接池

Node.js、Rust、Go 等不需要 Lithograph 特有查询语言 SDK，但**具体 SQLite binding 必须**允许加载 extension、提供符合最低版本与 FTS5 的 runtime，并允许目标部署环境的 native library loading。能执行普通 SQL 不自动表示能加载扩展。

纯 SQL binding 可以使用 SQL Bridge，包括 `lithograph_tx_*` 显式事务；只有需要 Native streaming callback 或其它 C ABI 能力的 binding 才必须可靠暴露同一 host runtime 的 `sqlite3*`。不要同时向一个 connection 混入来自另一份 SQLite library 的 handle 或内存释放函数。

每个池连接加载一次扩展；保留每次 query 的 branch/at 上下文，不把上个请求的业务目标泄漏到下个请求。关闭流式 cursor，显式处理 outer transaction，丢弃 cleanup 失败的 connection。SQLite connection threading mode 不因 Lithograph 自动增强。

本轮实测环境与未验证的平台见 [验证说明](examples/README.md)。Python loading/backup 行为依据 [Python sqlite3 官方文档](https://docs.python.org/3/library/sqlite3.html)，Native 协议见 [Native API](../reference/native-api.md)。
