#!/usr/bin/env python3
"""S22.4: malformed derived manifests fall back before fresh output.

Each case owns a new small synthetic database. Graph/schema writes use the
public API; only the deliberate derived-manifest corruption uses internal SQL.
The existing phase22_query_only_sql.py gate owns actual READ AUTH, interrupt
and read-only I/O propagation; bound unit tests own active-continuation failure.
"""

import hashlib
import json
from pathlib import Path
import sqlite3
import sys
import tempfile

from phase21_predicate_routing_sql import execute, require, rows
from phase22_access_work import file_hash, open_db


def canonical_digest(db):
    tables = ("commits", "layers", "schema_objects", "branches", "node_delta",
              "label_delta", "rel_delta", "property_delta", "labels", "rel_types",
              "prop_keys", "sequences")
    digest = hashlib.sha256()
    for suffix in tables:
        content = db.execute(f"SELECT * FROM main._lithograph_{suffix}").fetchall()
        digest.update(repr((suffix, sorted(content))).encode())
    return digest.hexdigest()


def head(db):
    return db.execute("SELECT lower(hex(commit_id)) FROM main._lithograph_branches "
                      "WHERE name='main'").fetchone()[0]


def invoke(db, query, adapter):
    return execute(db, query, {"x": 7})["rows"] if adapter == "scalar" else rows(db, query, {"x": 7})


def seed(db, kind):
    db.execute("SELECT lithograph_init()").fetchone()
    if kind == "node":
        execute(db, "UNWIND range(0,31) AS i CREATE (:Metric {v:i}) FINISH")
        pattern, variable = "(n:Metric)", "n"
    else:
        execute(db, "UNWIND range(0,31) AS i "
                "CREATE (:Endpoint)-[:Metric {v:i}]->(:Endpoint) FINISH")
        pattern, variable = "()-[r:Metric]->()", "r"
    execute(db, f"CREATE CONSTRAINT metric_integer FOR {pattern} "
            f"REQUIRE {variable}.v IS :: INTEGER")
    execute(db, f"CREATE RANGE INDEX metric_value FOR {pattern} ON ({variable}.v)")
    require(db.execute("SELECT count(*) FROM main._lithograph_index_generations "
                       "WHERE complete=1 AND indexed_entities=32").fetchone() == (1,),
            "fixture did not publish its complete 32-owner generation")
    return f"MATCH {pattern} WHERE {variable}.v=$x RETURN {variable}.v"


def corrupt(db, corruption):
    if corruption == "anchor-text32":
        column, bad = "anchor_commit", "t" * 32
    elif corruption == "definition-text":
        column, bad = "definition_blob", "broken"
    else:
        column = "definition_blob"
        original = db.execute("SELECT definition_blob FROM main._lithograph_index_generations").fetchone()[0]
        require(isinstance(original, bytes) and original, "missing canonical definition bytes")
        bad = bytes([original[0] ^ 1]) + original[1:]
    # All three values are reachable under the real non-STRICT table's CHECKs.
    db.execute(f"UPDATE main._lithograph_index_generations SET {column}=?", (bad,))
    expected_type = "blob" if isinstance(bad, bytes) else "text"
    require(db.execute(f"SELECT typeof({column}) FROM main._lithograph_index_generations").fetchone()
            == (expected_type,), "SQLite coerced the intended malformed payload")


def case(extension, directory, ordinal, kind, warm, corruption, adapter):
    path = directory / f"manifest-{ordinal}-{adapter}.sqlite3"
    db = open_db(extension, path)
    try:
        query = seed(db, kind)
        before_head, before_canonical = head(db), canonical_digest(db)
        if warm:
            require(invoke(db, query, adapter) == [[7]], "warm fixture result")
            require(db.execute("SELECT count(*) FROM temp._lithograph_standard_index_cache_meta "
                               "WHERE generation_id IS NOT NULL").fetchone() == (1,),
                    "warm fixture did not retain a persistent cache binding")
        corrupt(db, corruption)
        before_main = file_hash(path)
        # The first invocation is the requested fresh/warm binding path for
        # this adapter; no EXPLAIN/helper may repair it before this assertion.
        require(invoke(db, query, adapter) == [[7]], "malformed generation changed first read")
        canonical = query.replace("=$x", "=$x OR false")
        for selected in ("scalar", "rows"):
            require(invoke(db, query, selected) == [[7]], "fallback result")
            require(invoke(db, canonical, selected) == [[7]], "canonical oracle")
            require(invoke(db, "PROFILE " + query, selected) == [[7]], "fallback PROFILE")
            explained = invoke(db, "EXPLAIN " + query, selected)
            require(explained and "IndexSeek" in explained[0][0], "fallback EXPLAIN lost access")
        require(head(db) == before_head, "fallback moved Branch head")
        require(canonical_digest(db) == before_canonical, "fallback changed canonical history")
        require(file_hash(path) == before_main, "fallback wrote main database")
        require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "fixture quick_check")
        return {"kind": kind, "binding": "warm" if warm else "cold", "corruption": corruption,
                "first_adapter": adapter, "head": before_head, "main_sha256": before_main}
    finally:
        db.close()


def run(extension):
    require(sqlite3.sqlite_version_info >= (3, 45, 0), "SQLite 3.45+ required")
    print(json.dumps({"stage": "S22.4", "group": "derived-manifest", "sqlite": sqlite3.sqlite_version,
                      "extension_sha256": file_hash(extension)}, sort_keys=True), flush=True)
    cases = (("node", False, "definition-text"), ("node", True, "anchor-text32"),
             ("relationship", False, "definition-bad-blob"),
             ("relationship", True, "definition-text"))
    checked = []
    with tempfile.TemporaryDirectory(prefix="lithograph-join-manifest-") as temporary:
        for ordinal, (kind, warm, corruption) in enumerate(cases):
            for adapter in ("scalar", "rows"):
                print(json.dumps({"testing": {"kind": kind, "warm": warm, "corruption": corruption,
                                               "first_adapter": adapter}}, sort_keys=True), flush=True)
                result = case(extension, Path(temporary), ordinal, kind, warm, corruption, adapter)
                checked.append(result)
                print(json.dumps({"case": result, "passed": True}, sort_keys=True), flush=True)
    print(json.dumps({"stage": "S22.4", "group": "derived-manifest", "cases": len(checked),
                      "sqlite": sqlite3.sqlite_version, "extension_sha256": file_hash(extension)},
                     sort_keys=True))


if __name__ == "__main__":
    run(sys.argv[1])
