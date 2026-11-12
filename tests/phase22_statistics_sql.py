#!/usr/bin/env python3
"""S22.3 statistics affect access cost, never visibility or version semantics.

The database is synthetic and owned by this process. Both public adapters use
the same cached SQLite statements while parameters, schema and staged state
change. No assertion depends on an unordered result's natural row order.
"""

import json
from pathlib import Path
import sqlite3
import sys
import tempfile

from phase21_predicate_routing_sql import execute, require, rows
from phase22_access_sql import error, plan
from phase22_access_work import file_hash, open_db

VIEW = {"graphView": {"requireAllLabels": ["Rare"]}}
QUERY = "MATCH (n:Common) WHERE n.v=$x RETURN n.k ORDER BY n.k"
HIDDEN = {"readAccess": {"query": {"denyProperties": ["v"]}}}


def with_view(options=None):
    return dict(VIEW, **(options or {}))


def head(db):
    return db.execute(
        "SELECT commit_id FROM main._lithograph_branches WHERE name='main'"
    ).fetchone()[0]


def check_both(db, text, expected, params=None, options=None):
    require(execute(db, text, params, options)["rows"] == expected, "scalar result")
    require(rows(db, text, params, options) == expected, "rows result")


def frequency_and_permissions(db):
    hot = [[value] for value in range(15)]
    for value, expected, operator in ((0, hot, "LabelIndexScan"),
                                      (56, [[56]], "IndexSeek"),
                                      (0, hot, "LabelIndexScan"),
                                      (56, [[56]], "IndexSeek")):
        explained = plan(db, QUERY, {"x": value}, VIEW)
        require(operator in explained, "frequency did not affect access")
        if operator == "LabelIndexScan":
            require('label: "Rare"' in explained, "hot value scanned the broader Common domain")
        check_both(db, QUERY, expected, {"x": value}, VIEW)
        profile = execute(db, "PROFILE " + QUERY, {"x": value}, VIEW)
        require(profile["rows"] == expected, "PROFILE result")
        require(profile["summary"]["metrics"]["dbHits"] <= 256,
                "selected access scanned unrelated owners")
        require(any(op["operator"] == operator and op["dbHits"] > 0
                    for op in profile["summary"]["profile"]["operators"]),
                "PROFILE did not execute its selected access")
    for value in (None, -1, 1000000):
        check_both(db, QUERY, [], {"x": value}, VIEW)
    error(db, QUERY, {"x": "different-family"}, VIEW)
    hidden = with_view(HIDDEN)
    hidden_plans = []
    for value in (0, 56, -1, "different-family"):
        explained = plan(db, QUERY, {"x": value}, hidden)
        require("IndexSeek" not in explained, "hidden property used for an index or MCV")
        check_both(db, QUERY, [], {"x": value}, hidden)
        require(execute(db, "PROFILE " + QUERY, {"x": value}, hidden)["rows"] == [],
                "hidden property PROFILE leaked a value")
        hidden_plans.append(explained)
    require(len(set(hidden_plans)) == 1, "hidden value changed plan through statistics")
    check_both(db, "MATCH (n:Common) WHERE n.v IS NULL RETURN n.k ORDER BY n.k",
               hot + [[56]], options=hidden)
    check_both(db, QUERY, [[56]], {"x": 56},
               with_view({"readAccess": {"result": {"denyProperties": ["v"]}}}))
    return hidden_plans[0]


def staged_revisions(db):
    original = head(db)
    db.execute("SELECT lithograph_tx_begin('{}')").fetchone()
    try:
        created = execute(db, "CREATE (n:Common:Rare {v:6000,k:6000}) RETURN elementId(n)")["rows"][0][0]
        check_both(db, QUERY, [[6000]], {"x": 6000}, VIEW)
        execute(db, "MATCH (n:Common) WHERE elementId(n)=$id SET n.v=6001 FINISH", {"id": created})
        check_both(db, QUERY, [], {"x": 6000}, VIEW)
        check_both(db, QUERY, [[6000]], {"x": 6001}, VIEW)
        execute(db, "MATCH (n:Common) WHERE elementId(n)=$id DELETE n FINISH", {"id": created})
        check_both(db, QUERY, [], {"x": 6001}, VIEW)
    finally:
        # A failed invocation already aborts its Lithograph transaction. Do not
        # replace that error with a second abort's INVALID_ARGUMENT.
        if db.in_transaction:
            db.execute("SELECT lithograph_tx_abort()").fetchone()
    require(head(db) == original, "aborted statistics revision moved Branch head")
    check_both(db, QUERY, [], {"x": 6001}, VIEW)
    require(rows(db, "CREATE (:Common:Rare {v:6002,k:6002}) WITH 1 AS seed "
                 "MATCH (n:Common) WHERE n.v=6002 RETURN n.k", options=VIEW) == [[6002]],
            "same-execution staged membership was not visible")
    execute(db, "MATCH (n:Common) WHERE n.v=6002 DELETE n FINISH", options=VIEW)


def cancellation(db):
    calls = 0

    def interrupt():
        nonlocal calls
        calls += 1
        if calls == 8:
            db.interrupt()
        return 0

    db.set_progress_handler(interrupt, 32)
    try:
        try:
            # Real graph/property work ensures host VM participates; no timer
            # races or graph-sized retained Python result are needed.
            for _ in db.execute("SELECT event,data FROM lithograph_rows(?)", (
                    "MATCH (n:Common) WHERE n.v>=0 RETURN n.k",)):
                pass
        except sqlite3.Error as exc:
            require(getattr(exc, "sqlite_errorcode", None) == sqlite3.SQLITE_INTERRUPT,
                    f"wrong interrupt error: {exc}")
        else:
            raise AssertionError("actual sqlite3_interrupt did not stop query")
    finally:
        db.set_progress_handler(None, 0)
    require(calls >= 8, "interrupt trigger was not reached")


def derived_advice(extension):
    db = open_db(extension, ":memory:")
    try:
        db.execute("SELECT lithograph_init()").fetchone()
        execute(db, "UNWIND range(0,31) AS i CREATE (:Common {k:i,v:i}) FINISH")
        execute(db, "MATCH (n:Common) WHERE n.v<16 SET n:Rare FINISH")
        execute(db, "CREATE CONSTRAINT value_type FOR (n:Common) REQUIRE n.v IS :: INTEGER")
        execute(db, "CREATE RANGE INDEX values_range FOR (n:Common) ON (n.v)")
        generation, owners, entries = db.execute(
            "SELECT generation_id,indexed_entities,entry_count "
            "FROM main._lithograph_index_generations ORDER BY generation_id DESC LIMIT 1"
        ).fetchone()
        original = head(db)

        def correct():
            require("IndexSeek" in plan(db, QUERY, {"x": 15}, VIEW), "advice fallback changed access")
            check_both(db, QUERY, [[15]], {"x": 15}, VIEW)
            require(execute(db, "PROFILE " + QUERY, {"x": 15}, VIEW)["rows"] == [[15]],
                    "derived advice changed PROFILE")

        correct()
        # Deliberately malformed *derived counts*, confined to this private
        # fixture. They are neither graph/schema writes nor correctness input.
        # The numeric CHECK alone accepts TEXT/BLOB/REAL on non-STRICT tables.
        for column, bad in (("indexed_entities", "broken"), ("entry_count", "broken"),
                            ("indexed_entities", b"not-an-integer"), ("entry_count", 1.5)):
            db.execute(f"UPDATE main._lithograph_index_generations SET {column}=? "
                       "WHERE generation_id=?", (bad, generation))
            try:
                correct()
            finally:
                db.execute("UPDATE main._lithograph_index_generations "
                           "SET indexed_entities=?,entry_count=? WHERE generation_id=?",
                           (owners, entries, generation))
        db.execute("PRAGMA ignore_check_constraints=ON")
        try:
            db.execute("UPDATE main._lithograph_index_generations SET indexed_entities=-1 "
                       "WHERE generation_id=?", (generation,))
            correct()
        finally:
            db.execute("UPDATE main._lithograph_index_generations "
                       "SET indexed_entities=? WHERE generation_id=?", (owners, generation))
            db.execute("PRAGMA ignore_check_constraints=OFF")
        # Missing ready content uses the existing bounded/canonical fallback.
        db.execute("DELETE FROM main._lithograph_index_entries WHERE generation_id=?", (generation,))
        db.execute("DELETE FROM main._lithograph_index_generations WHERE generation_id=?", (generation,))
        correct()
        require(head(db) == original, "derived statistics repair changed canonical Commit")
        require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "derived fixture integrity")
    finally:
        db.close()


def run(extension):
    extension = str(Path(extension).resolve())
    with tempfile.TemporaryDirectory(prefix="lithograph-s22-statistics-") as directory:
        path = Path(directory) / "synthetic.sqlite3"
        db = open_db(extension, str(path))
        db.execute("SELECT lithograph_init()").fetchone()
        execute(db, "UNWIND range(0,63) AS i CREATE "
                "(:Common {k:i,v:CASE WHEN i<48 THEN 0 ELSE i END}) FINISH")
        execute(db, "MATCH (n:Common) WHERE n.k<15 OR n.k=56 SET n:Rare FINISH")
        execute(db, "CREATE CONSTRAINT common_value_type FOR (n:Common) REQUIRE n.v IS :: INTEGER")
        index_commit = execute(db, "CREATE RANGE INDEX common_values FOR (n:Common) ON (n.v)")["summary"]["commit"]
        hidden_plan = frequency_and_permissions(db)
        staged_revisions(db)
        # Keep membership/counts unchanged while replacing the value frequency.
        execute(db, "MATCH (n:Common) SET n.v=1 FINISH")
        require(plan(db, QUERY, {"x": 0}, with_view(HIDDEN)) == hidden_plan,
                "hidden distribution changed EXPLAIN")
        check_both(db, QUERY, [], {"x": 56}, VIEW)
        check_both(db, QUERY, [[56]], {"x": 56}, with_view({"at": index_commit}))
        execute(db, "DROP INDEX common_values")
        require("IndexSeek" not in plan(db, QUERY, {"x": 1}, VIEW), "dropped IndexDefinition reused")
        require("IndexSeek" in plan(db, QUERY, {"x": 56}, with_view({"at": index_commit})),
                "historical IndexDefinition was replaced by current schema")
        require(plan(db, QUERY, {"x": 0}, with_view(HIDDEN)) == hidden_plan,
                "hidden index availability changed EXPLAIN")
        # Removing the type proof must restore equality TYPE_ERROR. Historical
        # proof and generation continue to belong to the pinned old Commit.
        execute(db, "DROP CONSTRAINT common_value_type")
        execute(db, "CREATE (:Common:Rare {k:7000,v:'mixed'}) FINISH")
        error(db, QUERY, {"x": 1}, VIEW)
        check_both(db, QUERY, [], {"x": 1}, with_view(HIDDEN))
        check_both(db, QUERY, [[56]], {"x": 56}, with_view({"at": index_commit}))
        # Locate the synthetic mixed node without a cross-family comparison.
        execute(db, "MATCH (n:Common) WHERE n.k=7000 DELETE n FINISH")
        db.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchall()
        db.close()
        before = file_hash(path)
        db = open_db(extension, str(path), True)
        before_head = head(db)
        error(db, QUERY, {"x": 1}, {**VIEW, "memoryLimitBytes": 64}, "LITHOGRAPH_RESOURCE_ERROR")
        cancellation(db)
        expected = [[i] for i in range(15)] + [[56]]
        check_both(db, QUERY, expected, {"x": 1}, VIEW)
        require("LabelIndexScan" in plan(db, QUERY, {"x": 1}, VIEW), "readonly EXPLAIN")
        require(execute(db, "PROFILE " + QUERY, {"x": 1}, VIEW)["rows"] == expected,
                "readonly PROFILE result")
        require(head(db) == before_head, "read/EXPLAIN/PROFILE/cancel moved Branch head")
        require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "reopen integrity")
        db.close()
        require(file_hash(path) == before, "read-only query modified main")
    derived_advice(extension)
    print(json.dumps({"stage": "S22.3", "groups": ["frequency", "permissions", "staged",
          "historical", "schema", "resource", "interrupt", "readonly", "derived-advice"],
          "sqlite": sqlite3.sqlite_version}))


if __name__ == "__main__":
    run(sys.argv[1])
