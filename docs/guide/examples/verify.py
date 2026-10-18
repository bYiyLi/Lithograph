#!/usr/bin/env python3
"""Execute documentation SQL blocks and verify public adapter boundaries."""

import argparse
import json
from pathlib import Path
import re
import sqlite3
import subprocess
import tempfile

from python_quickstart import open_graph, query, require
from version_workflow import expect_error, head
from doc_runtime import DocGraph, python_sqlite_supported
from access_controls import verify_access_controls


GUIDE = Path(__file__).resolve().parent.parent
SQL_PAGES = (
    "installation.md", "getting-started.md", "graph-and-cypher.md",
    "schema-and-indexes.md", "search.md", "versioning.md",
    "transactions.md", "graph-views.md", "operations.md",
    "import.md", "safe-execution.md", "access-controls.md",
)
SQL_REFERENCE_PAGES = ("sql-api.md", "values.md")


def sql_blocks(path):
    return re.findall(r"^```sql\n(.*?)^```\s*$", path.read_text(), re.M | re.S)


def statements(block):
    pending = ""
    for line in block.splitlines(keepends=True):
        pending += line
        if sqlite3.complete_statement(pending):
            yield pending
            pending = ""
    require(not pending.strip(), "Incomplete SQL block: " + pending)

def verify_guides(extension, sqlite_cli):
    python_enabled = python_sqlite_supported()
    require(
        python_enabled or sqlite_cli is not None,
        "Python SQLite is older than 3.45.0; pass --sqlite3 with a supported CLI",
    )
    count = 0
    pages = [(GUIDE / name, "guide/" + name) for name in SQL_PAGES]
    pages += [(GUIDE.parent / "reference" / name, "reference/" + name)
              for name in SQL_REFERENCE_PAGES]
    for path, name in pages:
        blocks = sql_blocks(path)
        require(bool(blocks), "Missing SQL examples: " + name)
        require(not any("db.index.semantic." in block for block in blocks),
                name + ": real-service SQL must be an explicitly labelled text template; "
                "register synthetic Provider examples in the Cypher manifest")
        executable_blocks = blocks
        page_statements = [
            statement for block in executable_blocks for statement in statements(block)
        ]
        count += len(page_statements)
        if python_enabled:
            db = open_graph(extension, initialize=False)
            try:
                for statement in page_statements:
                    db.execute(statement).fetchall()
                if name == "guide/getting-started.md":
                    require(query(db, "MATCH (p:Person) RETURN p.name")["rows"] == [["Alicia"]],
                            "Tutorial current state")
                    require(query(db, "MATCH (p:Person) RETURN p.name",
                        options={"at": "tag/before-rename"})["rows"] == [["Alice"]],
                        "Tutorial historical state")
                if name == "guide/search.md":
                    result = query(db, "MATCH (d:Doc) SEARCH d IN (VECTOR INDEX doc_embedding "
                        "FOR vector([1.0,0.0],2,FLOAT64) LIMIT 1) RETURN d.id")
                    require(result["rows"] == [["graph"]], "Vector nearest result")
            except Exception as error:
                raise RuntimeError(name + ": " + str(error)) from error
            finally:
                db.close()
        if sqlite_cli:
            with tempfile.TemporaryDirectory(prefix="lithograph-sql-docs-") as directory:
                script = ".bail on\n.load \"" + str(extension.resolve()) + "\"\n"
                script += "\n".join(executable_blocks)
                result = subprocess.run([str(sqlite_cli), "-batch", ":memory:"],
                    input=script, text=True, capture_output=True, cwd=directory, timeout=120)
                require(result.returncode == 0, name + ": " + result.stderr)
        print(
            "PASS SQL:",
            name,
            len(executable_blocks),
            "blocks",
        )
    print("SQL statements verified:", count)


def verify_boundaries(extension):
    db = open_graph(extension)
    try:
        baseline = head(db)
        query(db, "CALL lithograph.branch.create('checkout-probe')")
        query(db, "CALL lithograph.branch.checkout('checkout-probe')")
        query(db, "CALL lithograph.branch.checkout('main')")
        db.execute(
            "SELECT event FROM lithograph_rows("
            "'CREATE (:EarlyBlocked) RETURN 1 AS x') LIMIT 1"
        ).fetchall()
        require(query(db, "MATCH (n:EarlyBlocked) RETURN count(n)")["rows"] == [[0]],
                "Outer LIMIT 1 unexpectedly completed a rows mutation")
        expect_error(lambda: query(db, "CREATE (:Blocked)", options={"at": baseline}),
                     "READ_ONLY_SNAPSHOT")
        expect_error(lambda: query(db, "RETURN 1", options={"timeout": 10}), "INVALID_ARGUMENT")
        expect_error(lambda: query(db, "CREATE (:Blocked)",
            options={"graphView": {"requireAllLabels": ["Tenant"]}}), "GRAPH_VIEW_VIOLATION")
        require(head(db) == baseline, "Rejected query changed Branch head")
        batched = query(db,
            "UNWIND [1,2] AS n CALL (n) { RETURN n AS value } "
            "IN TRANSACTIONS OF 1 ROWS RETURN value ORDER BY value")
        require(batched["rows"] == [[1], [2]], "Transaction subquery SQL execution mismatch")
        big = {"$type": "Integer", "value": "9223372036854775807"}
        escaped = {"$type": "Map", "entries": {"$type": "application", "key": "v"}}
        values = query(db, "RETURN $big AS big,$map AS map", {"big": big, "map": escaped})
        require(values["rows"] == [[big, escaped]], "Typed JSON did not round-trip")
        structural = {"$type": "Node", "elementId": "n:1", "labels": [], "properties": {}}
        expect_error(lambda: query(db, "RETURN $node", {"node": structural}), "INVALID_ARGUMENT")
        catalog = query(db,
            "SHOW PROCEDURES YIELD name,argumentDescription,returnDescription "
            "WHERE name='lithograph.branch.list' "
            "RETURN argumentDescription,returnDescription")
        described = {field["name"]: field["type"] for field in catalog["rows"][0][1]}
        actual = query(db, "CALL lithograph.branch.list()")
        active = actual["rows"][0][actual["columns"].index("active")]
        require(isinstance(active, bool), "Branch active field is not Boolean")
        if described["active"] != "BOOLEAN":
            print("NOTICE: returnDescription active type is", described["active"],
                  "while the returned value is Boolean; use the Procedure Reference")
        db.execute("BEGIN")
        query(db, "CREATE (:Rollback) FINISH")
        query(db, "CREATE (:Rollback) FINISH")
        db.execute("ROLLBACK")
        require(head(db) == baseline, "Outer SQL rollback kept graph Commits")
        with tempfile.TemporaryDirectory(prefix="lithograph-csv-docs-") as directory:
            source = Path(directory) / "people.csv"
            source.write_text("id,name\nalice,Alice\nbob,Bob\n")
            result = query(db, "LOAD CSV WITH HEADERS FROM $source AS row "
                "MERGE (p:ImportedPerson {id:row.id}) SET p.name=row.name "
                "RETURN count(p) AS imported", {"source": source.as_uri()})
            require(result["rows"] == [[2]], "CSV import mismatch")
        require(json.loads(db.execute("SELECT lithograph_integrity_check()").fetchone()[0])["ok"],
                "Integrity failure after boundary tests")
        print("PASS: rejected writes/options/structural params, typed JSON, rollback, "
              "LOAD CSV, checkout/introspection, integrity")
    finally:
        db.close()


def export_inventory(extension, directory, sqlite_cli=None):
    directory.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="lithograph-export-docs-") as fixture:
        with DocGraph(extension, fixture, sqlite_cli) as graph:
            version = graph.check_version()
            # databaseId is fixture-specific and is not part of the exported version baseline.
            version.pop("databaseId", None)
            (directory / "version.json").write_text(
                json.dumps(version, ensure_ascii=False, indent=2) + "\n")
            for name in ("FUNCTIONS", "PROCEDURES"):
                result = graph.query("SHOW " + name + " YIELD *")
                (directory / (name.lower() + ".json")).write_text(
                    json.dumps(result, ensure_ascii=False, indent=2) + "\n")
                print(name, len(result["rows"]), "rows; columns:", result["columns"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extension", type=Path)
    parser.add_argument("--sqlite3", type=Path)
    parser.add_argument("--export-inventory", type=Path)
    parser.add_argument("--inventory-only", action="store_true")
    args = parser.parse_args()
    if not args.inventory_only:
        require(python_sqlite_supported(),
                "Required Python boundary probes need Python SQLite 3.45.0+ with "
                "enable_load_extension; --sqlite3 adds CLI regression coverage and "
                "does not replace the Python probes")
    with tempfile.TemporaryDirectory(prefix="lithograph-doc-version-") as fixture:
        with DocGraph(args.extension, fixture, args.sqlite3) as graph:
            graph.check_version()
    if args.export_inventory:
        export_inventory(args.extension, args.export_inventory, args.sqlite3)
    if not args.inventory_only:
        verify_guides(args.extension, args.sqlite3)
        verify_boundaries(args.extension)
        verify_access_controls(args.extension)


if __name__ == "__main__":
    main()
