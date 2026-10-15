#!/usr/bin/env python3
"""Execute every registered public-handbook Cypher example with result assertions."""

import argparse
import json
import math
from pathlib import Path
import re
import tempfile

from doc_runtime import DocGraph, ROOT


MARKER = re.compile(r"^<!-- example: ([a-zA-Z0-9][a-zA-Z0-9_.-]*) -->$")
PUBLIC = (ROOT / "docs/cypher", ROOT / "docs/guide", ROOT / "docs/reference")
INDEX = ROOT / "docs/guide/examples/cypher"


def read_blocks():
    blocks = {}
    errors = []
    for directory in PUBLIC:
        for path in sorted(directory.rglob("*.md")):
            lines = path.read_text().splitlines()
            position = 0
            while position < len(lines):
                start = re.match(r"^(`{3,}|~{3,})(\S*)\s*$", lines[position])
                if not start:
                    position += 1
                    continue
                fence, language = start.groups()
                begin = position
                position += 1
                while position < len(lines) and not re.match(
                    r"^" + re.escape(fence[0]) + "{" + str(len(fence)) + r",}\s*$",
                    lines[position],
                ):
                    position += 1
                if language.lower() == "cypher":
                    marker = MARKER.fullmatch(lines[begin - 1]) if begin else None
                    location = str(path.relative_to(ROOT)) + ":" + str(begin + 1)
                    if not marker:
                        errors.append(location + ": Cypher block has no example marker "
                                      "(use text for a syntax template)")
                    elif marker[1] in blocks:
                        errors.append(location + ": duplicate example ID " + marker[1])
                    elif position == len(lines):
                        errors.append(location + ": unclosed Cypher fence")
                    else:
                        blocks[marker[1]] = {"query": "\n".join(lines[begin + 1:position]),
                                             "path": str(path.relative_to(ROOT)),
                                             "line": begin + 1}
                position += 1
    return blocks, errors


def read_examples(blocks):
    examples = {}
    errors = []
    for path in sorted(INDEX.rglob("*.json")):
        try:
            document = json.loads(path.read_text())
            for example in document["examples"]:
                name = example["id"]
                if name in examples:
                    errors.append(str(path.relative_to(ROOT)) + ": duplicate metadata " + name)
                    continue
                examples[name] = example
                if name not in blocks:
                    errors.append(name + ": metadata has no Markdown Cypher block")
                elif example["path"] != blocks[name]["path"]:
                    errors.append(name + ": metadata path differs from Markdown path")
                if "error" not in example and not any(
                    key in example for key in ("rows", "row_count")
                ):
                    errors.append(name + ": missing rows, row_count or error assertion")
                if "rows" in example and "row_count" in example:
                    errors.append(name + ": choose rows or row_count, not both")
                if "error" in example and not re.fullmatch(
                    r"(?:LITHOGRAPH_)?[A-Z][A-Z0-9_]*", example["error"]
                ):
                    errors.append(name + ": invalid error code")
                if example.get("fixture") not in (None, "csv"):
                    errors.append(name + ": unknown fixture")
        except (KeyError, TypeError, ValueError) as error:
            errors.append(str(path.relative_to(ROOT)) + ": invalid metadata: " + str(error))
    for name in sorted(set(blocks) - set(examples)):
        errors.append(name + ": Markdown example has no metadata")
    for name, example in examples.items():
        for setup in example.get("setup", []):
            if setup not in examples:
                errors.append(name + ": unknown setup ID " + setup)
    return examples, errors


def ordered_examples(name, examples, visiting=None, complete=None):
    visiting = set() if visiting is None else visiting
    complete = set() if complete is None else complete
    if name in visiting:
        raise RuntimeError("Setup dependency cycle at " + name)
    if name in complete:
        return []
    visiting.add(name)
    result = []
    for dependency in examples[name].get("setup", []):
        result.extend(ordered_examples(dependency, examples, visiting, complete))
    visiting.remove(name)
    complete.add(name)
    return result + [name]


def equal(actual, expected):
    if isinstance(actual, bool) or isinstance(expected, bool):
        return type(actual) is type(expected) and actual == expected
    if type(actual) is not type(expected):
        return False
    if isinstance(actual, float):
        return actual == expected or math.isclose(actual, expected, rel_tol=1e-9, abs_tol=1e-12)
    if isinstance(actual, list):
        return len(actual) == len(expected) and all(map(lambda pair: equal(*pair), zip(actual, expected)))
    if isinstance(actual, dict):
        return actual.keys() == expected.keys() and all(equal(actual[key], expected[key]) for key in actual)
    return actual == expected


def substitute(value, directory):
    if isinstance(value, dict):
        return {key: substitute(item, directory) for key, item in value.items()}
    if isinstance(value, list):
        return [substitute(item, directory) for item in value]
    if isinstance(value, str):
        source = Path(directory) / "people.csv"
        if value.startswith("fixture:"):
            name = value[8:]
            if not re.fullmatch(r"[a-zA-Z0-9_-]+\.csv", name):
                raise RuntimeError("Fixture must name a synthetic CSV basename")
            target = Path(directory) / name
            if not target.is_file():
                raise RuntimeError("Missing synthetic CSV fixture: " + name)
            return target.as_uri()
        return value.replace("{{csv_uri}}", source.as_uri()).replace("{{csv_path}}", str(source))
    return value


def create_csv_fixtures(directory):
    path = ROOT / "docs/cypher/load-csv.md"
    fixtures = re.findall(r"<!-- fixture: ([a-zA-Z0-9_-]+\.csv) -->\n```csv\n(.*?)\n```",
                          path.read_text(), flags=re.S)
    if not fixtures:
        raise RuntimeError("CSV fixture content has no Markdown fixture marker")
    for name, content in fixtures:
        (Path(directory) / name).write_text(content + "\n")


def matches_error(error, expected):
    code = expected.removeprefix("LITHOGRAPH_")
    actual = re.search(r"\bLITHOGRAPH_([A-Z][A-Z0-9_]*)\s*:", str(error))
    return bool(expected and actual and actual[1] == code)


def self_check():
    if equal(9007199254740993, 9007199254740992) or equal(1, 1.0) or equal(True, 1):
        raise RuntimeError("Result comparison lost numeric type or integer precision")
    if not equal(1.0, 1.0 + 1e-12) or equal(1.0, 1.1):
        raise RuntimeError("Float tolerance is incorrect")
    if not matches_error("LITHOGRAPH_TYPE_ERROR: invalid", "TYPE_ERROR") or matches_error(
        "LITHOGRAPH_TYPE_ERROR_OTHER: invalid", "TYPE_ERROR"
    ):
        raise RuntimeError("Error assertions do not match the complete error code")
    if matches_error("LITHOGRAPH_SEMANTIC_ERROR: undefined variable LITHOGRAPH_TYPE_ERROR",
                     "TYPE_ERROR") or not matches_error(
        "Runtime error near line 7: LITHOGRAPH_TYPE_ERROR: invalid", "TYPE_ERROR"
    ):
        raise RuntimeError("Error assertions matched message content instead of the actual error category")
    fixtures = {"a": {}, "b": {"setup": ["a"]}, "c": {"setup": ["a", "b"]}}
    if ordered_examples("c", fixtures) != ["a", "b", "c"]:
        raise RuntimeError("Setup dependency ordering/deduplication failed")
    try:
        ordered_examples("a", {"a": {"setup": ["b"]}, "b": {"setup": ["a"]}})
    except RuntimeError:
        pass
    else:
        raise RuntimeError("Setup cycles are not rejected")
    print("PASS: runner integer precision, numeric types, float tolerance, exact error codes and setup DAG guards")


def assert_example(graph, name, example, block, directory):
    try:
        result = graph.query(block["query"],
                             substitute(example.get("params", {}), directory),
                             substitute(example.get("options", {}), directory))
    except Exception as error:
        expected = example.get("error", "")
        if matches_error(error, expected):
            return
        raise RuntimeError(name + ": " + str(error)) from error
    if "error" in example:
        raise RuntimeError(name + ": expected " + example["error"] + ", query succeeded")
    for key in ("columns", "rows"):
        if key in example and not equal(result[key], example[key]):
            raise RuntimeError(name + ": " + key + " expected " +
                               json.dumps(example[key], ensure_ascii=False) + " got " +
                               json.dumps(result[key], ensure_ascii=False))
    if "row_count" in example and len(result["rows"]) != example["row_count"]:
        raise RuntimeError(name + ": expected " + str(example["row_count"]) +
                           " rows, got " + str(len(result["rows"])))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extension", type=Path, nargs="?")
    parser.add_argument("--sqlite3", type=Path)
    parser.add_argument("--provider", type=Path, help="Synthetic Provider fixture")
    parser.add_argument("--check", action="store_true", help="Only check registration and dependencies")
    parser.add_argument("--only", help="Run matching example IDs (regular expression)")
    parser.add_argument("--surface", choices=("scalar", "rows", "both"), default="both",
                        help="Default: both public SQLite execution surfaces, each in a fresh database")
    parser.add_argument("--self-test", action="store_true", help="Check the verifier's assertion and setup guards")
    args = parser.parse_args()
    if args.self_test:
        self_check()
        return
    blocks, errors = read_blocks()
    examples, registration_errors = read_examples(blocks)
    errors.extend(registration_errors)
    for name in examples:
        try:
            ordered_examples(name, examples)
        except (KeyError, RuntimeError) as error:
            errors.append(str(error))
    if errors:
        raise SystemExit("\n".join(errors))
    if not examples:
        raise SystemExit("No registered Cypher examples")
    if args.check:
        print("PASS: registered", len(examples), "Cypher examples; all fences and dependencies accounted for")
        return
    if not args.extension:
        parser.error("extension is required unless --check is used")
    selected = [name for name in sorted(examples) if not args.only or re.search(args.only, name)]
    if not selected:
        raise SystemExit("No examples match --only")
    failures = []
    surfaces = ("scalar", "rows") if args.surface == "both" else (args.surface,)
    for name in selected:
        for surface in surfaces:
            try:
                order = ordered_examples(name, examples)
                provider = any(examples[item].get("provider") for item in order)
                if provider and not args.provider:
                    raise RuntimeError(name + ": requires --provider (not skipped)")
                with tempfile.TemporaryDirectory(prefix="lithograph-cypher-docs-") as directory:
                    if any(examples[item].get("fixture") == "csv" for item in order):
                        create_csv_fixtures(directory)
                    with DocGraph(args.extension, directory, args.sqlite3,
                                  args.provider if provider else None, surface) as graph:
                        graph.check_version()
                        for item in order:
                            assert_example(graph, item, examples[item], blocks[item], directory)
            except Exception as error:
                failures.append(name + " [" + surface + "] (" + blocks[name]["path"] + "): " + str(error))
    if failures:
        raise SystemExit("\n".join(failures) + "\nFAILED: " + str(len(failures)) + "/" +
                         str(len(selected) * len(surfaces)))
    print("PASS:", len(selected), "Cypher examples x", len(surfaces), "surfaces;",
          "columns/results/errors and stream event order/terminal summary checked")


if __name__ == "__main__":
    main()
