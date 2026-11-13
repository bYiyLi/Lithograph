#!/usr/bin/env python3
"""S22.8 real SQL surfaces, complete cyclic identity bags and lifecycle.

Public CREATE captures every NodeId, RelationshipId and endpoint used by the
independent Cartesian oracle. These bounded fixtures mirror the Core tests;
they are a correctness gate, not pressure or latency measurements. Only the
existing checkpoint helper constructs derived state after the writer closes.
Positive cycles use sixteen nodes per domain and degree-eight circulant edges.
The former single-Hub dense Cartesian cycle remains a cheaper-binary fallback.
"""

from itertools import product
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile

from phase21_predicate_routing_sql import execute, require
from phase22_access_work import open_db
from phase22_factorized_sql import bag, head, lifecycle, streamed


SHAPES = {
    "triangle": [(0, 1, "S8A"), (1, 2, "S8B"), (2, 0, "S8C")],
    "four-cycle": [(0, 1, "S8A"), (1, 2, "S8B"), (2, 3, "S8C"), (3, 0, "S8D")],
    "clique": [(0, 1, "S8A"), (0, 2, "S8B"), (0, 3, "S8C"),
               (1, 2, "S8D"), (1, 3, "S8E"), (2, 3, "S8F")],
    "shared-cycles": [(0, 1, "S8A"), (1, 2, "S8B"), (2, 0, "S8C"),
                      (0, 2, "S8D"), (2, 3, "S8E"), (3, 0, "S8F")],
}
EXPECTED_ROWS = {"triangle": 352, "four-cycle": 5056, "clique": 1920, "shared-cycles": 16}
MULTIWAY_EVENTS = ("inputLists", "adjacencyRows", "seekOperations", "intersectionSteps",
                   "candidateNodes", "nodeAssignments", "relationshipCandidates")


class Fixture:
    """Captured CREATE identities define the oracle independently of MATCH."""

    def __init__(self, db, shape, first_copies, dense=False):
        self.edges = SHAPES[shape]
        self.nodes = []
        self.relationships = []
        all_nodes, all_relationships = set(), set()
        domains = max(max(source, target) for source, target, _ in self.edges) + 1
        db.execute("SELECT lithograph_init()").fetchone()
        for domain in range(domains):
            size = (1 if domain == 0 else 32) if dense else 16
            created = execute(db, f"UNWIND range(0,{size-1}) AS seq "
                f"CREATE(n:S8D{domain} {{seq:seq}}) RETURN seq,elementId(n)")["rows"]
            captured = [None] * size
            require(len(created) == size, "incomplete node CREATE")
            for seq, identity in created:
                require(type(seq) is int and 0 <= seq < size, "untyped CREATE sequence")
                require(identity.startswith("n:") and identity not in all_nodes, "reused NodeId")
                require(captured[seq] is None, "duplicate CREATE sequence")
                captured[seq] = identity
                all_nodes.add(identity)
            require(None not in captured, "missing public NodeId")
            self.nodes.append(captured)
        for ordinal, (source, target, kind) in enumerate(self.edges):
            copies = first_copies if ordinal == 0 else 1
            pairs = {(left, right) for left in range(len(self.nodes[source]))
                     for right in range(len(self.nodes[target]))
                     if dense or (right - left + 16) % 16 < 8}
            where = "" if dense else "WHERE ((b.seq-a.seq+16)%16)<8 "
            created = execute(db, f"MATCH(a:S8D{source}),(b:S8D{target}) "
                f"{where}UNWIND range(1,{copies}) AS copy CREATE(a)-[r:{kind}]->(b) "
                "RETURN a.seq,b.seq,elementId(a),elementId(b),elementId(r),"
                "elementId(startNode(r)),elementId(endNode(r)),copy")["rows"]
            require(len(created) == len(pairs) * copies, "incomplete relationship CREATE")
            captured = {}
            for left, right, start, end, identity, actual_start, actual_end, copy in created:
                require(type(left) is int and type(right) is int and type(copy) is int,
                        "untyped edge CREATE")
                require(start == actual_start == self.nodes[source][left] and
                        end == actual_end == self.nodes[target][right], "CREATE endpoints")
                require(1 <= copy <= copies, "invalid parallel edge ordinal")
                require(identity.startswith("r:") and identity not in all_relationships,
                        "parallel RelationshipId reused")
                all_relationships.add(identity)
                captured.setdefault((left, right), []).append(identity)
            require(set(captured) == pairs and
                    all(len(identities) == copies for identities in captured.values()),
                    "public CREATE changed endpoint multiplicity")
            self.relationships.append(captured)

    def query(self):
        pattern = ",".join(f"(n{source}:S8D{source})-[r{ordinal}:{kind}]->"
                           f"(n{target}:S8D{target})"
                           for ordinal, (source, target, kind) in enumerate(self.edges))
        columns = [f"elementId(n{node})" for node in range(len(self.nodes))]
        columns += [f"elementId(r{edge})" for edge in range(len(self.edges))]
        return "MATCH " + pattern + " RETURN " + ",".join(columns)

    def expected(self, extra=None):
        result = []
        for selected in product(*(range(len(domain)) for domain in self.nodes)):
            nodes = [domain[seq] for domain, seq in zip(self.nodes, selected)]
            choices = []
            for ordinal, (source, target, _) in enumerate(self.edges):
                key = selected[source], selected[target]
                identities = list(self.relationships[ordinal].get(key, []))
                if extra is not None and (ordinal, key) == extra[:2]:
                    identities.append(extra[2])
                choices.append(identities)
            for relationships in product(*choices):
                if len(set(relationships)) == len(relationships):
                    result.append(nodes + list(relationships))
        return result


class OverlapFixture(Fixture):
    """Three domains share actual NodeIds and one typed RelationshipId bag."""

    def __init__(self, db):
        db.execute("SELECT lithograph_init()").fetchone()
        created = execute(db, "UNWIND range(0,15) AS seq "
            "CREATE(n:S8O0:S8O1:S8O2 {seq:seq}) RETURN seq,elementId(n)")["rows"]
        require(len(created) == 16, "incomplete overlapping node CREATE")
        nodes = [None] * 16
        for seq, identity in created:
            require(type(seq) is int and 0 <= seq < 16, "overlap node sequence")
            require(nodes[seq] is None and identity.startswith("n:"), "overlap NodeId")
            nodes[seq] = identity
        require(None not in nodes and len(set(nodes)) == 16, "overlap Node identities reused")
        self.nodes = [nodes, nodes, nodes]
        self.edges = [(0, 1, "S8Overlap"), (1, 2, "S8Overlap"), (2, 0, "S8Overlap")]
        created = execute(db, "MATCH(a:S8O0),(b:S8O1) "
            "WHERE ((b.seq-a.seq+16)%16)<8 UNWIND range(1,3) AS copy "
            "CREATE(a)-[r:S8Overlap]->(b) RETURN a.seq,b.seq,elementId(a),elementId(b),"
            "elementId(r),elementId(startNode(r)),elementId(endNode(r)),copy")["rows"]
        require(len(created) == 16 * 8 * 3, "incomplete overlap relationship CREATE")
        captured, identities, copies = {}, set(), set()
        for left, right, start, end, identity, actual_start, actual_end, copy in created:
            require(type(left) is int and type(right) is int and type(copy) is int,
                    "untyped overlap edge CREATE")
            require(0 <= left < 16 and 0 <= right < 16 and 1 <= copy <= 3,
                    "invalid overlap CREATE sequence")
            require(start == actual_start == nodes[left] and end == actual_end == nodes[right],
                    "overlap CREATE endpoints")
            require(identity.startswith("r:") and identity not in identities, "overlap RID reused")
            require((left, right, copy) not in copies, "duplicate overlap CREATE copy")
            identities.add(identity)
            copies.add((left, right, copy))
            captured.setdefault((left, right), []).append(identity)
        pairs = {(left, right) for left in range(16) for right in range(16)
                 if (right-left+16) % 16 < 8}
        require(set(captured) == pairs and all(len(ids) == 3 for ids in captured.values()),
                "overlap endpoint multiplicity differs")
        # Each pattern edge reads the same physical typed adjacency. Independent
        # per-edge copies would hide the runtime's per-MATCH RID uniqueness.
        self.relationships = [captured, captured, captured]

    def query(self):
        return ("MATCH (n0:S8O0)-[r0:S8Overlap]->(n1:S8O1),"
                "(n1)-[r1:S8Overlap]->(n2:S8O2),(n2)-[r2:S8Overlap]->(n0) "
                "RETURN elementId(n0),elementId(n1),elementId(n2),"
                "elementId(r0),elementId(r1),elementId(r2)")


def check_summary(result, expected, pin):
    require(bag(result["rows"]) == bag(expected), "incomplete cyclic identity/RID bag")
    summary = result["summary"]
    require(summary["commit"] == pin, "wrong immutable/staged pin")
    require(summary["metrics"]["rows"] == len(expected), "terminal output count differs")
    require(all(value == 0 for value in summary["counters"].values()), "read changed counters")


def check_profile(result, expected, domains, owners=1):
    summary = result["summary"]
    profile = summary["profile"]
    operators = profile["operators"]
    require(sum(operator["dbHits"] for operator in operators) == summary["metrics"]["dbHits"],
            "PROFILE dbHits sum differs")
    selected = [operator for operator in operators if operator["operator"] == "MultiwayIntersect"]
    require(len(selected) == 1 and selected[0]["rows"] == len(expected),
            "normal CBO did not execute MultiwayIntersect")
    require(selected[0]["dbHits"] > 0, "intersection graph reads not attributed")
    work = selected[0]["multiway"]
    for key in (*MULTIWAY_EVENTS, "memoryPeakBytes"):
        require(type(work[key]) is int and work[key] > 0, "missing actual multiway work: " + key)
    require(work["nodeAssignments"] == owners * len({tuple(row[:domains]) for row in expected}),
            "node intersections confused with parallel Relationship bag")
    require(work["relationshipCandidates"] >= len(expected), "missing relationship enumeration")
    require(profile["executionMemoryPeakBytes"] >= work["memoryPeakBytes"], "invalid owner peak")
    rss = profile["processPeakRssBytes"]
    require(rss is None or type(rss) is int and rss > 0, "invalid process RSS encoding")
    return work


def repeated_owners(db, fixture, expected, pin):
    """One site accumulates event deltas from two sequential pull owners."""
    for adapter in (execute, streamed):
        # Keep the incoming binding's shape identical. The tag is intentionally
        # absent from the projection, so the complete identity bag doubles.
        single = adapter(db, "PROFILE UNWIND [1] AS tag " + fixture.query())
        check_summary(single, expected, pin)
        before = check_profile(single, expected, len(fixture.nodes))
        doubled = adapter(db, "PROFILE UNWIND [1,2] AS tag " + fixture.query())
        check_summary(doubled, expected + expected, pin)
        after = check_profile(doubled, expected + expected, len(fixture.nodes), owners=2)
        for key in MULTIWAY_EVENTS:
            require(after[key] == 2 * before[key], "site lost the first owner's events: " + key)
        require(after["memoryPeakBytes"] == before["memoryPeakBytes"],
                "sequential owner memory peaks must use max, not sum")
        plain = adapter(db, "UNWIND [1,2] AS tag " + fixture.query())
        check_summary(plain, expected + expected, pin)
        require("profile" not in plain["summary"], "repeated owner leaked PROFILE")


def complete_adapters(db, fixture, expected, pin, multiway):
    columns = None
    for adapter in (execute, streamed):
        plain = adapter(db, fixture.query())
        check_summary(plain, expected, pin)
        require("profile" not in plain["summary"], "plain query exposed stale PROFILE")
        profiled = adapter(db, "PROFILE " + fixture.query())
        check_summary(profiled, expected, pin)
        require(profiled["columns"] == plain["columns"], "PROFILE changed result columns")
        require(columns is None or plain["columns"] == columns, "SQL surface columns differ")
        columns = plain["columns"]
        if multiway:
            check_profile(profiled, expected, len(fixture.nodes))
        else:
            operators = profiled["summary"]["profile"]["operators"]
            require(not any(operator["operator"] == "MultiwayIntersect" for operator in operators),
                    "dense no-pruning fixture paid for more expensive intersection")
            require(sum(operator["dbHits"] for operator in operators) ==
                    profiled["summary"]["metrics"]["dbHits"], "fallback PROFILE sum differs")
    require(head(db) == pin, "read moved Branch")


def prefix_consumers(db, query, expected):
    oracle = bag(expected)
    for adapter in (execute, streamed):
        for take in (0, 1):
            for suffix in (f" LIMIT {take}", " LIMIT $take"):
                result = adapter(db, "PROFILE " + query + suffix, {"take": take})
                require(len(result["rows"]) == take, "partial cyclic result count")
                require(not (bag(result["rows"]) - oracle), "partial cyclic result identity")
                summary = result["summary"]
                operators = summary["profile"]["operators"]
                require(not any(operator["operator"] == "MultiwayIntersect"
                                for operator in operators), "partial consumer selected full cost proof")
                require(sum(operator["dbHits"] for operator in operators) ==
                        summary["metrics"]["dbHits"], "prefix PROFILE sum differs")
                if take == 0:
                    require(summary["metrics"]["dbHits"] == 0, "LIMIT 0 touched graph")


def policy_and_errors(db, fixture, expected):
    query = fixture.query()
    filtered = query.replace(" RETURN ", " WHERE n1.seq=$seq RETURN ")
    subset = [row for row in expected if row[1] == fixture.nodes[1][0]]
    for adapter in (execute, streamed):
        visible = {"graphView": {"requireAnyLabels": [f"S8D{i}" for i in range(len(fixture.nodes))]}}
        require(bag(adapter(db, query, options=visible)["rows"]) == bag(expected),
                "visible cyclic endpoints disappeared")
        require(adapter(db, query, options={"graphView": {"excludeAnyLabels": ["S8D1"]}})
                ["rows"] == [], "Graph View leaked cyclic endpoints")
        require(bag(adapter(db, filtered, {"seq": 0})["rows"]) == bag(subset),
                "filtered cyclic bag differs")
        hidden = {"readAccess": {"query": {"denyProperties": ["seq"]}}}
        require(adapter(db, filtered, {"seq": 0}, hidden)["rows"] == [],
                "query-denied property leaked")
        result_only = {"readAccess": {"result": {"denyProperties": ["seq"]}}}
        require(bag(adapter(db, filtered, {"seq": 0}, result_only)["rows"]) == bag(subset),
                "result-only policy changed query access")
        require(adapter(db, filtered, {"seq": None})["rows"] == [], "NULL predicate changed")
        try:
            adapter(db, filtered, {"seq": "wrong-family"})
        except sqlite3.Error as error:
            require("LITHOGRAPH_TYPE_ERROR" in str(error), "different equality family lost error")
        else:
            raise AssertionError("different scalar families must retain equality error")
        require(bag(adapter(db, filtered, {"seq": 0})["rows"]) == bag(subset),
                "failed invocation retained state")


def add_parallel(db, fixture):
    rows = execute(db, "MATCH(a:S8D0),(b:S8D1) "
        "WHERE elementId(a)=$start AND elementId(b)=$end "
        "CREATE(a)-[r:S8A]->(b) "
        "RETURN elementId(r),elementId(startNode(r)),elementId(endNode(r))",
        {"start": fixture.nodes[0][0], "end": fixture.nodes[1][0]})
    require(len(rows["rows"]) == 1, "staged edge CREATE incomplete")
    identity, start, end = rows["rows"][0]
    require(identity.startswith("r:") and start == fixture.nodes[0][0] and
            end == fixture.nodes[1][0], "staged edge endpoints")
    return rows, (0, (0, 0), identity)


def staged_and_history(db, observer, fixture, expected, pin):
    count_sql = "SELECT count(*) FROM main._lithograph_commits"
    committed_count = observer.execute(count_sql).fetchone()[0]
    begin = json.loads(db.execute("SELECT lithograph_tx_begin('{}')").fetchone()[0])
    require(begin["baseCommit"] == pin, "wrong explicit transaction base")
    try:
        created, extra = add_parallel(db, fixture)
        require(created["summary"]["commit"] is None, "staged edge exposed public Commit")
        staged_expected = fixture.expected(extra)
        require(len(staged_expected) > len(expected), "staged oracle did not change")
        for adapter in (execute, streamed):
            check_summary(adapter(db, fixture.query()), staged_expected, None)
        require(db.in_transaction and head(observer) == pin, "staged write published early")
        require(observer.execute(count_sql).fetchone()[0] == committed_count, "staged history leaked")
        check_summary(streamed(observer, fixture.query()), expected, pin)
    finally:
        if db.in_transaction:
            db.execute("SELECT lithograph_tx_abort()").fetchone()
    require(head(db) == pin and db.execute(count_sql).fetchone()[0] == committed_count,
            "abort retained private history")
    for adapter in (execute, streamed):
        check_summary(adapter(db, fixture.query()), expected, pin)
    _, extra = add_parallel(db, fixture)
    current = head(db)
    require(current != pin, "historical case requires a newer current Commit")
    for adapter in (execute, streamed):
        check_summary(adapter(db, fixture.query()), fixture.expected(extra), current)
        check_summary(adapter(db, fixture.query(), options={"at": pin}), expected, pin)
    require(head(db) == current, "historical read moved Branch")


def failed_write(db, fixture, pin):
    write = fixture.query().split(" RETURN ")[0] + \
        " SET n0.mark=1 SET n0.mark=1/0 FINISH"
    try:
        execute(db, write)
    except sqlite3.Error as error:
        require("LITHOGRAPH_TYPE_ERROR" in str(error), "cyclic write lost its original error")
    else:
        raise AssertionError("division by zero write succeeded")
    require(head(db) == pin, "failed cyclic write published a Commit")
    require(bag(execute(db, "MATCH(n:S8D0) RETURN elementId(n),n.mark")["rows"]) ==
            bag([[identity, None] for identity in fixture.nodes[0]]),
            "failed cyclic write published a partial property")


def case(extension, checkpoint, directory, shape, copies, dense=False):
    path = directory / (shape + ("-dense" if dense else "-circulant") + f"-{copies}.sqlite3")
    db = open_db(extension, path)
    try:
        fixture = OverlapFixture(db) if shape == "overlap" else Fixture(db, shape, copies, dense)
        expected = fixture.expected()
        if shape == "overlap":
            # 352 node tuples: 16 all-self tuples retain only 3P3=6 RID
            # permutations, while the other 336 retain all 3**3 combinations.
            require(len(expected) == (352-16) * 27 + 16 * 6 == 9168, "overlap full bag size")
            require(sum(row[0] == row[1] == row[2] for row in expected) == 96,
                    "same NodeId self-loop tuples disappeared")
            require(all(len(set(row[3:])) == 3 for row in expected), "oracle reused a MATCH RID")
        else:
            require(len(expected) ==
                    (32 ** (len(fixture.nodes)-1) if dense else EXPECTED_ROWS[shape]) * copies,
                    "unexpected public CREATE oracle size")
    finally:
        db.close()
    completed = subprocess.run([str(Path(checkpoint).resolve()), str(path)],
                               capture_output=True, text=True, timeout=60, check=True)
    maintenance = json.loads(completed.stdout)
    require(maintenance["checkpoint_count"] == 1, "missing complete checkpoint")
    db = open_db(extension, path)
    try:
        pin = head(db)
        require(maintenance["commit"] == pin, "checkpoint pin differs")
        complete_adapters(db, fixture, expected, pin, not dense)
        prefix_consumers(db, fixture.query(), expected)
        if shape == "triangle" and not dense:
            lifecycle(db, fixture.query(), expected)
            policy_and_errors(db, fixture, expected)
        if shape == "overlap":
            lifecycle(db, fixture.query(), expected)
        if shape == "triangle" and copies == 1 and not dense:
            repeated_owners(db, fixture, expected, pin)
            failed_write(db, fixture, pin)
            observer = open_db(extension, path, readonly=True)
            try:
                staged_and_history(db, observer, fixture, expected, pin)
            finally:
                observer.close()
        require(db.execute("PRAGMA integrity_check").fetchone() == ("ok",), "SQLite integrity")
        integrity = json.loads(db.execute("SELECT lithograph_integrity_check()").fetchone()[0])
        require(integrity["ok"], "canonical integrity")
    finally:
        db.close()
    return {"case": shape, "fixture": "dense" if dense else "circulant16-degree8",
            "first_edge_copies": copies, "rows": len(expected),
            "adapters": ["scalar", "rows"], "passed": True}


def run(extension, checkpoint):
    require(sqlite3.sqlite_version_info >= (3, 45, 0), "SQLite 3.45+ required")
    results = []
    with tempfile.TemporaryDirectory(prefix="lithograph-multiway-") as temporary:
        for shape, copies in [(name, 1) for name in SHAPES] + [("triangle", 2), ("overlap", 3)]:
            result = case(extension, checkpoint, Path(temporary), shape, copies)
            results.append(result)
            print(json.dumps(result), flush=True)
        result = case(extension, checkpoint, Path(temporary), "triangle", 1, dense=True)
        results.append(result)
        print(json.dumps(result), flush=True)
    print(json.dumps({"stage": "S22.8", "sqlite": sqlite3.sqlite_version, "cases": results}),
          flush=True)


def old_owner_overwrite_repro(extension, checkpoint):
    """An explicit old-artifact repro; unrelated failures never count as success."""
    require(sqlite3.sqlite_version_info >= (3, 45, 0), "SQLite 3.45+ required")
    with tempfile.TemporaryDirectory(prefix="lithograph-owner-overwrite-") as temporary:
        path = Path(temporary) / "triangle.sqlite3"
        db = open_db(extension, path)
        try:
            fixture = Fixture(db, "triangle", 1)
            expected = fixture.expected()
            require(len(expected) == 352, "unexpected owner-repro oracle")
        finally:
            db.close()
        completed = subprocess.run([str(Path(checkpoint).resolve()), str(path)],
                                   capture_output=True, text=True, timeout=60, check=True)
        maintenance = json.loads(completed.stdout)
        require(maintenance["checkpoint_count"] == 1, "missing owner-repro checkpoint")
        db = open_db(extension, path)
        try:
            pin = head(db)
            require(maintenance["commit"] == pin, "owner-repro checkpoint pin differs")
            for name, adapter in (("scalar", execute), ("rows", streamed)):
                observed = []
                for values, oracle in (("[1]", expected), ("[1,2]", expected + expected)):
                    query = "PROFILE UNWIND " + values + " AS tag " + fixture.query()
                    result = adapter(db, query)
                    check_summary(result, oracle, pin)
                    operators = result["summary"]["profile"]["operators"]
                    require(sum(operator["dbHits"] for operator in operators) ==
                            result["summary"]["metrics"]["dbHits"], "repro PROFILE sum differs")
                    selected = [op for op in operators if op["operator"] == "MultiwayIntersect"]
                    require(len(selected) == 1 and selected[0]["rows"] == len(oracle),
                            "repro must execute selected Multiway with the complete bag")
                    work = selected[0]["multiway"]
                    for key in (*MULTIWAY_EVENTS, "memoryPeakBytes"):
                        require(type(work[key]) is int and work[key] > 0,
                                "missing actual repro counter: " + key)
                    observed.append({"query": query, "outputRows": len(result["rows"]),
                                     "operator": selected[0]})
                first, second = [item["operator"]["multiway"] for item in observed]
                require(first["nodeAssignments"] == second["nodeAssignments"] == 352,
                        "old overwrite signature requires 704 output rows but 352 assignments")
                require(all(first[key] == second[key] for key in MULTIWAY_EVENTS),
                        "old overwrite signature must retain exactly the last owner's events")
                require(first["memoryPeakBytes"] == second["memoryPeakBytes"],
                        "repro owner shape differs")
                print(json.dumps({"mode": "old-owner-overwrite-repro", "surface": name,
                                  "sqlite": sqlite3.sqlite_version, "commit": pin,
                                  "observed": observed, "reproduced": True}), flush=True)
            require(head(db) == pin, "repro moved Branch")
            require(db.execute("PRAGMA integrity_check").fetchone() == ("ok",), "repro integrity")
        finally:
            db.close()
        print(json.dumps({"mode": "old-owner-overwrite-repro", "completed": True,
                          "surfaces": ["scalar", "rows"], "reproduced": True}), flush=True)


if __name__ == "__main__":
    require(len(sys.argv) == 3 or len(sys.argv) == 4 and sys.argv[3] == "--old-owner-overwrite-repro",
            "usage: phase22_multiway_sql.py <extension> <checkpoint-tool> [--old-owner-overwrite-repro]")
    if len(sys.argv) == 4:
        old_owner_overwrite_repro(sys.argv[1], sys.argv[2])
    else:
        run(sys.argv[1], sys.argv[2])
