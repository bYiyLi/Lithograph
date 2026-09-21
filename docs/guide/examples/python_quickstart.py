#!/usr/bin/env python3
"""Run the current SQL-only Lithograph quickstart in an isolated database."""

import argparse
import json
from pathlib import Path
import sqlite3
from typing import Any, Dict, Optional


def open_graph(extension: Path, database: str = ":memory:",
               initialize: bool = True) -> sqlite3.Connection:
    """Load a trusted extension; initialization is an explicit caller choice."""
    extension = extension.expanduser().resolve(strict=True)
    if sqlite3.sqlite_version_info < (3, 45, 0):
        raise RuntimeError("Lithograph requires SQLite 3.45.0 or newer")
    if not hasattr(sqlite3.Connection, "enable_load_extension"):
        raise RuntimeError("This Python SQLite build disables extension loading")
    db = sqlite3.connect(database, isolation_level=None, timeout=5.0)
    try:
        db.enable_load_extension(True)
        try:
            db.load_extension(str(extension))
        finally:
            db.enable_load_extension(False)
        version = json.loads(db.execute("SELECT lithograph_version()").fetchone()[0])
        if version.get("cypherProfile") != "CY25-2026.08":
            raise RuntimeError("Unexpected Lithograph compatibility profile")
        if initialize:
            db.execute("SELECT lithograph_init()").fetchone()
        return db
    except Exception:
        db.close()
        raise


def query(db: sqlite3.Connection, cypher: str,
          params: Optional[Dict[str, Any]] = None,
          options: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
    """Bind SQL inputs; preserve Lithograph tagged JSON values unchanged."""
    payload = (
        cypher,
        json.dumps({} if params is None else params, allow_nan=False),
        json.dumps({} if options is None else options, allow_nan=False),
    )
    cursor = db.execute("SELECT lithograph(?, ?, ?)", payload)
    try:
        return json.loads(cursor.fetchone()[0])
    finally:
        cursor.close()


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extension", type=Path, help="Trusted Lithograph shared library")
    args = parser.parse_args()
    db = open_graph(args.extension)
    try:
        result = query(db,
            "CREATE (p:Person {name:$person}), (c:Company {name:$company}), "
            "(p)-[:WORKS_AT {since:2026}]->(c) "
            "RETURN p.name AS person, c.name AS company",
            {"person": "Alice", "company": "Acme"},
            {"message": "Create the first graph"})
        require(result["rows"] == [["Alice", "Acme"]], "Unexpected create result")
        baseline = result["summary"]["commit"]
        query(db, "CALL lithograph.tag.create('before-rename', $target)",
              {"target": baseline})
        changed = query(db,
            "MATCH (p:Person) SET p.name=$name RETURN p.name AS name",
            {"name": "Alicia"})
        historical = query(db, "MATCH (p:Person) RETURN p.name AS name",
                           options={"at": "tag/before-rename"})
        require(changed["rows"] == [["Alicia"]], "Current state mismatch")
        require(historical["rows"] == [["Alice"]], "Historical state changed")
        require(changed["summary"]["commit"] != baseline, "Missing new Commit")
        cursor = db.execute(
            "SELECT ordinal, event, data FROM lithograph_rows(?, ?, ?)",
            ("MATCH (p:Person) RETURN p.name AS name", "{}", "{}"))
        try:
            for ordinal, event, data in cursor:
                print(json.dumps({"ordinal": ordinal, "event": event,
                                  "data": json.loads(data)}))
        finally:
            cursor.close()
        check = json.loads(db.execute("SELECT lithograph_integrity_check()").fetchone()[0])
        require(check["ok"], "Integrity check failed")
        print("PASS: create, parameter binding, streaming, tag, update, history, integrity")
    finally:
        db.close()


if __name__ == "__main__":
    main()
