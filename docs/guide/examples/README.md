<a id="当前可执行示例与历史验证记录"></a>

# 可运行示例与文档验证

适用版本：Lithograph v0.3.1 · [产品手册](../README.md) · [Cypher 手册](../../cypher/index.md)

这里的应用示例只使用 SQLite SQL，不要求 SDK、账号或外部服务。每次检查都创建临时数据库；不把业务数据库传给验证工具。Python 工具只依赖标准库，要求 Python 3.11+。Python 应用示例另要求其 SQLite 为 3.45.0+ 且允许扩展加载；文档 Cypher 与 inventory 工具还可以使用 `--sqlite3` 指定符合要求的 SQLite CLI。

| 文件 | 用途 |
| --- | --- |
| [python_quickstart.py](python_quickstart.py) | 参数绑定、图写入、stream、Tag、history、integrity |
| [version_workflow.py](version_workflow.py) | Session 重连、冲突/CAS、Diff/Patch、Commit Data、历史改写、GC、备份恢复 |
| [sql_transaction.py](sql_transaction.py) | 多次 execution 合并为一个 Commit，显式提交和回滚 |
| [streaming.py](streaming.py) | 完整消费、提前关闭、晚期错误、scalar/rows 混用、事务失败清理和安全选项 |
| [native_transaction.c](native_transaction.c) | C host 通过标准 SQLite API 加载扩展并执行同一 SQL surface |
| [verify.py](verify.py) | 按页执行 Guide SQL，并验证拒绝路径、类型编码、回滚与 CSV |
| [verify_cypher.py](verify_cypher.py) | 从 Markdown 读取所有可执行 Cypher，断言结果和具体错误 |
| [generate_reference.py](generate_reference.py) | 生成函数/Procedure 索引及受管理的签名块，支持只检查 |
| [check_docs.py](check_docs.py) | 检查公开手册本地链接、锚点与冻结 Profile 覆盖映射 |

<a id="应用示例运行"></a>

## 应用示例

在仓库根目录运行，替换成本机可信扩展的绝对路径：

```sh
python3 -B docs/guide/examples/python_quickstart.py /absolute/path/lithograph.dylib
python3 -B docs/guide/examples/version_workflow.py /absolute/path/lithograph.dylib
python3 -B docs/guide/examples/sql_transaction.py /absolute/path/lithograph.dylib
python3 -B docs/guide/examples/streaming.py /absolute/path/lithograph.dylib
```

断言全部成功后输出 PASS；失败返回非零。C host 的前置条件、编译与运行命令见 [Integration 的 C/C++ 接入](../integration.md#c-host)。

<a id="文档-sql-回归"></a>

## Guide SQL 回归

```sh
python3 -B docs/guide/examples/verify.py /absolute/path/lithograph.dylib \
  --sqlite3 /absolute/path/to/sqlite3
```

每篇指南的 `sql` 块按出现顺序运行；不同页面使用独立空库。`--sqlite3` 指定时还会使用该 CLI 重跑 Guide SQL。实服务 Provider 命令用 `text` 明确标为配置模板；带 Provider 的可执行语义示例使用下面的 synthetic fixture，不静默跳过后记作成功。

## Cypher 示例登记与执行

每个可执行 `cypher` fence 的上一行必须是稳定的 `<!-- example: unique-id -->`。完整语法模板使用 `text` fence。查询文本只保留在 Markdown；`cypher/` 下的 JSON 只保存数据准备关系与断言，例如：

```json
{
  "examples": [
    {
      "id": "reading-projection",
      "path": "docs/cypher/reading.md",
      "setup": ["reading-data"],
      "params": {"minimum": 18},
      "options": {},
      "columns": ["name"],
      "rows": [["Alice"]]
    }
  ]
}
```

`setup` 引用其它 Markdown 示例 ID，按依赖顺序执行，同一次数据库准备中去重；循环依赖失败。每个示例在 scalar 和 rows surface 分别重新初始化一个临时数据库，包括以该 surface 执行全部 setup。确定结果使用 `columns` 与 `rows`；生成的 descriptor、计划或其他不稳定值可使用 `row_count` 配合正文解释。预期失败使用 `error: "TYPE_ERROR"` 等精确错误 code。Integer 精确比较，Integer、Float 和 Boolean 不混同；Float 使用相对 `1e-9`、绝对 `1e-12` 容差。

`provider: true` 表示必须加载仓库的 synthetic Provider；缺少 `--provider` 会失败。`fixture: "csv"` 从 [LOAD CSV](../../cypher/load-csv.md) 中带 `fixture` 注释的教学 CSV fence 创建临时 `people.csv` 和 `malformed.csv`，避免重复维护测试数据。参数中的 `fixture:people.csv`、`fixture:malformed.csv` 或 `{{csv_uri}}` 替换为对应 URI，`{{csv_path}}` 替换为 people.csv 本地路径。任何私有路径或 credential 都不进入文件。

```sh
python3 -B docs/guide/examples/verify_cypher.py --check
python3 -B docs/guide/examples/verify_cypher.py /absolute/path/lithograph.dylib \
  --provider /absolute/path/synthetic_embedding_provider.dylib
```

可用 `--only 'reading|path-'` 定向执行，或 `--surface scalar` / `--surface rows` 诊断单个入口；正式门禁默认执行 `both`，不传 `--only`。rows 检查 columns-first、连续 ordinal、行宽、summary-last 和完整事件消费。登记检查覆盖全部 `docs/cypher/`、`docs/reference/`、`docs/guide/` Markdown：未登记、重名、失效 metadata、缺少断言、setup 循环均失败。`--self-test` 检查验证器不会把大整数精度丢失、错误 code 前缀或循环 setup 判为通过。固定示例结果不会替代完整实现的兼容测试。

<a id="reference-inventory-再生成"></a>
<a id="reference-generation"></a>

## Reference inventory 生成

直接从目标扩展更新结构化内容：

```sh
python3 -B docs/guide/examples/generate_reference.py \
  --extension /absolute/path/lithograph.dylib
python3 -B docs/guide/examples/generate_reference.py \
  --extension /absolute/path/lithograph.dylib --check
```

工具核对 `lithograph_version()` 与 workspace 版本、`CY25-2026.08` 和 storage format 3，再读取 `SHOW FUNCTIONS/PROCEDURES YIELD *`。`--check` 不写文件。只更新函数/Procedure 索引，以及函数正文的 `function-signatures` 标记块；参数语义、错误、null 行为及示例保持人工维护。登记 metadata 不能作为完整解码合同。

需要留存检查输入时，先导出到不提交的 target 目录：

```sh
python3 -B docs/guide/examples/verify.py /absolute/path/lithograph.dylib \
  --export-inventory target/developer-docs --inventory-only
python3 -B docs/guide/examples/generate_reference.py target/developer-docs --check
```

导出包含 `version.json`、`functions.json` 和 `procedures.json`，必须与当前目标版本一致；旧发布事实保留在历史记录中。

## 链接与覆盖门禁

```sh
python3 -B docs/guide/examples/check_docs.py
```

默认检查 README、Guide、Cypher、Reference 和 Release 的本地文件/章节链接，并核对 [覆盖映射](../../development/documentation-coverage.json)。分母直接提取自兼容矩阵：每个 capability family 的 `Required coverage` 子项、Cypher 25 Delta 和 Phase 12/13/15 supplemental inventory 都必须逐项映射，不能只登记 family 名称。每个 required 子项提供准确文档落点与已登记示例；非 Cypher runtime/SPI inventory 可以提供 `verification.command/reason`，历史已替代项必须明确标记 `superseded` 并提供原因和当前替代引用。遗漏、重复、未知项和失效引用均失败。函数与 Procedure 的逐项覆盖另外由生成索引的全量行与链接检查共同证明。`scripts/ci.sh` 在真实扩展和 synthetic Provider 构建后运行这些门禁。

下面保留的 v0.1.0 记录只代表当时发布制品，不覆盖当前 v0.3.1 的结果。

## v0.1.0 历史验证记录

2026-09-16 UTC，从 [v0.1.0 Release](https://github.com/bYiyLi/Lithograph/releases/tag/v0.1.0) 下载 `lithograph-macos-arm64.tar.gz`，核对 SHA-256：

```text
71a50eb3d7b745dc5a616a12ad1c4bc06578b0ee17a4f7f7e1d4c0c4f7c0e278
```

解压后的 `lithograph.dylib` 用于示例，而不是用修改后的引擎重新 build 来替代发布版本。基线源码为 v0.1.0 tag 的 `19b5bd9`；编写手册期间未修改引擎实现或重发资产。

<a id="已验证的范围"></a>

## v0.1.0 已验证的范围

| 检查 | 环境与结果 |
| --- | --- |
| 安装包内容、SHA-256、version/init | 已发布 macOS arm64 制品；extension 0.1.0、ABI 1、profile CY25-2026.08、format 3 |
| Python quickstart | Python 3.14.0 + SQLite 3.51.0，PASS |
| 完整 version workflow 与备份恢复 | 同上，PASS；包括预期冲突、过期 revision 拒绝和重连 |
| 指南 SQL | 9 篇指南、31 个 SQL 块、93 条语句；Python SQLite 3.51.0，以及 CLI 3.45.0 / 3.53.4 均通过 |
| 当时的 Native C | 使用 SQLite 3.51.0 header/library，`-Wall -Wextra -Werror` 编译；仅描述 v0.1.0 tag 的历史验证 |
| 发布接口 inventory | 172 条 function signature、33 个 procedure，均由发布扩展 SHOW 导出 |
| 已知问题 probes | SQL / Native checkout、Native NULL JSON，以及 Procedure 类型 metadata 不一致；已记录并采用显式规避 |

全部验证命令的本地执行结果同时记录在 [当日开发日志](../../vlog/2026-09-16.md)。这个表区分成功教程与预期失败 probe；已知问题见 [Known Issues](../../reference/known-issues.md)，不宣称缺陷已修复。

<a id="不包含的验证"></a>

## v0.1.0 不包含的验证

该次 v0.1.0 文档验证没有重新运行六平台 Release Matrix、完整 Rust CI、全部 inherited TCK 或大规模性能压测。Linux/Windows 制品存在于已发布资产，但本轮运行示例的平台是 macOS arm64；也没有为所有 Node.js/Rust/Go binding 声称兼容性已实测。

当时 v0.1.0 手册与示例不依赖新的 engine behavior。用户文档可以纠错，但 v0.1.0 的事实不能被未来实现结果替换；修复发布缺陷应进入独立的实现、回归和新版本发布流程。
