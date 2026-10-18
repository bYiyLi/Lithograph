#!/usr/bin/env python3
"""在真实 SQLite SQL 接口验证 Phase 18（只使用标准库和临时数据）。"""

import argparse
from copy import deepcopy
from itertools import zip_longest
import json
from pathlib import Path
import re
import sqlite3
import tempfile


OMITTED = object()
COUNTERS = (
    "nodesCreated", "nodesDeleted", "relationshipsCreated", "relationshipsDeleted",
    "propertiesSet", "propertiesRemoved", "labelsAdded", "labelsRemoved",
    "constraintsAdded", "constraintsRemoved", "indexesAdded", "indexesRemoved",
)
RULES = {
    "denyProperties": ["secret"],
    "denyPropertyPrefixes": ["@private:"],
    "denyPropertyPatterns": ["audit_[0-9]+"],
}
MATCH_PATH = "MATCH p=(a:Person {name:'Alice'})-[r:KNOWS]->(b) "
GRAPH_QUERY = MATCH_PATH + "RETURN a AS a, r AS r, b AS b, p AS p"
ALICE_PROPERTIES = {
    "name": "Alice", "secret": "alice-secret", "@private:key": "a-key",
    "audit_7": "a-audit", "x_audit_7": "a-safe", "Secret": "a-case",
}
BOB_PROPERTIES = {
    "name": "Bob", "secret": "bob-secret", "@private:key": "b-key",
    "audit_7": "b-audit", "x_audit_7": "b-safe", "Secret": "b-case",
}
EDGE_PROPERTIES = {
    "name": "edge", "secret": "edge-secret", "@private:key": "edge-key",
    "audit_7": "edge-audit", "x_audit_7": "edge-safe", "Secret": "edge-case",
}


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def equal(actual, expected, context):
    if actual != expected:
        raise AssertionError(
            f"{context}\nexpected: {json.dumps(expected, ensure_ascii=False)}"
            f"\nactual:   {json.dumps(actual, ensure_ascii=False)}")


def public_properties(properties):
    """Explicit fixture expectation; verify regex anchors and case sensitivity."""
    return {key: properties[key] for key in ("name", "x_audit_7", "Secret")}


class Graph:
    def __init__(self, extension, surface):
        self.directory = tempfile.TemporaryDirectory(prefix="lithograph-phase18-sql-")
        self.surface = surface
        self.extension = extension
        self.database = Path(self.directory.name) / "graph.sqlite"
        self.db = sqlite3.connect(self.database, isolation_level=None)
        try:
            self.db.enable_load_extension(True)
            try:
                self.db.load_extension(str(extension))
            finally:
                self.db.enable_load_extension(False)
            self.db.execute("CREATE VIRTUAL TABLE temp.phase18_fts5_probe USING fts5(text)")
            self.db.execute("DROP TABLE temp.phase18_fts5_probe")
            self.sql_json("SELECT lithograph_init()")
            self.query(
                "CREATE (a:Person {name:'Alice', secret:'alice-secret', "
                "`@private:key`:'a-key', audit_7:'a-audit', x_audit_7:'a-safe', Secret:'a-case'}), "
                "(b:Person {name:'Bob', secret:'bob-secret', `@private:key`:'b-key', "
                "audit_7:'b-audit', x_audit_7:'b-safe', Secret:'b-case'}), "
                "(a)-[:KNOWS {name:'edge', secret:'edge-secret', `@private:key`:'edge-key', "
                "audit_7:'edge-audit', x_audit_7:'edge-safe', Secret:'edge-case'}]->(b) FINISH")
            self.base = self.head()
            baseline = self.query(GRAPH_QUERY)
            self.a, self.r, self.b, self.p = baseline["rows"][0]
            self.check_fixture()
        except Exception:
            self.close()
            raise

    def check_fixture(self):
        for node, properties in ((self.a, ALICE_PROPERTIES), (self.b, BOB_PROPERTIES)):
            require(re.fullmatch(r"n:[1-9][0-9]*", node["elementId"]), "Node ID shape")
            equal(node, {"$type": "Node", "elementId": node["elementId"],
                         "labels": ["Person"], "properties": properties}, "fixture Node")
        require(self.a["elementId"] != self.b["elementId"], "distinct fixture Nodes")
        require(re.fullmatch(r"r:[1-9][0-9]*", self.r["elementId"]), "Relationship ID shape")
        equal(self.r, {"$type": "Relationship", "elementId": self.r["elementId"],
                       "start": self.a["elementId"], "end": self.b["elementId"],
                       "type": "KNOWS", "properties": EDGE_PROPERTIES}, "fixture Relationship")
        equal(self.p, {"$type": "Path", "nodes": [self.a, self.b],
                       "relationships": [self.r]}, "fixture Path")

    def sql_json(self, sql, parameters=()):
        cursor = self.db.execute(sql, parameters)
        try:
            row = cursor.fetchone()
            require(row is not None, "SQL returned no JSON result")
            return json.loads(row[0])
        finally:
            cursor.close()

    def query(self, cypher, params=OMITTED, options=OMITTED):
        arguments = [cypher]
        if params is not OMITTED or options is not OMITTED:
            arguments.append(json.dumps({} if params is OMITTED else params,
                                        ensure_ascii=False, allow_nan=False))
        if options is not OMITTED:
            arguments.append(json.dumps(options, ensure_ascii=False, allow_nan=False))
        placeholders = ",".join("?" for _ in arguments)
        if self.surface == "scalar":
            result = self.sql_json(f"SELECT lithograph({placeholders})", arguments)
        else:
            cursor = self.db.execute(
                f"SELECT ordinal,event,data FROM lithograph_rows({placeholders})", arguments)
            try:
                events = [(ordinal, event, json.loads(data))
                          for ordinal, event, data in cursor]
            finally:
                cursor.close()
            require(len(events) >= 2, "rows needs columns and terminal summary")
            equal([entry[0] for entry in events], list(range(len(events))), "rows ordinals")
            equal(events[0][1], "columns", "first rows event")
            equal(events[-1][1], "summary", "terminal rows event")
            equal([entry[1] for entry in events[1:-1]],
                  ["row"] * (len(events) - 2), "middle rows events")
            result = {"columns": events[0][2], "rows": [entry[2] for entry in events[1:-1]],
                      "summary": events[-1][2]}
        require(isinstance(result["columns"], list), "columns is a list")
        require(all(isinstance(name, str) for name in result["columns"]), "column names")
        require(all(len(row) == len(result["columns"]) for row in result["rows"]), "row widths")
        summary = result["summary"]
        require({"commit", "queryType", "counters", "metrics"} <= summary.keys(), "summary shape")
        equal(sorted(summary["counters"]), sorted(COUNTERS), "closed counter inventory")
        require(all(isinstance(value, int) and value >= 0
                    for value in summary["counters"].values()), "nonnegative counters")
        require(summary["queryType"] in {"read", "write", "schema", "version", "mixed"},
                "query type")
        return result

    def read(self, cypher, columns, rows, options=OMITTED, params=OMITTED, commit=OMITTED):
        result = self.query(cypher, params, options)
        equal(result["columns"], columns, "read columns")
        equal(result["rows"], rows, "read rows")
        equal(result["summary"]["queryType"], "read", "read query type")
        equal(result["summary"]["commit"], self.head() if commit is OMITTED else commit,
              "read Commit")
        equal(result["summary"]["counters"], dict.fromkeys(COUNTERS, 0), "read counters")
        return result

    def head(self):
        result = self.query("CALL lithograph.branch.list() YIELD name, commit "
                            "WHERE name='main' RETURN commit")
        equal(result["columns"], ["commit"], "Branch columns")
        require(len(result["rows"]) == 1, "one main Branch")
        head = result["rows"][0][0]
        require(re.fullmatch(r"commit/[0-9a-f]{64}", head), "Commit descriptor shape")
        return head

    def commit_count(self):
        return self.db.execute("SELECT count(*) FROM main._lithograph_commits").fetchone()[0]

    def count(self, label):
        return self.query(f"MATCH (n:{label}) RETURN count(n) AS c")["rows"][0][0]

    def error(self, cypher, options, category="ACCESS_DENIED", params=OMITTED):
        try:
            self.query(cypher, params, options)
        except sqlite3.Error as error:
            require(f"LITHOGRAPH_{category}:" in str(error),
                    f"wrong error category: {error}")
            if category == "ACCESS_DENIED":
                equal(error.sqlite_errorcode & 0xFF, sqlite3.SQLITE_AUTH, "SQLite AUTH mapping")
                for secret in ("alice-secret", "bob-secret", "edge-secret", "a-key"):
                    require(secret not in str(error), "permission error exposed a protected value")
            else:
                require(error.sqlite_errorcode & 0xFF != sqlite3.SQLITE_AUTH,
                        "configuration error must not map to AUTH")
            return
        raise AssertionError(f"{category} expected for {cypher}")

    def filtered_graph(self):
        a, r, b = deepcopy((self.a, self.r, self.b))
        for element in (a, r, b):
            element["properties"] = public_properties(element["properties"])
        return a, r, b, {"$type": "Path", "nodes": [a, b], "relationships": [r]}

    def close(self):
        self.db.close()
        self.directory.cleanup()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


def defaults_and_configuration(graph):
    for options in (OMITTED, {}, {"readAccess": {}, "writeAccess": {}},
                    {"readAccess": {"query": {}, "result": {}, "denyFunctions": []},
                     "writeAccess": {key: [] for key in RULES}}):
        graph.read(GRAPH_QUERY, ["a", "r", "b", "p"],
                   [[graph.a, graph.r, graph.b, graph.p]], options)
    for options in (
            {"readAccess": None}, {"writeAccess": []},
            {"readAccess": {"unknown": True}},
            {"readAccess": {"query": {"unknown": []}}},
            {"readAccess": {"allowShow": 1}},
            {"readAccess": {"denyFunctions": [""]}},
            {"readAccess": {"result": {"denyProperties": [""]}}},
            {"readAccess": {"query": {"denyPropertyPrefixes": [""]}}},
            {"writeAccess": {"denyPropertyPatterns": ["["]}},
            {"writeAccess": {"denyProperties": "secret"}}):
        graph.error("RETURN 1 AS x", options, "INVALID_ARGUMENT")
    equal(graph.head(), graph.base, "invalid configuration preserved Branch")
    before = graph.commit_count()
    for index, options in enumerate((OMITTED, {}, {"readAccess": {}, "writeAccess": {}})):
        result = graph.query("CREATE (:Unrestricted {name:$name}) RETURN $name AS name",
                             {"name": f"default-{index}"}, options)
        equal(result["rows"], [[f"default-{index}"]], "empty configuration allowed writes")
        equal(result["summary"]["counters"]["nodesCreated"], 1, "default create counter")
        require(result["summary"]["commit"] != graph.base, "default write published Commit")
    equal(graph.count("Unrestricted"), 3, "all default mutations persisted")
    equal(graph.commit_count(), before + 3, "default mutations retained one Commit each")


def result_encoding(graph):
    a, r, b, path = graph.filtered_graph()
    options = {"readAccess": {"result": RULES}}
    graph.read(GRAPH_QUERY, ["a", "r", "b", "p"], [[a, r, b, path]], options)
    graph.read(MATCH_PATH +
               "RETURN {secret:'literal', node:a, edge:r, path:p, "
               "items:[b, {secret:'nested-literal', path:p}]} AS nested",
               ["nested"], [[{"secret": "literal", "node": a, "edge": r, "path": path,
                              "items": [b, {"secret": "nested-literal", "path": path}]}]], options)
    graph.read("MATCH (a:Person {name:'Alice'}) "
               "RETURN {`$type`:'Node', properties:{secret:'literal'}, nested:a} AS ordinary",
               ["ordinary"], [[{"$type": "Map", "entries": {
                   "$type": "Node", "properties": {"secret": "literal"}, "nested": a}}]], options)
    graph.read("MATCH (a:Person {name:'Alice'}) RETURN a.secret AS secret, "
               "properties(a) AS props, a{.*} AS projection, "
               "{secret:'literal', `@private:key`:'literal-key', audit_7:'literal-audit'} AS ordinary",
               ["secret", "props", "projection", "ordinary"],
               [["alice-secret", ALICE_PROPERTIES, ALICE_PROPERTIES,
                 {"secret": "literal", "@private:key": "literal-key", "audit_7": "literal-audit"}]],
               options)
    graph.read("MATCH (a:Person) WHERE a.secret='alice-secret' "
               "RETURN a AS a, a.secret + '-suffix' AS derived",
               ["a", "derived"], [[a, "alice-secret-suffix"]], options)
    graph.read("MATCH (a:Person {name:'Alice'}) RETURN elementId(a) AS id, labels(a) AS labels",
               ["id", "labels"], [[a["elementId"], ["Person"]]], options)
    graph.read("MATCH (a:Person) RETURN a AS a ORDER BY a.secret DESC", ["a"], [[b], [a]], options)
    graph.read("MATCH (a:Person) RETURN DISTINCT a.secret AS secret ORDER BY secret",
               ["secret"], [["alice-secret"], ["bob-secret"]], options)
    graph.read(GRAPH_QUERY, ["a", "r", "b", "p"], [[a, r, b, path]],
               {"readAccess": {"result": RULES, "denyFunctions": ["elementId", "id"]}})
    graph.read(GRAPH_QUERY, ["a", "r", "b", "p"], [[graph.a, graph.r, graph.b, graph.p]])


def query_intersection(graph):
    a, r, b, path = graph.filtered_graph()
    options = {"readAccess": {"query": RULES, "result": {"denyProperties": ["Secret"]}}}
    for element in (a, r, b):
        del element["properties"]["Secret"]
    graph.read(GRAPH_QUERY, ["a", "r", "b", "p"], [[a, r, b, path]], options)
    graph.read("MATCH (a:Person {name:'Alice'}) RETURN a.secret AS direct, a[$key] AS dynamic, "
               "properties(a) AS props, a{.*} AS projection, a.Secret AS caseSensitive, "
               "a.x_audit_7 AS fullMatch, {secret:'ordinary'} AS ordinary",
               ["direct", "dynamic", "props", "projection", "caseSensitive", "fullMatch", "ordinary"],
               [[None, None, public_properties(ALICE_PROPERTIES), public_properties(ALICE_PROPERTIES),
                 "a-case", "a-safe", {"secret": "ordinary"}]], options, {"key": "secret"})
    graph.read("MATCH (a:Person) WHERE a.secret IS NULL RETURN a.name AS name ORDER BY name",
               ["name"], [["Alice"], ["Bob"]], options)
    graph.read("MATCH (a:Person) WHERE a.secret='alice-secret' RETURN a.name AS name",
               ["name"], [], options)
    graph.read("MATCH (a:Person) RETURN count(a.secret) AS present, collect(a.secret) AS secrets",
               ["present", "secrets"], [[0, []]], options)
    graph.read("MATCH (a:Person {name:'Alice'}) RETURN keys(a) AS keys",
               ["keys"], [[sorted(public_properties(ALICE_PROPERTIES))]], options)
    graph.read("MATCH (a:Person {name:'Alice'}) CALL (a) { RETURN a.secret AS x } RETURN x",
               ["x"], [[None]], options)


def capability_errors(graph):
    before = graph.commit_count()
    cases = (
        ("MATCH (a:Person) RETURN elementId(a) AS id",
         {"readAccess": {"denyFunctions": ["elementId"]}}),
        ("CALL db.labels()", {"readAccess": {"allowNamedProcedures": False}}),
        ("SHOW FUNCTIONS", {"readAccess": {"allowShow": False}}),
        ("EXPLAIN RETURN 1 AS x", {"readAccess": {"allowExplain": False}}),
        ("PROFILE RETURN 1 AS x", {"readAccess": {"allowProfile": False}}),
        ("CREATE (:DeniedCreate) FINISH", {"writeAccess": {"allowCreate": False}}),
    )
    for cypher, options in cases:
        graph.error(cypher, options)
    equal(graph.commit_count(), before, "capability failures added no Commit")
    equal(graph.head(), graph.base, "capability failures preserved Branch")
    equal(graph.count("DeniedCreate"), 0, "capability failure created no Node")
    graph.error("CREATE (:OldMode) FINISH",
                {"readOnlyNoExternalIO": True, "writeAccess": {"allowCreate": True}},
                "INVALID_ARGUMENT")
    equal(graph.count("OldMode"), 0, "new allow did not bypass old read-only mode")


def write_rollback(graph):
    before = graph.commit_count()
    for cypher in (
            "MATCH (a:Person {name:'Alice'}) SET a.secret='changed' RETURN a",
            "MATCH (a:Person {name:'Alice'}) SET a.secret=a.secret RETURN a",
            "MATCH (a:Person {name:'Alice'}) SET a[$key]=null RETURN a",
            "MATCH (a:Person {name:'Alice'}) SET a={name:'Alice'} RETURN a",
            "CREATE (:Partial) WITH 1 AS ignored MATCH (a:Person {name:'Alice'}) "
            "SET a.secret='changed' FINISH"):
        graph.error(cypher, {"writeAccess": {"denyProperties": ["secret"]}},
                    params={"key": "secret"})
        equal(graph.commit_count(), before, "rejected write added no Commit")
        equal(graph.head(), graph.base, "rejected write moved Branch")
        equal(graph.count("Partial"), 0, "rejected write left partial Node")
    graph.read(GRAPH_QUERY, ["a", "r", "b", "p"], [[graph.a, graph.r, graph.b, graph.p]])


def explicit_policy_changes(graph):
    before = graph.commit_count()
    begin = graph.sql_json("SELECT lithograph_tx_begin('{}')")
    equal(begin, {"baseCommit": graph.base}, "explicit begin anchor")
    changed = graph.query("MATCH (a:Person {name:'Alice'}) SET a.secret='trusted-staged' "
                          "RETURN a.secret AS secret")
    equal(changed["rows"], [["trusted-staged"]], "trusted staged write")
    equal(changed["summary"]["commit"], None, "staged write has no Commit")
    graph.read("MATCH (a:Person {name:'Alice'}) RETURN a.secret AS secret", ["secret"], [[None]],
               {"readAccess": {"query": {"denyProperties": ["secret"]}}}, commit=None)
    graph.read("MATCH (a:Person {name:'Alice'}) RETURN a.secret AS secret", ["secret"],
               [["trusted-staged"]], commit=None)
    allowed = graph.query("MATCH (a:Person {name:'Alice'}) SET a.x_audit_7='staged-public' "
                          "RETURN a.secret AS secret, a.x_audit_7 AS public",
                          options={"writeAccess": {"denyProperties": ["secret"]}})
    equal(allowed["rows"], [["trusted-staged", "staged-public"]], "per-execution staged baseline")
    equal(allowed["summary"]["commit"], None, "second staged write has no Commit")
    committed = graph.sql_json("SELECT lithograph_tx_commit()")
    require(committed["commit"] != graph.base, "explicit commit published a new version")
    equal(graph.head(), committed["commit"], "explicit commit moved Branch once")
    equal(graph.commit_count(), before + 1, "one combined explicit Commit")
    graph.read("MATCH (a:Person {name:'Alice'}) RETURN a.secret AS secret, a.x_audit_7 AS public",
               ["secret", "public"], [["trusted-staged", "staged-public"]])


def explicit_denial_aborts(graph):
    before = graph.commit_count()
    graph.sql_json("SELECT lithograph_tx_begin('{}')")
    staged = graph.query("CREATE (:StagedBeforeDenial {name:'staged'}) FINISH")
    equal(staged["summary"]["commit"], None, "unpublished staged creation")
    graph.error("MATCH (a:Person {name:'Alice'}) SET a.secret=a.secret FINISH",
                {"writeAccess": {"denyProperties": ["secret"]}})
    require(not graph.db.in_transaction, "access denial automatically aborted explicit transaction")
    equal(graph.count("StagedBeforeDenial"), 0, "explicit denial rolled back prior staged data")
    equal(graph.head(), graph.base, "explicit denial preserved Branch")
    equal(graph.commit_count(), before, "explicit denial added no Commit")
    for function in ("lithograph_tx_commit", "lithograph_tx_abort"):
        try:
            graph.sql_json(f"SELECT {function}()")
        except sqlite3.Error as error:
            require("LITHOGRAPH_INVALID_ARGUMENT:" in str(error), "no active transaction error")
        else:
            raise AssertionError("denied transaction must require a fresh begin")


def caller_owned_rollback(graph):
    before = graph.commit_count()
    graph.db.execute("BEGIN IMMEDIATE")
    staged = graph.query("CREATE (:CallerStaged {name:'allowed'}) FINISH")
    require(staged["summary"]["commit"] != graph.base, "caller-owned write has own Commit")
    equal(graph.commit_count(), before + 1, "caller-owned first invocation added one Commit")
    graph.error("CREATE (:CallerPartial) WITH 1 AS ignored MATCH (a:Person {name:'Alice'}) "
                "SET a.secret='changed' FINISH", {"writeAccess": {"denyProperties": ["secret"]}})
    require(graph.db.in_transaction, "invocation denial preserved caller-owned transaction")
    equal(graph.count("CallerStaged"), 1, "denial preserved earlier caller-owned invocation")
    equal(graph.count("CallerPartial"), 0, "denied invocation rolled back partial graph")
    equal(graph.head(), staged["summary"]["commit"], "denial preserved caller-owned Branch")
    equal(graph.commit_count(), before + 1, "denial added no caller-owned Commit")
    graph.db.execute("ROLLBACK")
    equal(graph.head(), graph.base, "host rollback restored original Branch")
    equal(graph.commit_count(), before, "host rollback removed unpublished history")
    equal(graph.count("CallerStaged"), 0, "host rollback removed earlier caller-owned write")
    graph.read(GRAPH_QUERY, ["a", "r", "b", "p"], [[graph.a, graph.r, graph.b, graph.p]])




def output_policy():
    return {"readAccess": {"query": {"denyProperties": ["secret"]},
                           "result": {"denyProperties": ["privateResult"]}},
            "writeAccess": {"denyProperties": ["secret"]}}


def graph_stream(database, options):
    return database.execute(
        "SELECT ordinal,event,data FROM lithograph_rows(?,?,?)",
        ("MATCH (n:Many) RETURN n AS n ORDER BY n.i", "{}", json.dumps(options)))


def assert_interleaved_streams(restricted, unrestricted):
    missing = object()
    counts = [0, 0]
    summaries = [0, 0]
    try:
        for expected_ordinal, pair in enumerate(zip_longest(
                restricted, unrestricted, fillvalue=missing)):
            for index, event in enumerate(pair):
                require(event is not missing, "interleaved streams have the same event count")
                ordinal, kind, data = event
                equal(ordinal, expected_ordinal, "interleaved stream ordinal")
                value = json.loads(data)
                if kind == "row":
                    counts[index] += 1
                    properties = {"i": counts[index], "public": "visible"}
                    if index == 1:
                        properties.update(secret="private", privateResult="result-private")
                    equal(value[0]["properties"], properties,
                          "each cursor retains its own query/result policy across prefetch")
                elif kind == "summary":
                    summaries[index] += 1
                else:
                    equal(kind, "columns", "only columns, row, summary events")
                    equal(ordinal, 0, "columns occur only at the start")
        equal(counts, [600, 600], "both streams cross the 256-row prefetch boundary")
        equal(summaries, [1, 1], "both interleaved streams reach terminal success")
    finally:
        restricted.close()
        unrestricted.close()


def streaming_policy_isolation(graph):
    graph.query("UNWIND range(1,600) AS i CREATE (:Many {i:i,public:'visible',"
                "secret:'private',privateResult:'result-private'}) FINISH")
    assert_interleaved_streams(graph_stream(graph.db, output_policy()),
                               graph_stream(graph.db, {}))
    second = sqlite3.connect(graph.database, isolation_level=None)
    try:
        second.enable_load_extension(True)
        try:
            second.load_extension(str(graph.extension))
        finally:
            second.enable_load_extension(False)
        assert_interleaved_streams(graph_stream(graph.db, output_policy()),
                                   graph_stream(second, {}))
    finally:
        second.close()
    graph.read("MATCH (n:Many) RETURN count(n),count(n.secret),count(n.privateResult)",
               ["count(n)", "count(n.secret)", "count(n.privateResult)"], [[600, 600, 600]])


def controlled_stream_early_close(graph):
    graph.query("MATCH (n:Person) SET n.privateResult='result-private' FINISH")
    before = (graph.head(), graph.commit_count())
    options = output_policy()
    cursor = graph.db.execute(
        "SELECT event,data FROM lithograph_rows(?,?,?)",
        ("MATCH (n:Person) SET n.open=1 RETURN n AS n", "{}", json.dumps(options)))
    try:
        equal(cursor.fetchone()[0], "columns", "controlled stream starts with columns")
        kind, data = cursor.fetchone()
        equal(kind, "row", "controlled write produced its first row before close")
        node = json.loads(data)[0]
        require("secret" not in node["properties"], "query rule filters early output")
        require("privateResult" not in node["properties"], "result rule filters early output")
        equal(node["properties"]["open"], 1, "first output observes the staged write")
    finally:
        cursor.close()
    equal((graph.head(), graph.commit_count()), before,
          "early close with active policy does not publish a Commit or move Branch")
    graph.read("MATCH (n:Person) RETURN n.open", ["n.open"], [[None], [None]])
    # The same cursor lifecycle must fail closed when earlier work is explicitly staged.
    graph.sql_json("SELECT lithograph_tx_begin('{}')")
    graph.query("CREATE (:BeforeControlledClose {value:1}) FINISH")
    cursor = graph.db.execute(
        "SELECT event,data FROM lithograph_rows(?,?,?)",
        ("MATCH (n:Person) SET n.open=2 RETURN n AS n", "{}", json.dumps(options)))
    try:
        equal(cursor.fetchone()[0], "columns", "explicit controlled stream columns")
        equal(json.loads(cursor.fetchone()[1])[0]["properties"]["open"], 2,
              "explicit controlled stream exposes a staged permitted write")
    finally:
        cursor.close()
    require(not graph.db.in_transaction, "early close aborted the explicit transaction")
    equal(graph.count("BeforeControlledClose"), 0, "early close discarded earlier staged work")
    equal((graph.head(), graph.commit_count()), before, "explicit early-close history unchanged")


def controlled_result_length_failure(graph):
    graph.query("MATCH (n:Person) SET n.payload=$payload, "
                "n.privateResult='result-private' FINISH", {"payload": "x" * 300})
    query = "MATCH (n:Person) SET n.open=1 RETURN [n,n,n,n,n] AS nodes"
    options = output_policy()
    # Under the normal limit the same allowed write and graph encoding must succeed.
    ordinary = graph.query(query, options=options)
    equal(ordinary["summary"]["counters"]["propertiesSet"], 2,
          "control query actually modifies both matching Nodes")
    equal(ordinary["rows"][0][0][0]["properties"]["open"], 1,
          "control output materializes the updated graph objects")
    graph.query("MATCH (n:Person) REMOVE n.open FINISH")
    before = (graph.head(), graph.commit_count())
    # Query/options and each source property are below this limit; the repeated encoded
    # graph objects exceed it after the permitted write. The failure is not access denial.
    old_limit = graph.db.setlimit(sqlite3.SQLITE_LIMIT_LENGTH, 1000)
    try:
        try:
            graph.query(query, options=options)
        except sqlite3.Error as error:
            equal(error.sqlite_errorcode & 0xFF, sqlite3.SQLITE_TOOBIG,
                  "restricted output fails at the SQLite result-length boundary")
        else:
            raise AssertionError("controlled result must exceed SQLITE_LIMIT_LENGTH")
    finally:
        graph.db.setlimit(sqlite3.SQLITE_LIMIT_LENGTH, old_limit)
    equal((graph.head(), graph.commit_count()), before,
          "controlled encoding failure did not publish a Commit or move Branch")
    graph.read("MATCH (n:Person) RETURN n.open", ["n.open"], [[None], [None]])
    equal(graph.count("Person"), 2, "length failure preserved canonical Nodes")


def batch_graph_outputs(graph):
    policy = {"readAccess": output_policy()["readAccess"]}
    for index, boundary in enumerate((
            "TRANSACTIONS OF 128 ROWS",
            "2 CONCURRENT TRANSACTIONS OF 128 ROWS DISJOINT BY (i)")):
        label = f"BatchGraph{index}"
        before = graph.commit_count()
        result = graph.query(
            f"UNWIND range(1,600) AS i CALL(i) {{ CREATE(n:{label} "
            "{open:i,secret:'hidden',privateResult:'result-private'}) RETURN n } IN "
            + boundary + " RETURN n AS n ORDER BY n.open", options=policy)
        equal(len(result["rows"]), 600, "batch graph results cross the prefetch boundary")
        for number, row in enumerate(result["rows"], start=1):
            equal(row[0]["properties"], {"open": number},
                  "query and result rules apply to every independently committed batch")
        equal(graph.commit_count(), before + 5, "600 rows in 128-row batches form five Commits")
        equal(result["summary"]["commit"], graph.head(), "batch summary points to final head")
        equal(graph.query(f"MATCH(n:{label}) RETURN count(n),count(n.secret),"
                          "count(n.privateResult)")["rows"], [[600, 600, 600]],
              "batch filtering never removes canonical properties")


def graph_view_access_intersection(graph):
    options = output_policy()
    options["graphView"] = {"requireAllLabels": ["FreshVisible"]}
    graph.query("CREATE (:FreshVisible {public:'visible',secret:'hidden',open:1,"
                "privateResult:'result-private'}),"
                "(:Invisible {public:'outside',secret:'real',open:2}) FINISH")
    result = graph.query("MATCH (n) RETURN n.open,n.secret,n ORDER BY n.open", options=options)
    equal(len(result["rows"]), 1, "property rules do not broaden Graph View visibility")
    equal(result["rows"][0][:2], [1, None], "visible Node still uses masked property reads")
    equal(result["rows"][0][2]["properties"], {"public": "visible", "open": 1},
          "Graph View and output restrictions intersect")
    graph.query("MATCH (n) SET n.open=3 FINISH", options=options)
    equal(graph.query("MATCH(n:Invisible) RETURN n.open")["rows"], [[2]],
          "controlled write does not target a Node outside the view")
    equal(graph.query("MATCH(n:FreshVisible) RETURN n.open")["rows"], [[3]],
          "allowed visible write succeeds")
    before = (graph.head(), graph.commit_count())
    graph.error("MATCH (n) SET n.secret='denied' FINISH", options)
    equal((graph.head(), graph.commit_count()), before, "protected visible write rolls back")
    dynamic = deepcopy(options)
    dynamic["graphView"] = {"requireAllLabels": ["BrandNewLabel"]}
    result = graph.query(
        "CREATE (n:BrandNewLabel {open:4}) WITH n CALL (n) { "
        "MATCH(m:BrandNewLabel) RETURN count(m) AS c } RETURN c,n", options=dynamic)
    equal(result["rows"][0][0], 1, "selector resolves a Label first created in this execution")
    equal(result["rows"][0][1]["properties"], {"open": 4}, "staged object materialization")
    before = (graph.head(), graph.commit_count())
    graph.error("MATCH(n) REMOVE n:BrandNewLabel FINISH", dynamic, "GRAPH_VIEW_VIOLATION")
    equal((graph.head(), graph.commit_count()), before,
          "access options do not weaken required-Label mutation validation")
    graph.sql_json("SELECT lithograph_tx_begin('{}')")
    graph.query("CREATE (:TransactionVisible {open:5,secret:'staged',"
                "privateResult:'result-private'}) FINISH")
    staged = deepcopy(options)
    staged["graphView"] = {"requireAllLabels": ["TransactionVisible"]}
    result = graph.query("MATCH (n) RETURN n.open,n.secret,n", options=staged)
    equal(result["rows"][0][:2], [5, None], "staged selector uses this execution's query rules")
    equal(result["rows"][0][2]["properties"], {"open": 5}, "staged result restriction")
    graph.query("MATCH(n) SET n.open=6 FINISH", options=staged)
    equal(graph.query("MATCH(n:TransactionVisible) RETURN n.open")["rows"], [[6]],
          "allowed staged write remains visible to later execution")
    graph.sql_json("SELECT lithograph_tx_abort()")
    equal(graph.count("TransactionVisible"), 0, "abort discards the controlled staged write")
    equal((graph.head(), graph.commit_count()), before, "staged view work publishes no history")


def interpolated_lifecycle_rejection_aborts_explicit_work(graph):
    baseline = graph.query(
        'RETURN s"{COUNT { CALL lithograph.branch.create(\'interpolation_baseline\') '
        'YIELD name RETURN name }}" AS count')
    equal(baseline["rows"], [["1"]], "ordinary interpolation performs its Version operation")
    equal(graph.query("CALL lithograph.branch.list() YIELD name,commit "
                      "WHERE name='interpolation_baseline' RETURN commit")["rows"], [[graph.base]],
          "ordinary interpolation created the real Branch reference")
    for body in (
            "CALL db.index.semantic.rebuild('missing','branch/main') YIELD name RETURN name",
            "CALL lithograph.branch.list() YIELD name RETURN name",
            "CALL lithograph.branch.create('must_not_exist') YIELD name RETURN name"):
        before = (graph.head(), graph.commit_count())
        graph.sql_json("SELECT lithograph_tx_begin('{}')")
        staged = graph.query("CREATE (:BeforeInterpolatedLifecycleDenial {value:1}) FINISH")
        equal(staged["summary"]["commit"], None, "work is staged before lifecycle rejection")
        graph.error(f'RETURN s"{{COUNT {{ {body} }}}}"', {}, "TRANSACTION_BOUNDARY_REQUIRED")
        require(not graph.db.in_transaction, "interpolated lifecycle denial aborted explicit work")
        equal(graph.count("BeforeInterpolatedLifecycleDenial"), 0,
              "lifecycle denial discarded earlier staged graph changes")
        equal((graph.head(), graph.commit_count()), before,
              "lifecycle denial did not publish a Commit or move Branch")
        equal(graph.query("CALL lithograph.branch.list() YIELD name "
                          "WHERE name='must_not_exist' RETURN name")["rows"], [],
              "forbidden interpolated Version procedure never ran")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extension", type=Path, help="新构建的可信扩展文件")
    parser.add_argument("--surface", choices=("scalar", "rows", "both"), default="both")
    args = parser.parse_args()
    extension = args.extension.expanduser().resolve(strict=True)
    require(sqlite3.sqlite_version_info >= (3, 45, 0), "Python SQLite needs version 3.45+")
    require(hasattr(sqlite3.Connection, "enable_load_extension"), "Python needs extension loading")
    cases = (defaults_and_configuration, result_encoding, query_intersection, capability_errors,
             write_rollback, explicit_policy_changes, explicit_denial_aborts, caller_owned_rollback,
             streaming_policy_isolation, controlled_stream_early_close,
             controlled_result_length_failure, batch_graph_outputs, graph_view_access_intersection,
             interpolated_lifecycle_rejection_aborts_explicit_work)
    surfaces = ("scalar", "rows") if args.surface == "both" else (args.surface,)
    for surface in surfaces:
        for case in cases:
            with Graph(extension, surface) as graph:
                case(graph)
                integrity = graph.sql_json("SELECT lithograph_integrity_check()")
                equal(integrity["ok"], True, "post-case canonical integrity")
            print(f"PASS: Phase 18 {surface} {case.__name__}", flush=True)
    print(f"PASS: Phase 18 real SQLite SQL access controls ({len(cases) * len(surfaces)} cases)")


if __name__ == "__main__":
    main()
