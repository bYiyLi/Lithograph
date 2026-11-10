#!/usr/bin/env python3
"""Phase 21: exercise actual SQLite scalar/row adapters and versioned seek.

All state is synthetic and scoped to an in-memory SQLite connection. The
separate benchmark uses larger reproducible fixtures; this is a correctness
and transaction-regression gate rather than a benchmark.
"""

from __future__ import annotations

import json
import sqlite3
import sys
from pathlib import Path


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def execute(db: sqlite3.Connection, query: str, params=None, options=None) -> dict:
    raw = db.execute(
        "SELECT lithograph(?,?,?)",
        (query, json.dumps(params or {}), json.dumps(options or {})),
    ).fetchone()[0]
    return json.loads(raw)


def rows(db: sqlite3.Connection, query: str, params=None, options=None) -> list:
    events = db.execute(
        "SELECT event,data FROM lithograph_rows(?,?,?) ORDER BY ordinal",
        (query, json.dumps(params or {}), json.dumps(options or {})),
    ).fetchall()
    require(events and events[-1][0] == "summary", "row stream did not complete")
    return [json.loads(data) for event, data in events if event == "row"]


def run(extension: str) -> None:
    require(sqlite3.sqlite_version_info >= (3, 45, 0), "SQLite 3.45+ required")
    db = sqlite3.connect(":memory:")
    db.enable_load_extension(True)
    db.load_extension(str(Path(extension).resolve()))
    db.enable_load_extension(False)
    db.execute("SELECT lithograph_init()").fetchone()
    try:
        created = execute(
            db,
            "CREATE "
            + ", ".join(f"(:Probe {{k:{index}}})" for index in range(80))
            + " FINISH",
        )
        historical_commit = created["summary"]["commit"]
        query = (
            "MATCH (a:Probe), (b:Probe) "
            "WHERE elementId(a)=$a AND elementId(b)=$b "
            "RETURN a.k AS a,b.k AS b"
        )
        params = {"a": "n:1", "b": "n:2"}
        explained = execute(db, "EXPLAIN " + query, params)["rows"][0][0]
        require(explained.count("NodeIdSeek") == 2, "both Node IDs must seek")
        require(
            explained.index("Filter") < explained.index("Cartesian"),
            "first predicate must run before the next Pattern Part",
        )
        profiled = execute(db, "PROFILE " + query, params)
        require(profiled["rows"] == [[0, 1]], "profile changed match semantics")
        require(
            profiled["summary"]["metrics"]["dbHits"] <= 12,
            "Node ID access scanned unrelated nodes",
        )
        require(
            execute(db, query, params)["rows"] == rows(db, query, params) == [[0, 1]],
            "scalar/row paths disagree",
        )
        require(
            execute(db, query.replace("elementId(a)=$a", "$a=elementId(a)"), params)[
                "rows"
            ]
            == [[0, 1]],
            "reversed equality broke seek",
        )
        require(
            execute(
                db,
                "MATCH (a:Probe), (b:Probe) "
                "WHERE a.k=0 AND b.k=1 RETURN a.k,b.k",
            )["rows"]
            == [[0, 1]],
            "single-variable property pushdown changed results",
        )
        no_seek = execute(
            db,
            "EXPLAIN MATCH (n:Probe) "
            "WHERE elementId(n)=$x OR elementId(n)=$y RETURN n.k",
            {"x": "n:1", "y": "n:2"},
        )["rows"][0][0]
        require("NodeIdSeek" not in no_seek, "OR must not be transformed to one ID")
        require(
            execute(
                db,
                "MATCH (n:Probe) "
                "WHERE elementId(n)=$x OR elementId(n)=$y RETURN n.k ORDER BY n.k",
                {"x": "n:1", "y": "n:2"},
            )["rows"]
            == [[0], [1]],
            "OR fallback changed results",
        )
        for bad in (None, "n:-1", "n:0001", "r:1", "not-an-id", "n:9999999"):
            require(
                execute(
                    db,
                    "MATCH (n:Probe) WHERE elementId(n)=$id RETURN n.k",
                    {"id": bad},
                )["rows"]
                == [],
                f"nonmatching ID {bad!r} changed equality semantics",
            )
        try:
            execute(db, "MATCH (n:Probe) WHERE elementId(n)=$id RETURN n.k", {"id": 1})
        except sqlite3.Error as error:
            require("LITHOGRAPH_TYPE_ERROR" in str(error), "wrong ID type lost Cypher error")
        else:
            raise AssertionError("different scalar types must keep original equality error")
        require(
            execute(
                db,
                "OPTIONAL MATCH (n:Probe) WHERE elementId(n)=$id RETURN n.k",
                {"id": "n:99999999"},
            )["rows"]
            == [[None]],
            "OPTIONAL null extension changed",
        )
        require(
            execute(
                db,
                "MATCH (a:Probe), (b:Probe) WHERE a.k=b.k AND "
                "elementId(a)=$id RETURN a.k,b.k",
                {"id": "n:1"},
            )["rows"]
            == [[0, 0]],
            "cross-Part residual changed",
        )
        hidden = execute(db, "CREATE (:Probe:Hidden {k:81}) FINISH")
        require(hidden["summary"]["counters"]["nodesCreated"] == 1, "hidden seed")
        opt = {"graphView": {"excludeAnyLabels": ["Hidden"]}}
        require(
            execute(
                db,
                "MATCH (n:Probe) WHERE elementId(n)=$id RETURN n.k",
                {"id": "n:81"},
                opt,
            )["rows"]
            == [],
            "Graph View must filter identity lookup",
        )
        require(
            execute(
                db,
                "MATCH (n:Hidden) WHERE elementId(n)='n:1' RETURN n.k",
            )["rows"]
            == [],
            "Label verification must follow direct ID access",
        )
        write = execute(
            db,
            "MATCH (a:Probe), (b:Probe) "
            "WHERE elementId(a)=$a AND elementId(b)=$b "
            "CREATE (a)-[:LINK]->(b) FINISH",
            params,
        )
        require(
            write["summary"]["counters"]["relationshipsCreated"] == 1,
            "write MATCH created the wrong number of Relationships",
        )
        require(
            execute(db, "MATCH ()-[r:LINK]->() RETURN count(r)")["rows"] == [[1]],
            "write MATCH produced an unexpected Relationship",
        )
        relationship_profile = execute(
            db,
            "PROFILE MATCH (a:Probe)-[r:LINK]->(b:Probe) "
            "RETURN elementId(r)",
        )
        profile_operators = relationship_profile["summary"]["profile"]["operators"]
        adjacency = next(
            (operator for operator in profile_operators if operator["operator"] == "AdjacencySeek"),
            None,
        )
        require(
            adjacency is not None and adjacency["dbHits"] > 0,
            "relationship page and endpoint access must be attributed to AdjacencySeek",
        )
        self_link = execute(
            db,
            "MATCH (a:Probe), (b:Probe) "
            "WHERE elementId(a)=$a AND elementId(b)=$a "
            "CREATE (a)-[:SELF]->(b) FINISH",
            {"a": "n:1"},
        )
        require(
            self_link["summary"]["counters"]["relationshipsCreated"] == 1,
            "self-loop must create exactly once",
        )
        db.execute("SELECT lithograph_tx_begin('{}')").fetchone()
        execute(db, "CREATE (:Probe {k:900}) FINISH")
        staged = execute(
            db, "MATCH (n:Probe) WHERE n.k=900 RETURN elementId(n)"
        )["rows"]
        require(len(staged) == 1, "staged CREATE not visible")
        staged_id = staged[0][0]
        require(
            execute(
                db,
                "MATCH (n:Probe) WHERE elementId(n)=$id RETURN n.k",
                {"id": staged_id},
            )["rows"]
            == [[900]],
            "ID seek did not see the transaction-local Snapshot",
        )
        db.execute("SELECT lithograph_tx_abort()").fetchone()
        require(
            execute(
                db,
                "MATCH (n:Probe) WHERE elementId(n)=$id RETURN n.k",
                {"id": staged_id},
            )["rows"]
            == [],
            "aborted CREATE leaked into Snapshot",
        )
        # A staged DELETE must be visible through the same ID access path,
        # while abort restores both the Node and its connected Relationships.
        db.execute("SELECT lithograph_tx_begin('{}')").fetchone()
        execute(
            db,
            "MATCH (n:Probe) WHERE elementId(n)='n:1' DETACH DELETE n FINISH",
        )
        require(
            execute(
                db,
                "MATCH (n:Probe) WHERE elementId(n)='n:1' RETURN n.k",
            )["rows"]
            == [],
            "staged DELETE leaked through Node ID seek",
        )
        db.execute("SELECT lithograph_tx_abort()").fetchone()
        require(
            execute(
                db,
                "MATCH (n:Probe) WHERE elementId(n)='n:1' RETURN n.k",
            )["rows"]
            == [[0]],
            "aborted DELETE did not restore identity",
        )
        require(
            execute(db, "MATCH ()-[r:LINK]->() RETURN count(r)")["rows"]
            == [[1]],
            "aborted DELETE lost the connected Relationship",
        )
        before_tx = execute(db, "MATCH (n:Probe) RETURN count(n)")["summary"]["commit"]
        db.execute("SELECT lithograph_tx_begin('{}')").fetchone()
        execute(db, "CREATE (:Probe {k:901}) FINISH")
        execute(
            db,
            "MATCH (a:Probe), (b:Probe) "
            "WHERE elementId(a)='n:1' AND elementId(b)='n:2' "
            "CREATE (a)-[:TX_LINK]->(b) FINISH",
        )
        committed_tx = json.loads(db.execute("SELECT lithograph_tx_commit()").fetchone()[0])
        require(
            committed_tx["commit"] != before_tx,
            "explicit transaction did not publish a new Commit",
        )
        require(
            execute(db, "MATCH (n:Probe) RETURN count(n)")["summary"]["commit"]
            == committed_tx["commit"],
            "two staged executions did not finalize on the same durable Commit",
        )
        require(
            execute(db, "MATCH ()-[r:TX_LINK]->() RETURN count(r)")["rows"]
            == [[1]],
            "explicit transaction lost or multiplied Relationship write",
        )
        execute(
            db,
            "MATCH (n:Hidden) WHERE elementId(n)='n:81' DETACH DELETE n FINISH",
        )
        require(
            execute(
                db,
                "MATCH (n:Probe) WHERE elementId(n)=$id RETURN n.k",
                {"id": "n:81"},
            )["rows"]
            == [],
            "deleted Node remains visible",
        )
        require(
            execute(
                db,
                "MATCH (n:Probe) WHERE elementId(n)=$id RETURN n.k",
                {"id": "n:1"},
                {"at": historical_commit},
            )["rows"]
            == [[0]],
            "historical Snapshot identity changed",
        )
        require(
            execute(
                db,
                "MATCH (n:Probe) WHERE elementId(n)=$id RETURN n.k",
                {"id": "n:81"},
                {"at": historical_commit},
            )["rows"]
            == [],
            "historical Snapshot leaked a later Node",
        )
        baseline = execute(db, "MATCH (n:Probe) RETURN count(n)")
        try:
            execute(
                db,
                "MATCH (a:Probe), (b:Probe) "
                "WHERE elementId(a)='n:1' AND elementId(b)='n:2' "
                "CREATE (a)-[:MUST_ROLL_BACK]->(b) FINISH",
                options={"memoryLimitBytes": 64},
            )
        except sqlite3.Error:
            pass
        else:
            raise AssertionError("tiny execution budget should reject write")
        require(
            execute(db, "MATCH (n:Probe) RETURN count(n)")["summary"]["commit"]
            == baseline["summary"]["commit"],
            "resource rejection changed Branch HEAD",
        )
        require(
            execute(db, "MATCH ()-[r:MUST_ROLL_BACK]->() RETURN count(r)")[
                "rows"
            ]
            == [[0]],
            "resource rejection left partial write",
        )
        # A prematurely raised Type error on an incomplete binding, or a
        # suppressed Type error after a NULL/late ID predicate, is a Cypher
        # semantic regression even if the final row set would be empty.
        execute(db, "CREATE (:A {k:'s'}), (:B {k:'s'}), (:A {k:0}), (:B {k:1}) FINISH")
        require(
            execute(
                db,
                "MATCH (a:A), (b:Never) WHERE a.k=1 AND b.k=2 RETURN a.k,b.k",
            )["rows"]
            == [],
            "eager comparison raised on a non-existent complete binding",
        )
        for unsafe in (
            "MATCH (a:A), (b:B) WHERE b.k=1 AND a.k=999 RETURN a.k,b.k",
            "MATCH (a:A), (b:B) WHERE a.k=null AND b.k=1 RETURN a.k,b.k",
            "MATCH (a:A), (b:B) WHERE b.k=1 AND elementId(a)='n:99999999' RETURN a.k,b.k",
            "MATCH (a:A), (b:B) WHERE a.k=1 AND elementId(a)='n:99999999' RETURN a.k,b.k",
        ):
            try:
                execute(db, unsafe)
            except sqlite3.Error as error:
                require("LITHOGRAPH_TYPE_ERROR" in str(error), "lost original Type error")
            else:
                raise AssertionError(f"filter/seek suppressed original Type error: {unsafe}")
        require(
            execute(
                db,
                "MATCH (a:A), (b:B) WHERE elementId(a)='n:99999999' AND b.k=1 RETURN a.k,b.k",
            )["rows"]
            == [],
            "leftmost identity equality must short-circuit a later Type error",
        )
        require(
            "NodeIdSeek"
            not in execute(
                db,
                "EXPLAIN MATCH (a:A), (b:B) WHERE b.k=1 AND elementId(a)='n:99999999' RETURN a.k,b.k",
            )["rows"][0][0],
            "late identity predicate must not hide an earlier error",
        )
        require(
            execute(
                db,
                "MATCH (a:Probe), (b:Probe), (c:Probe) "
                "WHERE a.k=0 AND b.k=1 AND c.k=2 RETURN a.k,b.k,c.k",
            )["rows"]
            == [[0, 1, 2]],
            "ordered three-Part predicate routing changed results",
        )
        require(
            execute(
                db,
                "MATCH (a:Probe), (a:Probe) WHERE elementId(a)='n:1' RETURN a.k",
            )["rows"]
            == [[0]],
            "repeated bound variable changed result multiplicity",
        )
    finally:
        db.close()
    print("Phase 21 SQL correctness: passed (SQLite", sqlite3.sqlite_version, ")")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: phase21_predicate_routing_sql.py <extension-path>")
    run(sys.argv[1])
