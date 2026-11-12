#!/usr/bin/env python3
"""S22.1 real SQL statement reuse, staged state, errors and cursor cleanup."""

import json
from pathlib import Path
import sqlite3
import sys

from phase21_predicate_routing_sql import execute, require, rows


def plan(db, text, params=None, options=None):
    return execute(db, "EXPLAIN " + text, params, options)["rows"][0][0]


def error(db, text, params=None, options=None, expected="LITHOGRAPH_TYPE_ERROR"):
    try:
        execute(db, text, params, options)
    except sqlite3.Error as exc:
        require(expected in str(exc), f"wrong error: {exc}")
    else:
        raise AssertionError(f"missing {expected}: {text}")


def run(extension):
    db = sqlite3.connect(":memory:", isolation_level=None, cached_statements=64)
    db.enable_load_extension(True)
    db.execute("SELECT load_extension(?,?)", (str(Path(extension).resolve()), "sqlite3_lithograph_init"))
    db.enable_load_extension(False)
    db.execute("SELECT lithograph_init()").fetchone()
    execute(db, "UNWIND range(0,255) AS i CREATE (:Common {v:i}) FINISH")
    execute(db, "MATCH (n:Common) WHERE n.v IN [0,1] SET n:Rare FINISH")
    require("IndexSeek" not in plan(db, "MATCH (n:Common) WHERE n.v=$x RETURN n.v", {"x": 1}), "unindexed plan")
    require(rows(db, "MATCH (n:Common) WHERE n.v=$x RETURN n.v", {"x": 1}) == [[1]], "unindexed result")
    execute(db, "CREATE CONSTRAINT value_type FOR (n:Common) REQUIRE n.v IS :: INTEGER")
    base = execute(db, "CREATE RANGE INDEX values_range FOR (n:Common) ON (n.v)")["summary"]["commit"]
    narrow = {"graphView": {"requireAllLabels": ["Rare"]}}
    text = "MATCH (n:Common) WHERE n.v=$x RETURN n.v"
    checks = 2
    for options, expected in (({}, "IndexSeek"), (narrow, "LabelIndexScan")):
        require(expected in plan(db, text, {"x": 1}, options), "wrong costed access")
        for prefix in ("", "WITH 1 AS seed "):
            query = prefix + text
            expected_rows = [[1]]
            require(execute(db, query, {"x": 1}, options)["rows"] == expected_rows, "scalar/program result")
            require(rows(db, query, {"x": 1}, options) == expected_rows, "rows/program result")
            profiled = execute(db, "PROFILE " + query, {"x": 1}, options)
            require(profiled["rows"] == expected_rows, "PROFILE result drift")
            require(expected in plan(db, query, {"x": 1}, options), "program EXPLAIN drift")
            operators = profiled["summary"]["profile"]["operators"]
            require(any(op["operator"] == expected and op["dbHits"] > 0 for op in operators), "PROFILE access drift")
            checks += 1
        # Identical SQL is reused by sqlite3's actual prepared statement cache.
        for value, expected_rows in ((1, [[1]]), (None, []), (9999, []), (0, [[0]]), (2, [] if options else [[2]])):
            require(execute(db, text, {"x": value}, options)["rows"] == expected_rows, "reused statement binding")
            checks += 1
        error(db, text, {"x": "string"}, options)
        checks += 1
    for values, operator in (([1], "IndexSeek"), (list(range(64)), "LabelIndexScan"), ([1], "IndexSeek")):
        query = "MATCH (n:Common) WHERE n.v IN $x RETURN n.v ORDER BY n.v"
        require(operator in plan(db, query, {"x": values}), "IN binding did not replan")
        require(rows(db, query, {"x": values}) == [[v] for v in values], "IN bag/ordering")
        checks += 1
    execute(db, "MATCH (n:Common) WHERE n.v=2 SET n:Other FINISH")
    union_view = {"graphView": {"requireAnyLabels": ["Rare", "Other"]}}
    require(rows(db, "MATCH (n:Common) WHERE n.v IN $x RETURN n.v ORDER BY n.v", {"x": [0, 1, 2]}, union_view) == [[0], [1], [2]], "OR Graph View narrowed to one member")
    checks += 1
    hidden = {"readAccess": {"query": {"denyProperties": ["v"]}}}
    require("IndexSeek" not in plan(db, text, {"x": 1}, hidden), "hidden property used for access")
    require(rows(db, text, {"x": 1}, hidden) == [], "hidden property leaked")
    require(rows(db, text, {"x": 1}, {"readAccess": {"result": {"denyProperties": ["v"]}}}) == [[1]], "result policy became query policy")
    checks += 3
    for options in ({}, narrow):
        write = "MATCH (n:Common) WHERE n.v=$x SET n.hit=1 RETURN n.v"
        require(rows(db, write, {"x": 1}, options) == [[1]], "write cursor result")
        program_write = "WITH 1 AS seed MATCH (n:Common) WHERE n.v=$x SET n.hit=seed RETURN n.v"
        require(rows(db, program_write, {"x": 1}, options) == [[1]], "program write result")
        error(db, "WITH 1 AS seed MATCH (n:Common) WHERE n.v=$x SET n.hit=2 RETURN 1/0", {"x": 1}, options)
        require(rows(db, "MATCH (n:Common) WHERE n.v=1 RETURN n.hit") == [[1]], "program write did not rollback")
        error(db, "MATCH (n:Common) WHERE n.v=$x SET n.hit=2 RETURN 1/0", {"x": 1}, options)
        require(rows(db, "MATCH (n:Common) WHERE n.v=1 RETURN n.hit") == [[1]], "write did not rollback")
        checks += 4
        # Stop the real SQL virtual-table cursor before its success summary.
        cursor = db.execute("SELECT event,data FROM lithograph_rows(?,?,?)", (
            "MATCH (n:Common) WHERE n.v IN [0,1] SET n.unpublished=9 RETURN n.v", '{}', json.dumps(options)))
        while True:
            event = cursor.fetchone()
            require(event is not None, "write stream ended before a row")
            if event[0] == "row":
                break
        cursor.close()
        require(rows(db, "MATCH (n:Common) WHERE n.v IN [0,1] RETURN n.unpublished") == [[None], [None]], "early-close published a write")
        checks += 1
    # Each staged invocation revalidates schema, visibility and provisional IDs.
    db.execute("SELECT lithograph_tx_begin('{}')").fetchone()
    created = execute(db, "CREATE (n:Common:Rare {v:900}) RETURN elementId(n)")["rows"][0][0]
    require(rows(db, text, {"x": 900}, narrow) == [[900]], "staged creation invisible")
    require(rows(db, "MATCH (n:Common) WHERE elementId(n)=$id DELETE n", {"id": created}) == [], "staged delete")
    require(rows(db, text, {"x": 900}, narrow) == [], "staged deletion reused plan")
    db.execute("SELECT lithograph_tx_abort()").fetchone()
    require(rows(db, text, {"x": 900}) == [], "aborted state survived")
    checks += 4
    # Relationship ID plans must bind against each fresh transaction-local
    # revision, not a saved current-head lookup or the previous SQL statement.
    link = "MATCH (a:LinkStage)-[r:STAGED]->(b:LinkStage) WHERE elementId(r)=$id RETURN a.v,b.v"
    original_head = db.execute(
        "SELECT commit_id FROM main._lithograph_branches WHERE name='main'"
    ).fetchone()[0]
    db.execute("SELECT lithograph_tx_begin('{}')").fetchone()
    relationship_id = execute(
        db, "CREATE (a:LinkStage {v:71})-[r:STAGED]->(b:LinkStage {v:72}) RETURN elementId(r)"
    )["rows"][0][0]
    require("RelationshipIdSeek" in plan(db, link, {"id": relationship_id}),
            "staged Relationship ID was not planned as a seek")
    require(rows(db, link, {"id": relationship_id}) == [[71, 72]],
            "staged Relationship identity seek was invisible")
    require(execute(db, "PROFILE " + link, {"id": relationship_id})["rows"] == [[71, 72]],
            "staged Relationship PROFILE disagreed")
    require(rows(db, link, {"id": "r:99999999"}) == [],
            "missing Relationship ID reused a previous prepared lookup")
    require(rows(db, link, {"id": None}) == [], "null Relationship ID fallback")
    execute(db, "MATCH ()-[r:STAGED]->() WHERE elementId(r)=$id DELETE r FINISH",
            {"id": relationship_id})
    require(rows(db, link, {"id": relationship_id}) == [],
            "staged Relationship delete used stale seek")
    db.execute("SELECT lithograph_tx_abort()").fetchone()
    require(rows(db, link, {"id": relationship_id}) == [],
            "aborted Relationship remained visible")
    require(db.execute(
        "SELECT commit_id FROM main._lithograph_branches WHERE name='main'"
    ).fetchone()[0] == original_head, "staged Relationship moved durable Branch head")
    checks += 8
    # A later clause must not use a base-Snapshot compiled membership proof.
    require(rows(db, "CREATE (:Common:Rare {v:901}) WITH 1 AS seed MATCH (n:Common) WHERE n.v=901 RETURN n.v", options=narrow) == [[901]], "staged clause membership")
    execute(db, "DROP CONSTRAINT value_type")
    execute(db, "CREATE (:Common {v:'mixed'})")
    require("IndexSeek" not in plan(db, text, {"x": 1}), "current schema supplied stale type proof")
    error(db, text, {"x": 1})
    require(rows(db, text, {"x": 1}, {"at": base}) == [[1]], "historical schema/type proof")
    execute(db, "DROP INDEX values_range")
    require("IndexSeek" in plan(db, text, {"x": 1}, {"at": base}), "historical IndexDefinition lost")
    require("IndexSeek" not in plan(db, text, {"x": 1}), "dropped index reused")
    checks += 6
    # Identity/OPTIONAL/zero-result fallbacks preserve equality errors and nulls.
    for value in (None, "bad", "n:0001", "n:999999"):
        require(rows(db, "MATCH (n) WHERE elementId(n)=$x RETURN n.v", {"x": value}) == [], "ID fallback")
    error(db, "MATCH (n) WHERE elementId(n)=$x RETURN n.v", {"x": 1})
    require(rows(db, "OPTIONAL MATCH (n:Missing) RETURN n.v") == [[None]], "optional null")
    error(db, "MATCH (n:Common) WHERE n.v=$x RETURN n.v", {"x": 1}, {"memoryLimitBytes": 64}, "LITHOGRAPH_RESOURCE_ERROR")
    require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "SQLite integrity")
    checks += 8
    db.close()
    print(json.dumps({"stage": "S22.1", "sql_checks": checks, "sqlite": sqlite3.sqlite_version}))


if __name__ == "__main__":
    run(sys.argv[1])
