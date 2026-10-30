#!/usr/bin/env python3
"""Real SQLite release-artifact regression for the Graph View Label union."""

from __future__ import annotations

import json
import sqlite3
import sys
import tomllib
from pathlib import Path


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def main(extension: str) -> None:
    require(sqlite3.sqlite_version_info >= (3, 45, 0), "SQLite 3.45+ required")
    root = Path(__file__).resolve().parent.parent
    expected_version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"][
        "version"
    ]
    with sqlite3.connect(":memory:") as db:
        db.enable_load_extension(True)
        try:
            db.load_extension(str(Path(extension).resolve()))
        finally:
            db.enable_load_extension(False)
        version = json.loads(db.execute("SELECT lithograph_version()").fetchone()[0])
        require(version["extension"] == expected_version, "loaded extension version mismatch")
        db.execute("SELECT lithograph_init()").fetchone()
        db.execute(
            "SELECT lithograph(?)",
            (
                "CREATE (a:Scope:A {name:'a'}), (b:Scope:B {name:'b'}), "
                "(ab:Scope:A:B {name:'ab'}), (hidden:Scope:B:Hidden {name:'hidden'}), "
                "(out:Other {name:'outside'}), (a)-[:LINK]->(b), "
                "(a)-[:LINK]->(out) FINISH",
            ),
        ).fetchone()

        select = "MATCH (n) RETURN n.name AS name ORDER BY name"
        opts = json.dumps(
            {
                "graphView": {
                    "requireAllLabels": ["Scope"],
                    "requireAnyLabels": ["A", "B", "Missing"],
                    "excludeAnyLabels": ["Hidden"],
                }
            }
        )
        args = (select, "{}", opts)
        scalar = json.loads(db.execute("SELECT lithograph(?,?,?)", args).fetchone()[0])
        expected = [["a"], ["ab"], ["b"]]
        require(scalar["rows"] == expected, "OR-only Node union was not deduplicated or filtered")
        events = db.execute(
            "SELECT event,data FROM lithograph_rows(?,?,?) ORDER BY ordinal", args
        ).fetchall()
        rows = [json.loads(data) for event, data in events if event == "row"]
        require(rows == expected, "rows surface differs from scalar Graph View union")
        require(events[-1][0] == "summary", "rows surface did not finish successfully")

        relationship = json.loads(
            db.execute(
                "SELECT lithograph(?,?,?)",
                ("MATCH ()-[r:LINK]->() RETURN count(r)", "{}", opts),
            ).fetchone()[0]
        )
        require(relationship["rows"] == [[1]], "hidden endpoint leaked a Relationship")
        db.execute("SELECT lithograph_tx_begin('{}')").fetchone()
        db.execute(
            "SELECT lithograph(?,?,?)",
            ("CREATE (:Scope:B {name:'staged'}) FINISH", "{}", opts),
        ).fetchone()
        staged = json.loads(db.execute("SELECT lithograph(?,?,?)", args).fetchone()[0])
        require(len(staged["rows"]) == 4, "transaction did not observe staged OR member")
        db.execute("SELECT lithograph_tx_abort()").fetchone()
        restored = json.loads(db.execute("SELECT lithograph(?,?,?)", args).fetchone()[0])
        require(restored["rows"] == expected, "aborted transaction leaked staged OR member")
    print(f"Phase 20 release SQL: passed (SQLite {sqlite3.sqlite_version}, v{expected_version})")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: phase20_release_sql.py <extension-path>")
    main(sys.argv[1])
