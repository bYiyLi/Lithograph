# v0.1.0 可执行示例与验证记录

这些代码面向第三方应用开发者，不是 SDK，也不要求阅读引擎实现。使用可信、校验过的 v0.1.0 发布扩展，以及支持 extension loading 的 SQLite 3.45.0+ 运行时。

| 文件 | 用途与安全范围 |
| --- | --- |
| [python_quickstart.py](python_quickstart.py) | 标准库 Python、内存库；SQL 参数绑定、图写入、stream、Tag、history、integrity |
| [version_workflow.py](version_workflow.py) | 独立临时目录；持久 Session/重连/冲突/CAS、Diff/Patch、Data、历史改写、GC、备份恢复 |
| [native_transaction.c](native_transaction.c) | POSIX C，内存库；两次写入一个 Commit、events/ownership、取消与 fail-closed abort |
| [verify.py](verify.py) | 提取指南 SQL，在独立空库执行；额外检查失败路径、类型保真和 CSV |
| [generate_reference.py](generate_reference.py) | 从已验证发布制品的 SHOW 导出重建 function/procedure reference tables |

示例不接受已有业务数据库作为默认目标，不需要账户、密钥或外部服务。Python 代码只依赖标准库；运行时仍须能加载本机扩展。C 示例使用 POSIX dynamic loader，不提供未经验证的 Windows build 脚本。

## 应用示例运行

在仓库根目录，替换扩展为实际绝对路径：

```sh
python3 -B docs/guide/examples/python_quickstart.py /absolute/path/lithograph.dylib
python3 -B docs/guide/examples/version_workflow.py /absolute/path/lithograph.dylib
```

程序全部断言成功后输出 PASS，失败返回非零。C 编译与运行见 [Integration](../integration.md)。默认 Native 示例会故意取消一次 query，输出 `RESOURCE_ERROR` / `SQLITE_INTERRUPT` 是该负向验证的预期诊断；最终仍须有 PASS 和退出码 0。

## 文档 SQL 回归

```sh
python3 -B docs/guide/examples/verify.py /absolute/path/lithograph.dylib \
  --sqlite3 /absolute/path/to/sqlite3
```

`--sqlite3` 可省略，省略时只使用 Python 绑定的 SQLite。指定时必须使用实际可执行 CLI 路径；脚本在临时工作目录运行 CLI，因此不依赖当前目录的文件。每篇指南的 SQL 块按出现顺序执行，但不同指南的库彼此隔离。

该检查不自动执行 shell 安装指令、含示意变量的语法参考、以及明确用于复现错误的 Known Issues 块。安装制品校验、Native C 和 Known Issues probes 是独立验证项，不能把“有一个脚本 PASS”写成整套手册全部已验证。

## Reference inventory 再生成

```sh
python3 -B docs/guide/examples/verify.py /absolute/path/lithograph.dylib \
  --export-inventory target/developer-docs-v010 --inventory-only
python3 -B docs/guide/examples/generate_reference.py target/developer-docs-v010
```

导出先检查 `extension == 0.1.0`，再执行 `SHOW FUNCTIONS YIELD *` 与 `SHOW PROCEDURES YIELD *`。JSON 临时输出留在 target，不提交；生成的两个 Markdown 表是用户 Reference。不要拿未来版本的导出覆盖标为 v0.1.0 的表。

## 本次使用的真实制品

2026-09-16 UTC，从 [v0.1.0 Release](https://github.com/bYiyLi/Lithograph/releases/tag/v0.1.0) 下载 `lithograph-macos-arm64.tar.gz`，核对 SHA-256：

```text
71a50eb3d7b745dc5a616a12ad1c4bc06578b0ee17a4f7f7e1d4c0c4f7c0e278
```

解压后的 `lithograph.dylib` 用于示例，而不是用修改后的引擎重新 build 来替代发布版本。基线源码为 v0.1.0 tag 的 `19b5bd9`；编写手册期间未修改引擎实现或重发资产。

## 已验证的范围

| 检查 | 环境与结果 |
| --- | --- |
| 安装包内容、SHA-256、version/init | 已发布 macOS arm64 制品；extension 0.1.0、ABI 1、profile CY25-2026.08、format 3 |
| Python quickstart | Python 3.14.0 + SQLite 3.51.0，PASS |
| 完整 version workflow 与备份恢复 | 同上，PASS；包括预期冲突、过期 revision 拒绝和重连 |
| 指南 SQL | 9 篇指南、31 个 SQL 块、93 条语句；Python SQLite 3.51.0，以及 CLI 3.45.0 / 3.53.4 均通过 |
| Native C | 使用 SQLite 3.51.0 header/library，`-Wall -Wextra -Werror` 编译，默认事务/取消/完整性流程 PASS |
| 发布接口 inventory | 172 条 function signature、33 个 procedure，均由发布扩展 SHOW 导出 |
| 已知问题 probes | SQL / Native checkout、Native NULL JSON，以及 Procedure 类型 metadata 不一致；已记录并采用显式规避 |

全部验证命令的本地执行结果同时记录在 [当日开发日志](../../vlog/2026-09-16.md)。这个表区分成功教程与预期失败 probe；已知问题见 [Known Issues](../../reference/known-issues.md)，不宣称缺陷已修复。

## 不包含的验证

本轮没有重新运行六平台 Release Matrix、完整 Rust CI、全部 inherited TCK 或大规模性能压测。Linux/Windows 制品存在于已发布资产，但本轮运行示例的平台是 macOS arm64；也没有为所有 Node.js/Rust/Go binding 声称兼容性已实测。

本手册和示例不依赖新的 engine behavior。用户文档可以纠错，但 v0.1.0 的事实不能被未来实现结果替换；修复发布缺陷应进入独立的实现、回归和新版本发布流程。
