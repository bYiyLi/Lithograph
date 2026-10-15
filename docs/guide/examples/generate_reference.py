#!/usr/bin/env python3
"""Render runtime inventory and managed signature blocks; --check never writes."""

import argparse
import json
from pathlib import Path
import re
import tempfile

from doc_runtime import DocGraph, PROFILE, ROOT, workspace_version


REFERENCE = ROOT / "docs/reference"
CATEGORIES = {
    "Numeric": "mathematical", "Trigonometric": "mathematical",
    "Logarithmic": "mathematical", "Scalar": "scalar", "Database": "scalar",
    "String": "string", "List": "list", "Predicate": "predicate",
    "Aggregating": "aggregation", "Temporal": "temporal", "Spatial": "spatial",
    "Vector": "vector",
}


def records(envelope):
    return [dict(zip(envelope["columns"], row)) for row in envelope["rows"]]


def function_page(row):
    return "vector" if row["name"] == "vector" else CATEGORIES[row["category"]]


def function_anchor(name):
    return "fn-" + name.lower().replace(".", "-")


def procedure_page(name):
    if name.startswith("db.index.fulltext."):
        return "fulltext"
    if name.startswith("db.index.semantic."):
        return "semantic"
    if name.startswith("db."):
        return "graph"
    if name.startswith("lithograph.merge."):
        return "merge"
    if name in ("lithograph.index.rebuild", "lithograph.gc"):
        return "maintenance"
    return "versioning"


def procedure_anchor(name):
    return "proc-" + name.lower().replace(".", "-")


def code(value):
    return "`" + str(value).replace("|", "\\|").replace("`", "&#96;") + "`"


def render_functions(functions, version):
    unique = len({row["name"] for row in functions})
    lines = ["# 内置函数索引", "", "适用版本：Lithograph v" + version +
             " · [产品手册](../guide/README.md) · [Cypher 手册](../cypher/index.md)", "",
             "本表由目标扩展的 `SHOW FUNCTIONS YIELD *` 生成：" + str(unique) +
             " 个名称，" + str(len(functions)) + " 条签名。重载分别列出；分类保持运行时登记值。",
             "Cypher Profile 为 `" + PROFILE + "`。`::`、可选方括号和默认值是签名说明，"
             "实际调用形式、参数语义、null/type/error 和示例见每行的语言参考。", "",
             "签名描述运行时登记信息，不能代替具体类型和维度检查；宿主参数与返回值编码见 "
             "[Values](values.md)。`id()` 的替代接口是 `elementId()`。", "",
             "<!-- generated: functions -->", "| 签名 | 分类 | Deprecated / replacement | 使用说明 |",
             "| --- | --- | --- | --- |"]
    for row in sorted(functions, key=lambda row: (row["name"].lower(), row["category"], row["signature"])):
        deprecated = str(row["deprecatedBy"] or "yes") if row["isDeprecated"] else "—"
        target = "../cypher/functions/" + function_page(row) + ".md#" + function_anchor(row["name"])
        lines.append("| " + code(row["signature"]) + " | " + row["category"] + " | " +
                     deprecated + " | [" + row["name"] + "](" + target + ") |")
    lines.extend(["<!-- /generated: functions -->", "", "更新和只读一致性检查见 "
                  "[示例验证说明](../guide/examples/README.md#reference-generation)。", ""])
    return "\n".join(lines)


def render_procedures(procedures, version):
    lines = ["# Procedure 清单", "", "适用版本：Lithograph v" + version +
             " · [产品手册](../guide/README.md) · [Procedure Reference](procedures.md)", "",
             "本表由目标扩展的 `SHOW PROCEDURES YIELD *` 生成，共 " + str(len(procedures)) +
             " 个公开 Procedure。参数的可选方括号是签名记号；返回列可由 `YIELD` 投影。", "",
             "Mode 表示登记分类。`WRITE` 操作可能只修改 ref/sidecar；是否产生 Commit、"
             "事务与历史 Snapshot 限制见每行的完整说明。登记存在不表示所有 execution context 都允许调用。", "",
             "表中输出列名来自 `returnDescription`。introspection 的 argument/return 类型 metadata "
             "不用于生成解码器：完整值类型由对应 Procedure Reference 规定；"
             "metadata 的已知差异见 [Known Issues](known-issues.md)。", "",
             "<!-- generated: procedures -->", "| 签名 | Mode | 输出列 | 完整说明 |",
             "| --- | --- | --- | --- |"]
    for row in sorted(procedures, key=lambda row: row["name"]):
        outputs = ", ".join(item["name"] for item in row["returnDescription"])
        target = "procedures/" + procedure_page(row["name"]) + ".md#" + procedure_anchor(row["name"])
        lines.append("| " + code(row["signature"]) + " | " + row["mode"] + " | " +
                     (code(outputs) if outputs else "无") + " | [" + row["name"] + "](" + target + ") |")
    lines.extend(["<!-- /generated: procedures -->", "", "`rolesExecution` 等兼容字段不表示 "
                  "Lithograph 提供独立账号、RBAC 或 system database。更新方式见 "
                  "[示例验证说明](../guide/examples/README.md#reference-generation)。", ""])
    return "\n".join(lines)


def managed_signatures(functions):
    rendered = {}
    managed = set()
    for path in sorted((ROOT / "docs/cypher/functions").glob("*.md")):
        original = path.read_text()

        def replace(match):
            name = match["name"]
            matches = [row for row in functions if row["name"] == name and function_page(row) == path.stem]
            if not matches:
                raise RuntimeError(str(path.relative_to(ROOT)) + ": no runtime signature for " + name)
            key = (name, path.stem)
            if key in managed:
                raise RuntimeError(str(path.relative_to(ROOT)) + ": duplicate signature block for " + name)
            managed.add(key)
            signatures = "\n".join(sorted(row["signature"] for row in matches))
            return ("<!-- function-signatures: " + name + " -->\n```text\n" + signatures +
                    "\n```\n<!-- /function-signatures -->")

        pattern = r"<!-- function-signatures: (?P<name>[^\n]+?) -->\n.*?<!-- /function-signatures -->"
        rendered[path] = re.sub(pattern, replace, original, flags=re.S)
    missing = {(row["name"], function_page(row)) for row in functions} - managed
    if missing:
        raise RuntimeError("Runtime functions without a managed manual signature: " +
                           ", ".join(name + " (" + page + ")" for name, page in sorted(missing)))
    return rendered


def load_inventory(args):
    if args.extension:
        with tempfile.TemporaryDirectory(prefix="lithograph-doc-inventory-") as directory:
            with DocGraph(args.extension, directory, args.sqlite3) as graph:
                version = graph.check_version()
                functions = records(graph.query("SHOW FUNCTIONS YIELD *"))
                procedures = records(graph.query("SHOW PROCEDURES YIELD *"))
        return version, functions, procedures
    if not args.inventory:
        raise RuntimeError("Pass an exported inventory directory or --extension")
    directory = args.inventory
    version = json.loads((directory / "version.json").read_text())
    if version.get("extension") != workspace_version() or version.get("cypherProfile") != PROFILE:
        raise RuntimeError("Inventory version/profile differs from the target workspace")
    return version, records(json.loads((directory / "functions.json").read_text())), records(
        json.loads((directory / "procedures.json").read_text()))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("inventory", type=Path, nargs="?")
    parser.add_argument("--extension", type=Path)
    parser.add_argument("--sqlite3", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    version, functions, procedures = load_inventory(args)
    rendered = {REFERENCE / "functions.md": render_functions(functions, version["extension"]),
                REFERENCE / "procedure-inventory.md": render_procedures(procedures, version["extension"])}
    rendered.update(managed_signatures(functions))
    stale = []
    for path, content in rendered.items():
        if args.check:
            if not path.exists() or path.read_text() != content:
                stale.append(str(path.relative_to(ROOT)))
        elif not path.exists() or path.read_text() != content:
            path.write_text(content)
    if stale:
        raise SystemExit("Generated documentation is stale:\n" + "\n".join(stale))
    print("PASS:" if args.check else "Rendered", len(functions), "function signatures and",
          len(procedures), "procedures for v" + version["extension"])


if __name__ == "__main__":
    main()
