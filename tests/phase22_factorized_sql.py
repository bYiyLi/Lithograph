#!/usr/bin/env python3
"""S22.7 real SQL adapters, complete identity bags and cursor lifecycle.

Small public CREATE fixtures exercise the actual cost-selected Group/ASP paths.
This is a correctness gate, not a benchmark. The checkpoint helper only builds
derived state after the creating connection closes; no canonical SQL is edited.
"""

from collections import Counter
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile

from phase21_predicate_routing_sql import execute, require
from phase22_access_work import open_db


STAR_MATCH = "MATCH (h:Hub)-[r:A]->(a:LeafA),(h)-[s:B]->(b:LeafB) WHERE h.k=$key"
STAR_RETURN = " RETURN elementId(h),elementId(r),elementId(a),elementId(s),elementId(b)"
STAR = STAR_MATCH + STAR_RETURN
ASP = ("MATCH (h:Hub)-[r:A]->(a:LeafA),(h)-[s:B]->(b:LeafB),(d:Build) "
       "WHERE h.k=$key AND a=d" + STAR_RETURN + ",elementId(d)")


def head(db):
    return "commit/" + db.execute(
        "SELECT lower(hex(commit_id)) FROM main._lithograph_branches WHERE name='main'"
    ).fetchone()[0]


def bag(values):
    return Counter(tuple(value) for value in values)


def streamed(db, query, params=None, options=None):
    result = {"columns": None, "rows": [], "summary": None}
    cursor = db.execute("SELECT event,data FROM lithograph_rows(?,?,?)", (
        query, json.dumps(params or {}), json.dumps(options or {})))
    try:
        for event, data in cursor:
            require(result["summary"] is None, "event after terminal summary")
            value = json.loads(data)
            if event == "columns":
                require(result["columns"] is None and not result["rows"], "late/duplicate columns")
                result["columns"] = value
            elif event == "row":
                require(result["columns"] is not None, "row before columns")
                result["rows"].append(value)
            elif event == "summary":
                result["summary"] = value
            else:
                raise AssertionError("unknown row event: " + event)
    finally:
        cursor.close()
    require(result["columns"] is not None and result["summary"] is not None, "incomplete stream")
    return result


def arm(db, kind, label, count, hub):
    created = execute(db, f"MATCH(h:Hub) WHERE h.k=7 UNWIND range(1,{count}) AS i "
        f"CREATE(n:{label}) CREATE(h)-[r:{kind}]->(n),(h)-[s:{kind}]->(n) "
        "RETURN elementId(n),elementId(r),elementId(startNode(r)),elementId(endNode(r)),"
        "elementId(s),elementId(startNode(s)),elementId(endNode(s))")["rows"]
    require(len(created) == count, "incomplete public CREATE")
    result = []
    for node, first, start, end, second, other_start, other_end in created:
        require(start == other_start == hub and end == other_end == node, "CREATE endpoints")
        require(node.startswith("n:") and first.startswith("r:") and second.startswith("r:"),
                "untyped CREATE identity")
        result.extend([[first, node], [second, node]])
    require(len({row[0] for row in result}) == 2 * count, "parallel relationship identity reused")
    require(len({row[1] for row in result}) == count, "endpoint identity reused")
    return result


def seed(db, kind):
    db.execute("SELECT lithograph_init()").fetchone()
    hub = execute(db, "CREATE(h:Hub {k:7}) RETURN elementId(h)")["rows"][0][0]
    execute(db, "CREATE CONSTRAINT hub_integer FOR(h:Hub) REQUIRE h.k IS :: INTEGER")
    execute(db, "CREATE CONSTRAINT hub_unique FOR(h:Hub) REQUIRE h.k IS UNIQUE")
    left = arm(db, "A", "LeafA", 2 if kind == "asp" else 64, hub)
    right = arm(db, "B", "LeafB", 2 if kind == "asp" else 128, hub)
    if kind == "asp":
        extra = execute(db, "MATCH(h:Hub),(a:LeafA) WHERE h.k=7 "
            "UNWIND range(1,62) AS i CREATE(h)-[r:A]->(a) "
            "RETURN elementId(r),elementId(a),elementId(startNode(r)),elementId(endNode(r))")["rows"]
        require(len(extra) == 124, "ASP multiplicity recipe")
        for relationship, node, start, end in extra:
            require(start == hub and end == node, "ASP extra endpoints")
            left.append([relationship, node])
        selected = left[0][1]
        execute(db, "MATCH(a:LeafA) WHERE elementId(a)=$id SET a:Build FINISH", {"id": selected})
        execute(db, "UNWIND range(1,64) AS i CREATE(:Build {noise:i}) FINISH")
        expected = [[hub, *a, *b, selected] for a in left for b in right if a[1] == selected]
        require(len(expected) == 256, "ASP oracle size")
        return hub, left, right, [("asp", ASP, expected)]
    expected = [[hub, *a, *b] for a in left for b in right]
    require(len(expected) == 32768, "Star oracle size")
    return hub, left, right, [("star", STAR, expected)]


def check_profile(result, name, expected_rows):
    summary = result["summary"]
    operators = summary["profile"]["operators"]
    require(sum(op["dbHits"] for op in operators) == summary["metrics"]["dbHits"], "PROFILE sum")
    groups = [op for op in operators if op["operator"] == "FactorizedAccumulate"]
    require(len(groups) == 1 and groups[0]["rows"] > 0, "CBO did not execute Group")
    require(groups[0]["factorized"]["groupMembers"] == (132 if name == "asp" else 384),
            "Group must retain members, not expanded product rows")
    flatten = [op for op in operators if op["operator"] == "Flatten"]
    require(len(flatten) == 1 and flatten[0]["rows"] == expected_rows, "actual Flatten rows")
    require(flatten[0]["factorized"]["flattenedRows"] == expected_rows, "Flatten counter")
    for op in operators:
        for value in op.get("factorized", {}).values():
            require(type(value) is int and value >= 0, "invalid public factor counter")
    if name == "asp":
        joined = [op for op in operators if op["operator"] == "ASPJoin"]
        require(len(joined) == 1, "CBO did not execute ASP")
        metrics = joined[0]["factorized"]
        for key, expected in (("semijoinKeys", 2), ("semijoinSeeks", 2), ("buildRows", 1),
                              ("hashEntries", 1), ("probeRows", 128)):
            require(metrics[key] == expected, "ASP physical work differs: " + key)


def complete_adapters(db, name, query, expected, pin):
    oracle = bag(expected)
    columns = None
    for adapter in (execute, streamed):
        result = adapter(db, "PROFILE " + query, {"key": 7})
        require(bag(result["rows"]) == oracle, name + " lost complete identity bag")
        require(columns is None or result["columns"] == columns, "visible columns differ")
        columns = result["columns"]
        summary = result["summary"]
        require(summary["commit"] == pin and summary["metrics"]["rows"] == len(expected),
                "terminal summary differs")
        require(all(value == 0 for value in summary["counters"].values()), "read changed counters")
        check_profile(result, name, len(expected))
        plain = adapter(db, query, {"key": None})
        require(plain["rows"] == [] and "profile" not in plain["summary"], "stale binding/profile")
    require(head(db) == pin, "read moved Branch")


def prefix_consumers(db, query, expected):
    oracle = bag(expected)
    with_names = "h,r,a,s,b,d" if query == ASP else "h,r,a,s,b"
    shapes = [query + " LIMIT $take", query + " SKIP 3 LIMIT $take",
              query.replace(" RETURN ", " WITH " + with_names + " LIMIT $take RETURN ")]
    for adapter in (execute, streamed):
        for shape in shapes:
            for take in (0, 1, 10):
                result = adapter(db, "PROFILE " + shape, {"key": 7, "take": take})
                require(len(result["rows"]) == take, "partial consumer row count")
                require(not (bag(result["rows"]) - oracle), "partial consumer lost identity bag")
                operators = result["summary"]["profile"]["operators"]
                require(not any(op["operator"] in ("ASPJoin", "FactorizedAccumulate")
                                for op in operators), "partial consumer paid full accumulation")
                if take == 0:
                    require(result["summary"]["metrics"]["dbHits"] == 0, "LIMIT 0 touched graph")


def lifecycle(db, query, expected):
    oracle = bag(expected)
    for interrupt in (False, True):
        cursor = db.execute("SELECT event,data FROM lithograph_rows(?,?,?)",
                            (query, '{"key":7}', '{}'))
        try:
            for event, _ in cursor:
                require(event != "summary", "stream completed before first row")
                if event == "row":
                    break
            else:
                raise AssertionError("missing first row")
            if interrupt:
                db.interrupt()
                try:
                    cursor.fetchall()
                except sqlite3.Error as error:
                    require(error.sqlite_errorcode == sqlite3.SQLITE_INTERRUPT, "wrong interrupt error")
                else:
                    raise AssertionError("live cursor ignored SQLite interrupt")
        finally:
            cursor.close()
        require(bag(streamed(db, query, {"key": 7})["rows"]) == oracle, "closed cursor retained state")


def aggregate_and_policy(db):
    query = STAR_MATCH + " RETURN count(*) AS n"
    for adapter in (execute, streamed):
        result = adapter(db, "PROFILE " + query, {"key": 7})
        require(result["rows"] == [[32768]], "group aggregate multiplicity")
        check_profile(result, "star", 0)
        require(result["summary"]["metrics"]["rows"] == 1, "aggregate row counted as Flatten")
        for options in ({"readAccess": {"query": {"denyProperties": ["k"]}}},
                        {"graphView": {"requireAnyLabels": ["Hub", "LeafA"]}}):
            require(adapter(db, STAR, {"key": 7}, options)["rows"] == [], "invisible group leaked")


def staged_and_history(db, observer, hub, left, right, expected, pin):
    query = "MATCH(h:Hub),(a:LeafA) WHERE h.k=7 AND elementId(a)=$id "
    create = query + "CREATE(h)-[r:A]->(a) RETURN elementId(r),elementId(a)"
    count_sql = "SELECT count(*) FROM main._lithograph_commits"
    committed_count = observer.execute(count_sql).fetchone()[0]
    begin = json.loads(db.execute("SELECT lithograph_tx_begin('{}')").fetchone()[0])
    require(begin["baseCommit"] == pin, "transaction pinned the wrong base")
    written = execute(db, create, {"id": left[0][1]})
    require(written["summary"]["commit"] is None, "staged write exposed a public Commit")
    created = written["rows"]
    require(len(created) == 1, "staged relationship recipe")
    oracle = expected + [[hub, *created[0], *b] for b in right]
    staged = streamed(db, STAR, {"key": 7})
    require(staged["summary"]["commit"] is None, "staged read exposed a public Commit")
    require(bag(staged["rows"]) == bag(oracle), "stale staged group")
    # Private uncommitted chain entries are not public history. Observe durable
    # publication from a second connection, rather than the writer's raw tables.
    require(db.in_transaction and head(observer) == pin, "staged query published early")
    require(observer.execute(count_sql).fetchone()[0] == committed_count, "staged history leaked")
    require(bag(streamed(observer, STAR, {"key": 7})["rows"]) == bag(expected), "staged rows leaked")
    db.execute("SELECT lithograph_tx_abort()").fetchone()
    require(head(db) == pin and db.execute(count_sql).fetchone()[0] == committed_count,
            "abort retained private transaction history")
    require(bag(streamed(db, STAR, {"key": 7})["rows"]) == bag(expected), "aborted group survived")
    execute(db, create, {"id": left[0][1]})
    require(head(db) != pin, "history check needs a different current Commit")
    require(bag(execute(db, STAR, {"key": 7}, {"at": pin})["rows"]) == bag(expected),
            "historical group observed the new relationship")


def case(extension, checkpoint, directory, kind):
    path = directory / (kind + ".sqlite3")
    db = open_db(extension, path)
    try:
        hub, left, right, cases = seed(db, kind)
    finally:
        db.close()
    result = subprocess.run([str(Path(checkpoint).resolve()), str(path)],
                            capture_output=True, text=True, timeout=60, check=True)
    maintenance = json.loads(result.stdout)
    require(maintenance["checkpoint_count"] == 1, "missing complete checkpoint")
    db = open_db(extension, path)
    try:
        pin = head(db)
        require(maintenance["commit"] == pin, "checkpoint pin differs")
        for name, query, expected in cases:
            complete_adapters(db, name, query, expected, pin)
            prefix_consumers(db, query, expected)
            lifecycle(db, query, expected)
        if kind == "star":
            aggregate_and_policy(db)
            observer = open_db(extension, path, readonly=True)
            try:
                staged_and_history(db, observer, hub, left, right, cases[0][2], pin)
            finally:
                observer.close()
        require(db.execute("PRAGMA integrity_check").fetchone() == ("ok",), "SQLite integrity")
    finally:
        db.close()
    return {"case": kind, "rows": len(cases[0][2]), "adapters": ["scalar", "rows"], "passed": True}


def run(extension, checkpoint):
    require(sqlite3.sqlite_version_info >= (3, 45, 0), "SQLite 3.45+ required")
    results = []
    with tempfile.TemporaryDirectory(prefix="lithograph-factorized-") as temporary:
        for kind in ("star", "asp"):
            result = case(extension, checkpoint, Path(temporary), kind)
            results.append(result)
            print(json.dumps(result), flush=True)
    print(json.dumps({"stage": "S22.7", "sqlite": sqlite3.sqlite_version, "cases": results}), flush=True)


if __name__ == "__main__":
    require(len(sys.argv) == 3, "usage: phase22_factorized_sql.py <extension> <checkpoint-tool>")
    run(sys.argv[1], sys.argv[2])
