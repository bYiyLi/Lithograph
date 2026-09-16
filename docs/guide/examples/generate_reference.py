#!/usr/bin/env python3
"""Render version-pinned function/procedure tables from verified SHOW exports."""

import argparse
import json
from pathlib import Path


def records(directory, name):
    envelope = json.loads((directory / (name + ".json")).read_text())
    return [dict(zip(envelope["columns"], row)) for row in envelope["rows"]]


def code(value):
    return "`" + str(value).replace("|", "\\|").replace("`", "&#96;") + "`"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("inventory", type=Path)
    args = parser.parse_args()
    reference = Path(__file__).resolve().parents[2] / "reference"
    functions = records(args.inventory, "functions")
    procedures = records(args.inventory, "procedures")
    unique = len({row["name"] for row in functions})
    lines = ["# Built-in Functions — v0.1.0", "",
        f"来自 v0.1.0 `SHOW FUNCTIONS YIELD *`：{len(functions)} 条签名，{unique} 个名称（重载分别列出）。",
        "范围固定为 `CY25-2026.08`。签名中的 `::` 是 introspection 类型说明，不是调用时要输入的字符。",
        "",
        "在 Cypher 中通过 `RETURN functionName(...)` 调用；聚合函数在分组上下文工作。",
        "签名不取消具体值的类型/维度/时区等校验；以 [Values](values.md) 和运行错误为准。",
        "`id()` 的替代接口是 `elementId()`。`PROPERTY_EXISTS` 是语言 predicate，不作为函数出现在本清单。",
        "",
        "```cypher", "SHOW FUNCTIONS YIELD name, signature, category",
        "RETURN name, signature, category ORDER BY name", "```", "",
        "| 签名 | 分类 | Deprecated / replacement |", "| --- | --- | --- |"]
    for row in functions:
        deprecated = str(row["deprecatedBy"] or "yes") if row["isDeprecated"] else "—"
        lines.append("| " + code(row["signature"]) + " | " + row["category"] + " | " + deprecated + " |")
    lines.extend(["", "依据：[v0.1.0 函数注册表](../../crates/lithograph-core/src/query/registry.rs)、发布制品 introspection。",
                  "更新方法见 [验证说明](../guide/examples/README.md)。不要按新版外部数据库清单直接扩充此版本。", ""])
    (reference / "functions.md").write_text("\n".join(lines))
    lines = ["# Procedure Inventory — v0.1.0", "",
        f"来自 v0.1.0 `SHOW PROCEDURES YIELD *` 的全部 {len(procedures)} 个公开 Procedure。",
        "参数的可选方括号是签名说明，不是实际 Cypher 调用字符。返回列可由 YIELD / RETURN 投影。",
        "",
        "**登记存在不等于每种 adapter 都能执行。** checkout 的发布问题见 [Known Issues](known-issues.md)，",
        "SQL rows 只读限制、Native transaction 限制及每个参数的语义见 [Procedures](procedures.md)。",
        "WRITE mode 也可能仅修改 ref / sidecar / cache，并不总是产生 Commit。", "",
        "v0.1.0 的 SHOW returnDescription.type 全部标为 STRING，argumentDescription 也为空；",
        "这些 metadata 不能用于生成参数/结果解码器。本表只采用真实列名，值类型见 [Procedure Reference](procedures.md)。",
        "相关复现见 [DOC-V010-03](known-issues.md)。", "",
        "| 签名 | Mode | 输出列 |", "| --- | --- | --- |"]
    for row in procedures:
        outputs = ", ".join(item["name"] for item in row["returnDescription"])
        lines.append("| " + code(row["signature"]) + " | " + row["mode"] + " | " + code(outputs) + " |")
    lines.extend(["", "依据：[注册表](../../crates/lithograph-core/src/query/registry.rs)、发布制品 introspection。",
                  "`rolesExecution` 等兼容字段不表示 Lithograph 有独立账号、RBAC 或 system database。", ""])
    (reference / "procedure-inventory.md").write_text("\n".join(lines))
    print("Rendered", len(functions), "function signatures and", len(procedures), "procedures")


if __name__ == "__main__":
    main()
