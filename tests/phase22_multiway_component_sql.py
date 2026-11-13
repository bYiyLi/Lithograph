#!/usr/bin/env python3
"""S22.8 same-MATCH component/tail SQL gates, after native CBO admission.

Run only after the private component CBO gates pass for the selected production
bytes. Public CREATE/SET identities define every expected tuple. This sidecar
does not force an operator, mutate a frozen fixture or stand in for pressure.
"""

import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile

from phase21_predicate_routing_sql import execute, require
from phase22_access_work import open_db
from phase22_factorized_sql import bag, head, lifecycle, streamed
from phase22_multiway_sql import Fixture, check_profile, check_summary


LIMIT = 512 * 1024


def checkpoint(checkpoint_tool, path):
    result = subprocess.run([str(Path(checkpoint_tool).resolve()), str(path)],
                            capture_output=True, text=True, timeout=60, check=True)
    maintenance = json.loads(result.stdout)
    require(maintenance["checkpoint_count"] == 1, "missing complete component checkpoint")
    return maintenance["commit"]


def tail_query(fixture, named):
    pattern, projection = fixture.query().split(" RETURN ", 1)
    relationship = "[tail]" if named else "[]"
    identity = ",elementId(tail)" if named else ""
    return (pattern + ",(n2)-" + relationship + "->(next:S8D0) RETURN " +
            projection + identity + ",elementId(next)")


def tail_expected(fixture, named):
    expected = []
    for prefix in fixture.expected():
        source = fixture.nodes[2].index(prefix[2])
        for (start, target), relationships in fixture.relationships[2].items():
            if start != source:
                continue
            for relationship in relationships:
                if relationship in prefix[3:6]:
                    continue
                expected.append(prefix + ([relationship] if named else []) +
                                [fixture.nodes[0][target]])
    require(len(expected) == 352 * 7 == 2464, "tail oracle changed its RID exclusion")
    if named:
        require(all(row[6] not in row[3:6] for row in expected), "tail reused a prefix RID")
    return expected


def check_component(result, fixture):
    # The shared verifier consumes the component's 352-row oracle here; the
    # summary and complete tail bag are checked independently by check_summary.
    check_profile(result, fixture.expected(), len(fixture.nodes))
    return result["summary"]["profile"]["operators"]


def check_binary_tail(result, fixture):
    operators = check_component(result, fixture)
    for name in ("AdjacencySeek", "NestedLoop"):
        selected = [operator for operator in operators if operator["operator"] == name]
        require(len(selected) == 1 and selected[0]["rows"] == 2464,
                "component tail did not execute the original " + name)
        if name == "AdjacencySeek":
            require(selected[0]["dbHits"] > 0, "tail adjacency never read graph data")


def complete(db, query, expected, pin, verify, options=None):
    columns = None
    for adapter in (execute, streamed):
        plain = adapter(db, query, options=options)
        check_summary(plain, expected, pin)
        require("profile" not in plain["summary"], "plain component exposed stale PROFILE")
        profiled = adapter(db, "PROFILE " + query, options=options)
        check_summary(profiled, expected, pin)
        require(profiled["columns"] == plain["columns"], "PROFILE changed component columns")
        require(columns is None or columns == plain["columns"], "component surface columns differ")
        columns = plain["columns"]
        verify(profiled)
    require(head(db) == pin and not db.in_transaction, "component read changed publication state")


def integrity(db):
    require(db.execute("PRAGMA integrity_check").fetchone() == ("ok",), "component SQLite integrity")
    canonical = json.loads(db.execute("SELECT lithograph_integrity_check()").fetchone()[0])
    require(canonical["ok"], "component canonical integrity")


def untyped_tails(extension, checkpoint_tool, directory):
    path = directory / "untyped-tail.sqlite3"
    db = open_db(extension, path)
    try:
        fixture = Fixture(db, "triangle", 1)
        require(len(fixture.expected()) == 352, "unexpected component oracle")
    finally:
        db.close()
    pin = checkpoint(checkpoint_tool, path)
    db = open_db(extension, path)
    try:
        require(head(db) == pin, "tail checkpoint pin differs")
        for named in (True, False):
            query = tail_query(fixture, named)
            expected = tail_expected(fixture, named)
            complete(db, query, expected, pin, lambda result: check_binary_tail(result, fixture))
            if named:
                lifecycle(db, query, expected)
        integrity(db)
    finally:
        db.close()
    return {"case": "same-match-untyped-named-and-anonymous-tail", "component_rows": 352,
            "output_rows": 2464, "surfaces": ["scalar", "rows"], "passed": True}


def hash_seed(db, fixture):
    source_keys = {identity: source for (source, _), identities in fixture.relationships[2].items()
                   for identity in identities}
    require(len(source_keys) == 128, "closing relationship domain differs")
    updated = execute(db, "MATCH(source:S8D2)-[r:S8C]->(:S8D0) SET r.k=source.seq "
                      "RETURN elementId(r),r.k")["rows"]
    require(len(updated) == len(source_keys), "incomplete public relationship SET")
    seen = set()
    for identity, key in updated:
        require(identity in source_keys and identity not in seen, "unexpected SET RID")
        require(type(key) is int and source_keys[identity] == key, "SET changed closing key")
        seen.add(identity)
    created = execute(db, "UNWIND range(0,1023) AS seq "
        "CREATE(n:S8ComponentBuild {seq:seq,k:CASE WHEN seq<2 THEN 0 "
        "WHEN seq=2 THEN 13 WHEN seq=3 THEN null ELSE 100000+seq END}) "
        "RETURN seq,elementId(n),n.k")["rows"]
    require(len(created) == 1024, "incomplete public Hash tail CREATE")
    build, sequences = {}, set()
    cycle_nodes = {identity for domain in fixture.nodes for identity in domain}
    for seq, identity, key in created:
        require(type(seq) is int and 0 <= seq < 1024 and seq not in sequences,
                "Hash tail CREATE sequence")
        require(identity.startswith("n:") and identity not in build and identity not in cycle_nodes,
                "Hash tail NodeId reused")
        expected_key = 0 if seq < 2 else 13 if seq == 2 else None if seq == 3 else 100000 + seq
        require(key == expected_key and (key is None or type(key) is int), "Hash tail typed key")
        sequences.add(seq)
        build[identity] = key
    for ddl in (
            "CREATE CONSTRAINT s8_component_rel_key FOR ()-[r:S8C]-() REQUIRE r.k IS :: INTEGER",
            "CREATE CONSTRAINT s8_component_build_key FOR(n:S8ComponentBuild) REQUIRE n.k IS :: INTEGER"):
        execute(db, ddl)
    expected = []
    for prefix in fixture.expected():
        key = source_keys[prefix[5]]
        for identity, build_key in build.items():
            if build_key is not None and build_key == key:
                expected.append(prefix + [key, identity, build_key])
    require(len(expected) == 66, "Hash tail oracle size")
    pattern, projection = fixture.query().split(" RETURN ", 1)
    query = (pattern + ",(tail:S8ComponentBuild) WHERE r2.k=tail.k RETURN " +
             projection + ",r2.k,elementId(tail),tail.k")
    return query, expected


def check_hash_tail(result, fixture, spill):
    operators = check_component(result, fixture)
    selected = [operator for operator in operators if operator["operator"] == "HashJoin"]
    require(len(selected) == 1 and selected[0]["rows"] == 66, "Hash tail not actually executed")
    work = selected[0]["hash"]
    require(work["buildRows"] == 1024 and work["probeRows"] == 352, "Hash tail input counters")
    for field in ("buildRows", "probeRows", "hashCollisions", "spillPartitions", "spillBatches",
                  "spillVmSteps", "spillEncodedBytes", "memoryPeakBytes", "tempAllocatedBytes"):
        require(type(work[field]) is int and work[field] >= 0, "invalid actual Hash counter: " + field)
    if spill:
        require(work["spillPartitions"] > 1, "small budget did not use partitioned Grace")
        require(all(work[field] > 0 for field in
                    ("spillBatches", "spillVmSteps", "spillEncodedBytes", "tempAllocatedBytes")),
                "Grace must execute real TEMP spill work")
        require(result["summary"]["profile"]["executionMemoryPeakBytes"] <= LIMIT,
                "Hash component exceeded the actual 512 KiB execution ledger")


def budget_lifecycle(db, query, expected, pin, fixture):
    # The shared default-budget lifecycle cannot prove cancellation prepared
    # with a quota. Pass this same quota to the initial live SQL cursor too.
    options = {"memoryLimitBytes": LIMIT}
    oracle = bag(expected)
    for interrupt in (False, True):
        cursor = db.execute("SELECT event,data FROM lithograph_rows(?,?,?)",
                            ("PROFILE " + query, '{}', json.dumps(options)))
        try:
            for event, data in cursor:
                require(event in ("columns", "row"), "Grace completed before first row")
                if event == "row":
                    require(oracle[tuple(json.loads(data))] > 0, "first Grace row is outside the bag")
                    break
            else:
                raise AssertionError("Grace produced no first row")
            if interrupt:
                db.interrupt()
                try:
                    cursor.fetchall()
                except sqlite3.Error as error:
                    require(error.sqlite_errorcode == sqlite3.SQLITE_INTERRUPT, "wrong Grace interrupt")
                else:
                    raise AssertionError("live Grace cursor ignored SQLite interrupt")
        finally:
            cursor.close()
        require(head(db) == pin and not db.in_transaction, "cancelled Grace changed publication")
        recovered = streamed(db, "PROFILE " + query, options=options)
        check_summary(recovered, expected, pin)
        check_hash_tail(recovered, fixture, True)


def hash_tail(extension, checkpoint_tool, directory):
    path = directory / "hash-tail.sqlite3"
    db = open_db(extension, path)
    try:
        fixture = Fixture(db, "triangle", 1)
        query, expected = hash_seed(db, fixture)
    finally:
        db.close()
    pin = checkpoint(checkpoint_tool, path)
    db = open_db(extension, path)
    try:
        require(head(db) == pin, "Hash component checkpoint pin differs")
        for limit in (None, LIMIT):
            options = {} if limit is None else {"memoryLimitBytes": limit}
            complete(db, query, expected, pin,
                     lambda result: check_hash_tail(result, fixture, limit is not None), options)
        budget_lifecycle(db, query, expected, pin, fixture)
        integrity(db)
    finally:
        db.close()
    return {"case": "same-match-closing-key-hash-grace-tail", "component_rows": 352,
            "output_rows": 66, "build_rows": 1024, "probe_rows": 352,
            "grace_memory_limit_bytes": LIMIT, "surfaces": ["scalar", "rows"], "passed": True}


def run(extension, checkpoint_tool):
    require(sqlite3.sqlite_version_info >= (3, 45, 0), "SQLite 3.45+ required")
    cases = []
    with tempfile.TemporaryDirectory(prefix="lithograph-multiway-component-") as temporary:
        for run_case in (untyped_tails, hash_tail):
            result = run_case(extension, checkpoint_tool, Path(temporary))
            cases.append(result)
            print(json.dumps(result), flush=True)
    print(json.dumps({"stage": "S22.8", "sqlite": sqlite3.sqlite_version, "cases": cases,
                      "completed": True}), flush=True)


if __name__ == "__main__":
    require(len(sys.argv) == 3,
            "usage: phase22_multiway_component_sql.py <extension> <checkpoint-tool>")
    run(sys.argv[1], sys.argv[2])
