#!/usr/bin/env python3
"""Manage one multi-execution Lithograph Commit through the current SQL surface."""

import json
from pathlib import Path
import sqlite3
import sys
import tempfile

def require(condition, message):
    if not condition:
        raise RuntimeError(message)

def scalar(connection, sql, parameters=()):
    row = connection.execute(sql, parameters).fetchone()
    require(row is not None and row[0] is not None, "SQL scalar returned no result")
    return json.loads(row[0])

def execute(connection, query):
    return scalar(connection, "SELECT lithograph(?)", (query,))

def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: sql_transaction.py /absolute/path/lithograph.extension")
    if sqlite3.sqlite_version_info < (3, 45, 0):
        raise RuntimeError("Lithograph requires SQLite 3.45.0 or newer")
    if not hasattr(sqlite3.Connection, "enable_load_extension"):
        raise RuntimeError("This Python SQLite build disables extension loading")
    extension = Path(sys.argv[1]).resolve()
    require(extension.is_file(), "extension does not exist")

    with tempfile.TemporaryDirectory(prefix="lithograph-sql-tx-") as directory:
        db = sqlite3.connect(Path(directory) / "graph.db", isolation_level=None)
        db.enable_load_extension(True)
        db.load_extension(str(extension))
        scalar(db, "SELECT lithograph_init()")

        begin = scalar(
            db,
            "SELECT lithograph_tx_begin(?)",
            (json.dumps({"author": "python", "message": "create two people"}),),
        )
        execute(db, "CREATE (:Person {name:'张三'}) FINISH")
        execute(db, "CREATE (:Person {name:'李四'}) FINISH")
        staged = execute(db, "MATCH (p:Person) RETURN p.name ORDER BY p.name")
        require(staged["rows"] == [["张三"], ["李四"]], "staged read mismatch")
        require(staged["summary"]["commit"] is None, "staged query exposed a Commit")
        committed = scalar(db, "SELECT lithograph_tx_commit()")
        require(committed["commit"] != begin["baseCommit"], "write did not create a Commit")

        scalar(db, "SELECT lithograph_tx_begin('{}')")
        execute(db, "CREATE (:Discarded) FINISH")
        require(scalar(db, "SELECT lithograph_tx_abort()") == {"aborted": True}, "abort mismatch")
        require(execute(db, "MATCH (n:Discarded) RETURN count(n)")["rows"] == [[0]], "aborted data remained visible")
        db.close()

    print("PASS: current SQL explicit transaction surface")

if __name__ == "__main__":
    main()
