#!/usr/bin/env python3
"""Manage one multi-execution Lithograph Commit with Python's SQLite driver."""

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


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: sql_transaction.py /absolute/path/lithograph.extension")
    extension = Path(sys.argv[1]).resolve()
    require(extension.is_file(), "extension does not exist")

    with tempfile.TemporaryDirectory(prefix="lithograph-sql-tx-") as directory:
        database = Path(directory) / "graph.db"
        db = sqlite3.connect(database, isolation_level=None)
        db.enable_load_extension(True)
        db.load_extension(str(extension))
        scalar(db, "SELECT lithograph_init()")

        begin = scalar(
            db,
            "SELECT lithograph_tx_begin(?)",
            (json.dumps({"author": "python", "message": "create two people"}),),
        )
        require(begin["baseCommit"].startswith("commit/"), "missing base Commit")
        scalar(
            db,
            "SELECT lithograph_tx_execute(?)",
            ("CREATE (:Person {name: '张三'}) FINISH",),
        )
        scalar(
            db,
            "SELECT lithograph_tx_execute(?)",
            ("CREATE (:Person {name: '李四'}) FINISH",),
        )
        staged = scalar(
            db,
            "SELECT lithograph_tx_execute(?)",
            ("MATCH (p:Person) RETURN p.name ORDER BY p.name",),
        )
        require(staged["rows"] == [["张三"], ["李四"]], "staged read mismatch")
        require(staged["summary"]["commit"] is None, "staged query exposed a Commit")
        committed = scalar(db, "SELECT lithograph_tx_commit()")
        require(committed["commit"] != begin["baseCommit"], "write did not create a Commit")

        scalar(db, "SELECT lithograph_tx_begin('{}')")
        scalar(
            db,
            "SELECT lithograph_tx_execute(?)",
            ("CREATE (:Discarded) FINISH",),
        )
        require(
            scalar(db, "SELECT lithograph_tx_abort()") == {"aborted": True},
            "abort acknowledgement mismatch",
        )
        discarded = scalar(
            db, "SELECT lithograph('MATCH (n:Discarded) RETURN count(n)')"
        )
        require(discarded["rows"] == [[0]], "aborted data remained visible")
        db.close()

    print("PASS: SQL explicit transaction committed two writes once and aborted cleanly")


if __name__ == "__main__":
    main()
