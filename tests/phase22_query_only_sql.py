#!/usr/bin/env python3
"""S22.3 TEMP-forbidden canonical access on both real SQLite adapters."""

import hashlib
import json
from pathlib import Path
import sqlite3
import sys
import tempfile

from phase21_predicate_routing_sql import execute, require, rows
from phase22_access_sql import plan


def connect(extension, path, readonly=False, immutable=0):
    name = f"file:{path}?mode=ro&immutable={immutable}" if readonly else str(path)
    db = sqlite3.connect(name, uri=readonly, isolation_level=None)
    db.enable_load_extension(True)
    db.execute("SELECT load_extension(?,?)", (str(extension), "sqlite3_lithograph_init"))
    db.enable_load_extension(False)
    return db


def seed(extension, path, generation):
    db = connect(extension, path)
    db.execute("SELECT lithograph_init()").fetchone()
    execute(db, "UNWIND range(0,15) AS i CREATE (:Item {v:i,g:i})-[:LINK {v:i,g:i}]->(:Endpoint)")
    for ddl in [
        "CREATE CONSTRAINT nv FOR(n:Item) REQUIRE n.v IS :: INTEGER",
        "CREATE CONSTRAINT ng FOR(n:Item) REQUIRE n.g IS :: INTEGER",
        "CREATE CONSTRAINT rv FOR()-[r:LINK]-() REQUIRE r.v IS :: INTEGER",
        "CREATE CONSTRAINT rg FOR()-[r:LINK]-() REQUIRE r.g IS :: INTEGER",
        "CREATE RANGE INDEX node_values FOR(n:Item) ON(n.v)",
        "CREATE RANGE INDEX rel_values FOR()-[r:LINK]-() ON(r.v)",
        "CREATE LOOKUP INDEX rel_types FOR()-[r]-() ON EACH type(r)",
    ]:
        old = execute(db, ddl)["summary"]["commit"]
    execute(db, "CREATE (:Item {v:99,g:99})-[:LINK {v:99,g:99}]->(:Endpoint)")
    if not generation:
        db.execute("DELETE FROM _lithograph_index_entries")
        db.execute("DELETE FROM _lithograph_index_generations")
    db.close()
    return old


def check_both(db, text, expected, options=None):
    require(execute(db, text, options=options)["rows"] == expected, text)
    require(rows(db, text, options=options) == expected, "rows: " + text)
    explained = plan(db, text, options=options)
    require("IndexSeek" not in explained, "fictional IndexSeek: " + explained)
    profile = execute(db, "PROFILE " + text, options=options)
    require(profile["rows"] == expected, "PROFILE: " + text)
    operators = profile["summary"]["profile"]["operators"]
    require(not any(op["operator"] == "IndexSeek" for op in operators), "fictional PROFILE")


def readonly_matrix(extension):
    checks = 0
    with tempfile.TemporaryDirectory(prefix="lithograph-query-only-") as tmp:
        for generation in [True, False]:
            path = Path(tmp) / f"fixture-{generation}.sqlite"
            old = seed(extension, path, generation)
            before = hashlib.sha256(path.read_bytes()).hexdigest()
            for readonly, immutable in [(False, 0), (True, 0), (True, 1)]:
                db = connect(extension, path, readonly, immutable)
                # main read-only still permits TEMP and must retain legal Index access.
                require("IndexSeek" in plan(db, "MATCH(n:Item) WHERE n.v=5 RETURN n.v"), "mode=ro confused with query_only")
                db.execute("PRAGMA query_only=ON")
                head = db.execute("SELECT commit_id FROM _lithograph_branches WHERE name='main'").fetchone()[0]
                for pattern, variable in [("(n:Item)", "n"), ("()-[r:LINK]->()", "r")]:
                    for predicate, expected in [
                        (f"{variable}.v=1000 AND {variable}.g>=0", []),
                        (f"{variable}.v=5 AND {variable}.g>=0", [[5]]),
                        (f"{variable}.v IN[1,1,3] AND {variable}.g>=0", [[1], [3]]),
                    ]:
                        text = f"MATCH {pattern} WHERE {predicate} RETURN {variable}.v ORDER BY {variable}.v"
                        check_both(db, text, expected)
                        checks += 1
                    text = f"MATCH {pattern} WHERE {variable}.v=99 RETURN {variable}.v"
                    check_both(db, text, [], {"at": old})
                    checks += 1
                check_both(db, "MATCH()-[r:LINK]->() RETURN count(r)", [[17]])
                require(head == db.execute("SELECT commit_id FROM _lithograph_branches WHERE name='main'").fetchone()[0], "Branch moved")
                db.close()
            require(hashlib.sha256(path.read_bytes()).hexdigest() == before, "main changed")
    return checks


def staged_and_errors(extension):
    with tempfile.TemporaryDirectory(prefix="lithograph-query-only-stage-") as tmp:
        path = Path(tmp) / "staged.sqlite"
        seed(extension, path, True)
        db = connect(extension, path)
        db.execute("SELECT lithograph_tx_begin('{}')").fetchone()
        execute(db, "CREATE (:Item {v:100,g:100})-[:LINK {v:100,g:100}]->(:Endpoint)")
        db.execute("PRAGMA query_only=ON")
        check_both(db, "MATCH(n:Item) WHERE n.v=100 RETURN n.v", [[100]])
        check_both(db, "MATCH()-[r:LINK]->() WHERE r.v=100 RETURN r.v", [[100]])
        db.execute("PRAGMA query_only=OFF")
        execute(db, "MATCH(n:Item) WHERE n.v=100 DETACH DELETE n")
        db.execute("PRAGMA query_only=ON")
        check_both(db, "MATCH(n:Item) WHERE n.v=100 RETURN n.v", [])
        db.execute("PRAGMA query_only=OFF")
        db.execute("SELECT lithograph_tx_abort()").fetchone()
        db.execute("PRAGMA query_only=ON")
        check_both(db, "MATCH(n:Item) WHERE n.v=100 RETURN n.v", [])
        try:
            execute(db, "CREATE (:Blocked)")
        except sqlite3.Error as error:
            require("LITHOGRAPH_IO_ERROR" in str(error), "real write IO error changed")
        else:
            raise AssertionError("query_only write unexpectedly succeeded")
        db.set_progress_handler(lambda: 1, 1)
        try:
            execute(db, "MATCH(n:Item) WHERE n.v=5 RETURN n.v")
        except sqlite3.Error as error:
            require(error.sqlite_errorcode == sqlite3.SQLITE_INTERRUPT, "real interrupt changed")
        else:
            raise AssertionError("interrupt was hidden")
        finally:
            db.set_progress_handler(None, 0)
        db.set_authorizer(lambda action, name, *_: sqlite3.SQLITE_DENY if action == sqlite3.SQLITE_PRAGMA and name == "query_only" else sqlite3.SQLITE_OK)
        try:
            execute(db, "MATCH(n:Item) WHERE n.v=5 RETURN n.v")
        except sqlite3.Error as error:
            require("LITHOGRAPH_ACCESS_DENIED" in str(error), "capability-probe error hidden")
        else:
            raise AssertionError("authorizer error was hidden")
        for pattern, variable, identifier in [("(n:Item)", "n", "n:11"), ("()-[r:LINK]->()", "r", "r:6")]:
            text = f"MATCH {pattern} WHERE elementId({variable})='{identifier}' RETURN {variable}.v"
            require(execute(db, text)["rows"] == [[5]], "IDs-only probed query_only")
            require(rows(db, text) == [[5]], "IDs-only rows probed query_only")
        db.set_authorizer(None)
        db.close()


def authorizer_matrix(extension, result=sqlite3.SQLITE_DENY, warm=False):
    with tempfile.TemporaryDirectory(prefix="lithograph-temp-authorizer-") as tmp:
        path = Path(tmp) / "authorizer.sqlite"
        seed(extension, path, True)
        db = connect(extension, path)
        if warm:
            for pattern, variable in [("(n:Item)", "n"), ("()-[r:LINK]->()", "r")]:
                text = f"MATCH {pattern} WHERE {variable}.v=5 RETURN {variable}.v"
                require("IndexSeek" in plan(db, text), "normal warm Index candidate lost")
                require(execute(db, text)["rows"] == [[5]], "normal warm scalar Index")
                require(rows(db, text) == [[5]], "normal warm rows Index")
                require("IndexSeek" in plan(db, text), "normal IF NOT EXISTS no-op rejected")
            execute(db, "CREATE (:Item {v:100,g:100})-[:LINK {v:100,g:100}]->(:Endpoint)")
        catalog_before = db.execute("SELECT name,type,sql FROM temp.sqlite_master ORDER BY name").fetchall()

        def deny_temp(action, name, argument, database, trigger):
            deny = action in (sqlite3.SQLITE_CREATE_TEMP_TABLE, sqlite3.SQLITE_CREATE_TEMP_INDEX)
            deny |= database == "temp" and action in (sqlite3.SQLITE_INSERT, sqlite3.SQLITE_UPDATE, sqlite3.SQLITE_DELETE)
            return result if deny else sqlite3.SQLITE_OK

        db.set_authorizer(deny_temp)
        for pattern, variable in [("(n:Item)", "n"), ("()-[r:LINK]->()", "r")]:
            cases = [(f"{variable}.v=1000", []), (f"{variable}.v=5", [[5]]), (f"{variable}.v IN[1,1,3]", [[1], [3]])]
            if warm:
                cases.insert(0, (f"{variable}.v=100", [[100]]))
            for predicate, expected in cases:
                text = f"MATCH {pattern} WHERE {predicate} RETURN {variable}.v ORDER BY {variable}.v"
                check_both(db, text, expected)
                check_both(db, text.replace(f"WHERE {predicate}", f"WHERE ({predicate}) OR false"), expected)
        require(db.execute("SELECT name,type,sql FROM temp.sqlite_master ORDER BY name").fetchall() == catalog_before,
                "prepare-only capability check executed TEMP DDL")
        db.set_authorizer(lambda action, name, argument, database, trigger:
                          sqlite3.SQLITE_DENY if action == sqlite3.SQLITE_READ and database == "main" and name == "_lithograph_commits" else sqlite3.SQLITE_OK)
        for adapter in (execute, rows):
            try:
                adapter(db, "MATCH(n:Item) WHERE n.v=5 RETURN n.v")
            except sqlite3.Error as error:
                require("LITHOGRAPH_ACCESS_DENIED" in str(error), "main READ denial hidden")
            else:
                raise AssertionError("main READ denial became canonical fallback")
        db.set_authorizer(None)
        db.close()


def no_index_has_no_capability_probe(extension):
    db = sqlite3.connect(":memory:", isolation_level=None)
    db.enable_load_extension(True)
    db.execute("SELECT load_extension(?,?)", (str(extension), "sqlite3_lithograph_init"))
    db.execute("SELECT lithograph_init()").fetchone()
    execute(db, "CREATE(:Visible {v:5})")
    for definition in [None, "CREATE LOOKUP INDEX labels_only FOR(n) ON EACH labels(n)"]:
        if definition:
            execute(db, definition)
        for query_only in (0, 1):
            db.execute(f"PRAGMA query_only={query_only}")
            db.set_authorizer(lambda action, name, *_:
                              sqlite3.SQLITE_DENY if action == sqlite3.SQLITE_PRAGMA and name == "query_only" else sqlite3.SQLITE_OK)
            require(execute(db, "MATCH(n:Visible) RETURN n.v")["rows"] == [[5]], "no-index capability probe")
            require(rows(db, "MATCH(n:Visible) RETURN n.v") == [[5]], "no-index rows capability probe")
            explained = plan(db, "MATCH(n:Visible) RETURN n.v")
            require("LabelIndexScan" in explained and "IndexSeek" not in explained,
                    "Node Lookup lost canonical Label access")
            db.set_authorizer(None)
        db.execute("PRAGMA query_only=OFF")
    db.close()


def run(extension):
    extension = Path(extension).resolve()
    checks = readonly_matrix(extension)
    staged_and_errors(extension)
    for result in (sqlite3.SQLITE_DENY, sqlite3.SQLITE_IGNORE):
        for warm in (False, True):
            authorizer_matrix(extension, result, warm)
    no_index_has_no_capability_probe(extension)
    print(json.dumps({"kind": "query_only", "matrix_cases": checks,
                      "extension_sha256": hashlib.sha256(extension.read_bytes()).hexdigest(),
                      "authorizer": ["DENY", "IGNORE", "fresh", "warm-new-target"],
                      "sqlite": sqlite3.sqlite_version, "status": "passed"}))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: phase22_query_only_sql.py EXTENSION")
    run(sys.argv[1])
