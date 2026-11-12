#!/usr/bin/env python3
"""S22.3 RelationshipId statement reuse, writes and physical SQLite work.

The ctypes adapter retains the exact sqlite3_stmt handles, rather than relying
on an implementation's statement cache. Scale workers open synthetic seeds
read-only, completely consume fixed results and trace actual nested SQLite VM
work. Seed construction/checkpoint work is excluded from query measurements.
"""

import argparse
import ctypes as c
import ctypes.util
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import shutil
import sqlite3
import statistics
import subprocess
import tempfile
import time

READ = "MATCH (a)-[r:LINK]->(b) WHERE elementId(r)=$id RETURN r.v,a.k,b.k"
SCALE = "MATCH ()-[r:SCALE_LINK]->() WHERE elementId(r)=$id RETURN 0"
TRACE = c.CFUNCTYPE(c.c_int, c.c_uint, c.c_void_p, c.c_void_p, c.c_void_p)


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def file_hash(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


class SqlError(RuntimeError):
    pass


class SQLite:
    """Only the public stock SQLite C API; the chosen library is explicit."""

    def __init__(self, library):
        self.path = str(Path(library).resolve()) if Path(library).exists() else library
        self.lib = c.CDLL(self.path)
        signatures = {
            "open_v2": ([c.c_char_p, c.POINTER(c.c_void_p), c.c_int, c.c_char_p], c.c_int),
            "close": ([c.c_void_p], c.c_int),
            "get_autocommit": ([c.c_void_p], c.c_int),
            "errmsg": ([c.c_void_p], c.c_char_p),
            "enable_load_extension": ([c.c_void_p, c.c_int], c.c_int),
            "load_extension": ([c.c_void_p, c.c_char_p, c.c_char_p, c.POINTER(c.c_void_p)], c.c_int),
            "free": ([c.c_void_p], None),
            "prepare_v2": ([c.c_void_p, c.c_char_p, c.c_int, c.POINTER(c.c_void_p), c.c_void_p], c.c_int),
            "bind_text": ([c.c_void_p, c.c_int, c.c_char_p, c.c_int, c.c_void_p], c.c_int),
            "clear_bindings": ([c.c_void_p], c.c_int),
            "reset": ([c.c_void_p], c.c_int),
            "step": ([c.c_void_p], c.c_int),
            "column_count": ([c.c_void_p], c.c_int),
            "column_type": ([c.c_void_p, c.c_int], c.c_int),
            "column_int64": ([c.c_void_p, c.c_int], c.c_int64),
            "column_text": ([c.c_void_p, c.c_int], c.c_char_p),
            "finalize": ([c.c_void_p], c.c_int),
            "trace_v2": ([c.c_void_p, c.c_uint, c.c_void_p, c.c_void_p], c.c_int),
            "stmt_status": ([c.c_void_p, c.c_int, c.c_int], c.c_int),
            "next_stmt": ([c.c_void_p, c.c_void_p], c.c_void_p),
            "libversion": ([], c.c_char_p),
        }
        for name, (args, result) in signatures.items():
            function = getattr(self.lib, "sqlite3_" + name)
            function.argtypes, function.restype = args, result
        self.version = self.lib.sqlite3_libversion().decode()
        require(tuple(map(int, self.version.split("."))) >= (3, 45, 0), "SQLite 3.45+ required")
        require(self.version == sqlite3.sqlite_version,
                f"ctypes SQLite {self.version} differs from Python runtime {sqlite3.sqlite_version}")

    def connect(self, extension, path=":memory:", readonly=False):
        return Database(self, extension, path, readonly)


def select_sqlite(library):
    if library:
        return SQLite(library)
    candidates = []
    configured = os.environ.get("LITHOGRAPH_SQLITE3")
    binary = configured or shutil.which("sqlite3")
    if binary:
        resolved = Path(shutil.which(binary) or binary).resolve()
        prefix = resolved if resolved.is_dir() else resolved.parent.parent
        for directory in (prefix/"lib", prefix/"lib64"):
            for name in ("libsqlite3.dylib", "libsqlite3.so", "libsqlite3.so.0"):
                candidates.append(str(directory/name))
            # Debian/Ubuntu install shared libraries below a multiarch lib
            # directory even when the selected CLI is /usr/bin/sqlite3.
            candidates.extend(str(path) for path in sorted(directory.glob("*-linux-gnu/libsqlite3.so*")))
    found = ctypes.util.find_library("sqlite3")
    if found:
        candidates.append(found)
    for candidate in dict.fromkeys(candidates):
        try:
            selected = SQLite(candidate)
        except (OSError, AssertionError):
            continue
        if configured:
            cli = subprocess.run([str(Path(shutil.which(configured) or configured).resolve()),
                                  ":memory:", "SELECT sqlite_version()"],
                                 check=True, capture_output=True, text=True, timeout=10)
            require(cli.stdout.strip() == selected.version,
                    "selected SQLite CLI/shared library/Python runtime versions differ")
        return selected
    raise RuntimeError(f"no SQLite 3.45+ shared library matches Python {sqlite3.sqlite_version}; "
                       "set --sqlite-library to the matching stock SQLite library")


class Statement:
    def __init__(self, db, sql):
        self.db, self.ptr = db, c.c_void_p()
        db.check(db.api.sqlite3_prepare_v2(db.ptr, sql.encode(), -1, c.byref(self.ptr), None))

    def execute(self, bindings=(), stop_after_row=False):
        # An expected previous invocation error is returned by reset again.
        self.db.api.sqlite3_reset(self.ptr)
        self.db.check(self.db.api.sqlite3_clear_bindings(self.ptr))
        for index, value in enumerate(bindings, 1):
            encoded = value.encode()
            self.db.check(self.db.api.sqlite3_bind_text(
                self.ptr, index, encoded, len(encoded), c.c_void_p(-1)))
        result = []
        while True:
            rc = self.db.api.sqlite3_step(self.ptr)
            if rc == 101:  # SQLITE_DONE
                return result
            self.db.check(rc, 100)  # SQLITE_ROW
            row = []
            for column in range(self.db.api.sqlite3_column_count(self.ptr)):
                kind = self.db.api.sqlite3_column_type(self.ptr, column)
                if kind == 5:
                    row.append(None)
                elif kind == 1:
                    row.append(self.db.api.sqlite3_column_int64(self.ptr, column))
                else:
                    row.append(self.db.api.sqlite3_column_text(self.ptr, column).decode())
            result.append(row)
            if stop_after_row and row[0] == "row":
                self.db.api.sqlite3_reset(self.ptr)
                return result

    def close(self):
        if self.ptr:
            # finalize can also repeat a previous expected step error.
            self.db.api.sqlite3_finalize(self.ptr)
            self.ptr = c.c_void_p()


class Database:
    def __init__(self, sqlite, extension, path, readonly):
        self.api, self.ptr = sqlite.lib, c.c_void_p()
        self.check(self.api.sqlite3_open_v2(str(path).encode(), c.byref(self.ptr),
                                         1 if readonly else 2 | 4, None))
        self.check(self.api.sqlite3_enable_load_extension(self.ptr, 1))
        error = c.c_void_p()
        rc = self.api.sqlite3_load_extension(self.ptr, str(Path(extension).resolve()).encode(),
                                             b"sqlite3_lithograph_init", c.byref(error))
        try:
            if rc:
                raise SqlError(c.string_at(error).decode() if error.value else "extension load failed")
        finally:
            self.api.sqlite3_free(error)
        self.check(self.api.sqlite3_enable_load_extension(self.ptr, 0))
        self.scalar_stmt = Statement(self, "SELECT lithograph(?1,?2,?3)")
        self.rows_stmt = Statement(self, "SELECT event,data FROM lithograph_rows(?1,?2,?3)")
        self.trace_callback = None
        self.sql("PRAGMA cache_size=-2000")

    def check(self, rc, expected=0):
        if rc != expected:
            raise SqlError(f"SQLite {rc}: {self.api.sqlite3_errmsg(self.ptr).decode()}")

    def sql(self, text):
        stmt = Statement(self, text)
        try:
            return stmt.execute()
        finally:
            stmt.close()

    def execute(self, text, params=None, options=None, surface="scalar"):
        bindings = (text, json.dumps(params or {}), json.dumps(options or {}))
        if surface == "scalar":
            rows = self.scalar_stmt.execute(bindings)
            require(len(rows) == 1, "scalar envelope cardinality")
            return json.loads(rows[0][0])
        events = self.rows_stmt.execute(bindings)
        require(events and events[0][0] == "columns" and events[-1][0] == "summary",
                "stream was not completely consumed")
        return {"rows": [json.loads(data) for event, data in events if event == "row"],
                "summary": json.loads(events[-1][1])}

    def start_work(self):
        self.work = dict(statements=0, vm_steps=0, fullscan_steps=0, sorts=0)
        self.trace_error = None
        # sqlite3_reset() does not clear STMTSTATUS counters. Clear every live
        # prepared statement before installing the trace, so warm-up work on
        # the retained outer handles cannot enter the next invocation's sample.
        stmt = self.api.sqlite3_next_stmt(self.ptr, None)
        while stmt:
            for status in (4, 1, 2):
                self.api.sqlite3_stmt_status(stmt, status, 1)
            stmt = self.api.sqlite3_next_stmt(self.ptr, stmt)

        def profile(kind, _context, stmt, _details):
            if kind != 2:  # SQLITE_TRACE_PROFILE
                return 0
            try:
                for field, status in (("vm_steps", 4), ("fullscan_steps", 1), ("sorts", 2)):
                    value = self.api.sqlite3_stmt_status(stmt, status, 1)
                    require(value >= 0, "negative/overflowed SQLite work counter")
                    self.work[field] += value
                self.work["statements"] += 1
            except BaseException as error:
                self.trace_error = str(error)
            return 0

        self.trace_callback = TRACE(profile)
        self.check(self.api.sqlite3_trace_v2(self.ptr, 2, self.trace_callback, None))

    def stop_work(self):
        self.check(self.api.sqlite3_trace_v2(self.ptr, 0, None, None))
        self.trace_callback = None
        require(self.trace_error is None, f"invalid SQLite work trace: {self.trace_error}")
        require(self.work["statements"] > 1 and self.work["vm_steps"] > 0,
                "trace did not include engine SQL")
        return self.work

    def close(self):
        self.api.sqlite3_trace_v2(self.ptr, 0, None, None)
        self.scalar_stmt.close()
        self.rows_stmt.close()
        self.check(self.api.sqlite3_close(self.ptr))


def state(db):
    return db.sql("SELECT lower(hex(commit_id)),(SELECT count(*) FROM _lithograph_commits) "
                  "FROM _lithograph_branches WHERE name='main'")[0]


def expect_error(db, query, params, surface="scalar"):
    try:
        db.execute(query, params, surface=surface)
    except SqlError as error:
        require("LITHOGRAPH_TYPE_ERROR" in str(error), f"unexpected error: {error}")
    else:
        raise AssertionError("expected Cypher type error")


def semantics(sqlite, extension):
    directory = tempfile.TemporaryDirectory(prefix="phase22-relationship-")
    path = str(Path(directory.name)/"semantic.sqlite3")
    db = sqlite.connect(extension, path)
    try:
        db.sql("SELECT lithograph_init()")
        result = db.execute("CREATE (a:Visible {k:11})-[x:LINK {v:1}]->(b:Visible {k:12}),"
                            "(a)-[y:LINK {v:2}]->(b) RETURN elementId(x),elementId(y)")
        first, second = result["rows"][0]
        historical = {"at": result["summary"]["commit"]}
        handles = (db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value)
        checks = 0
        for _ in range(8):
            for surface in ("scalar", "rows"):
                for identity, expected in ((first, [[1, 11, 12]]), (second, [[2, 11, 12]]),
                                           ("r:99999999", []), (None, []), ("r:0001", []),
                                           ("n:1", []), (first, [[1, 11, 12]])):
                    require(db.execute(READ, {"id": identity}, surface=surface)["rows"] == expected,
                            "exact prepared handle retained old Relationship binding")
                    checks += 1
                expect_error(db, READ, {"id": 42}, surface)
                require(db.execute(READ, {"id": first}, surface=surface)["rows"] == [[1, 11, 12]],
                        "prepared statement failed after error")
                for options, expected in (({"graphView": {"requireAllLabels": ["Noise"]}}, []),
                                          ({"graphView": {"requireAllLabels": ["Visible"]}}, [[1, 11, 12]]),
                                          ({"readAccess": {"query": {"denyProperties": ["v"]}}}, [[None, 11, 12]])):
                    require(db.execute(READ, {"id": first}, options, surface)["rows"] == expected,
                            "prepared statement retained visibility/access policy")
                    checks += 1
                checks += 2
        for surface in ("scalar", "rows"):
            before = state(db)
            changed = db.execute("PROFILE MATCH ()-[r:LINK]->() WHERE elementId(r)=$id "
                                 "SET r.v=r.v+1 RETURN r.v", {"id": first}, surface=surface)
            require(any(op["operator"] == "RelationshipIdSeek" and op["dbHits"] > 0
                        for op in changed["summary"]["profile"]["operators"]), "write did not use seek")
            require(state(db)[0] != before[0] and state(db)[1] == before[1]+1, "SET publication count")
            current = changed["rows"][0][0]
            for query in ("SET r.v=1/0 FINISH", "SET r.v=999 RETURN 1/0", "DELETE r WITH 1 AS x RETURN 1/0"):
                before = state(db)
                expect_error(db, "MATCH ()-[r:LINK]->() WHERE elementId(r)=$id " + query,
                             {"id": first}, surface)
                require(state(db) == before, "failed write moved head or published Commit")
                require(db.execute(READ, {"id": first})["rows"] == [[current, 11, 12]], "failed write survived")
                checks += 1
            # Resetting an actual row cursor before the summary must abort its writes.
            before = state(db)
            db.rows_stmt.execute(("MATCH ()-[r:LINK]->() WHERE elementId(r)=$id SET r.v=888 RETURN r.v",
                                  json.dumps({"id": first}), "{}"), stop_after_row=True)
            require(state(db) == before and db.execute(READ, {"id": first})["rows"] == [[current, 11, 12]],
                    "early-close published a Relationship write")
            require(db.execute(READ, {"id": first}, historical, surface)["rows"] == [[1, 11, 12]], "historical rebind")
            checks += 4
        before = state(db)
        program = db.execute("PROFILE MATCH ()-[r:LINK]->() WHERE elementId(r)=$id "
                             "SET r.v=23 WITH r MATCH ()-[same:LINK]->() WHERE elementId(same)=$id "
                             "SET same.v=r.v+1 RETURN r.v,same.v", {"id": first})
        require(program["rows"] == [[24, 24]] and state(db)[1] == before[1]+1,
                "a later MATCH clause did not observe the completed Relationship SET")
        require(sum(op["operator"] == "RelationshipIdSeek" and op["dbHits"] > 0
                    for op in program["summary"]["profile"]["operators"]) == 2,
                "MATCH clauses around WITH/write scanned unrelated Relationships")
        before = state(db)
        db.sql("SELECT lithograph_tx_begin('{}')")
        created = db.execute("CREATE (a:Visible {k:41})-[r:LINK {v:7}]->(b:Visible {k:42}) RETURN elementId(r)")
        require(created["summary"]["commit"] is None, "staged execution exposed a public Commit identity")
        staged = created["rows"][0][0]
        require(staged not in (first, second), "committed Relationship identity reused")
        for surface in ("scalar", "rows"):
            require(db.execute(READ, {"id": staged}, surface=surface)["rows"] == [[7, 41, 42]], "staged rebind")
        db.execute("MATCH ()-[r:LINK]->() WHERE elementId(r)=$id SET r.v=8 FINISH", {"id": staged})
        require(db.execute(READ, {"id": staged})["rows"] == [[8, 41, 42]], "staged SET was stale")
        db.execute("MATCH ()-[r:LINK]->() WHERE elementId(r)=$id DELETE r FINISH", {"id": second})
        require(db.execute(READ, {"id": second}, surface="rows")["rows"] == [], "staged DELETE was stale")
        # The writer sees private provisional heads inside its transaction.
        # Another real connection must continue to see the durable base.
        observer = sqlite.connect(extension, path, True)
        try:
            require(state(observer) == before, "staged calls published durable history")
            require(observer.execute(READ, {"id": staged})["rows"] == [], "staged ID escaped transaction")
            require(observer.execute(READ, {"id": second})["rows"] == [[2, 11, 12]], "staged delete escaped transaction")
        finally:
            observer.close()
        committed = json.loads(db.sql("SELECT lithograph_tx_commit()")[0][0])
        require(state(db)[0] != before[0] and state(db)[1] == before[1]+1, "staged commit publication count")
        require(committed["counters"]["relationshipsCreated"] == 1
                and committed["counters"]["relationshipsDeleted"] == 1
                and committed["counters"]["nodesCreated"] == 2,
                "staged writes were duplicated or omitted")
        for surface in ("scalar", "rows"):
            require(db.execute(READ, {"id": staged}, surface=surface)["rows"] == [[8, 41, 42]], "committed seek stale")
            require(db.execute(READ, {"id": second}, surface=surface)["rows"] == [], "committed delete stale")
            require(db.execute(READ, {"id": second}, historical, surface)["rows"] == [[2, 11, 12]], "historical delete rebind")
            before = state(db)
            db.sql("SELECT lithograph_tx_begin('{}')")
            db.execute("MATCH ()-[r:LINK]->() WHERE elementId(r)=$id SET r.v=777 FINISH", {"id": first})
            db.execute("MATCH ()-[r:LINK]->() WHERE elementId(r)=$id DELETE r FINISH", {"id": staged})
            expect_error(db, READ, {"id": 42}, surface)
            require(db.api.sqlite3_get_autocommit(db.ptr) == 1 and state(db) == before,
                    "failed invocation did not abort the entire explicit transaction")
            require(db.execute(READ, {"id": first})["rows"] == [[24, 11, 12]], "failed transaction kept SET")
            require(db.execute(READ, {"id": staged})["rows"] == [[8, 41, 42]], "failed transaction kept DELETE")
        before = state(db)
        for surface in ("scalar", "rows"):
            tail = ("DELETE r WITH 1 AS deleted MATCH ()-[same:LINK]->() "
                    "WHERE elementId(same)=$id RETURN same.v") if surface == "scalar" else "DELETE r FINISH"
            deleted = db.execute("PROFILE MATCH ()-[r:LINK]->() WHERE elementId(r)=$id " + tail,
                                 {"id": first}, surface=surface)
            require(deleted["rows"] == [] and deleted["summary"]["counters"]["relationshipsDeleted"]
                    == (1 if surface == "scalar" else 0), "DELETE effect or later MATCH visibility")
            require(any(op["operator"] == "RelationshipIdSeek" and op["dbHits"] > 0
                        for op in deleted["summary"]["profile"]["operators"]),
                    "DELETE program did not perform the physical identity lookup")
            require(db.execute(READ, {"id": first}, surface=surface)["rows"] == [], "deleted ID rebind")
        require(state(db)[1] == before[1]+2, "each successful mutating invocation must publish once, including an empty DELETE")
        replacement = db.execute("CREATE (:Visible {k:51})-[r:LINK {v:11}]->(:Visible {k:52}) RETURN elementId(r)")["rows"][0][0]
        require(replacement not in (first, second, staged), "a deleted committed Relationship identity was reused")
        require(db.execute(READ, {"id": replacement})["rows"] == [[11, 51, 52]], "replacement identity binding")
        require(db.execute(READ, {"id": first})["rows"] == []
                and db.execute(READ, {"id": first}, historical)["rows"] == [[1, 11, 12]],
                "new allocation changed deleted-current or immutable historical identity")
        require(handles == (db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value), "prepared handles replaced")
        require(db.sql("PRAGMA quick_check") == [["ok"]], "semantic fixture integrity")
        return {"kind": "relationship_semantics", "sqlite": sqlite.version, "sqlite_library": sqlite.path,
                "extension_sha256": file_hash(extension), "rebind_and_write_checks": checks,
                "same_scalar_and_rows_handles": True, "staged_commit": committed["commit"]}
    finally:
        db.close()
        directory.cleanup()


def seed(sqlite, extension, path, count, checkpoint):
    require(count > 0 and not Path(path).exists(), "seed requires a positive size and new disposable path")
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    db = sqlite.connect(extension, path)
    started = time.perf_counter()
    try:
        db.sql("SELECT lithograph_init()")
        result = db.execute(f"UNWIND range(0,{count-1}) AS i CREATE (:RelScale)-[:SCALE_LINK]->(:RelScale) FINISH")
        require(result["summary"]["counters"]["relationshipsCreated"] == count, "seed Relationship count")
        require(result["summary"]["counters"]["nodesCreated"] == 2*count, "seed Node count")
    finally:
        db.close()
    completed = subprocess.run([str(Path(checkpoint).resolve()), str(Path(path).resolve())],
                               check=True, capture_output=True, text=True)
    checkpoint_result = json.loads(completed.stdout)
    require(checkpoint_result["node_count"] == 2*count and checkpoint_result["relationship_count"] == count,
            "checkpoint cardinality")
    return {"kind": "relationship_seed", "relationships": count, "nodes": 2*count,
            "seconds": time.perf_counter()-started, "checkpoint": checkpoint_result}


def percentiles(values):
    values = sorted(values)
    return {"n": len(values), "p50_us": statistics.median(values),
            "p95_us": values[min(len(values)-1, int(len(values)*.95))], "max_us": max(values)}


def measure_one(sqlite, extension, path, count, connections, repeats, reopens, seek):
    before = file_hash(path)
    inventory = sqlite.connect(extension, path, True)
    try:
        checkpoints = inventory.sql("SELECT lower(hex(commit_id)) FROM _lithograph_checkpoints")
        require(len(checkpoints) == 1, "scale fixture must have one unambiguous checkpoint Snapshot")
        checkpoint = checkpoints[0][0]
        # The reused large fixture can have later workload commits. Pin the
        # measured graph to its verified immutable checkpoint, rather than
        # claiming checkpoint counts describe a different Branch head.
        options = {"at": "commit/" + checkpoint}
        actual = inventory.sql("SELECT count(*) FROM _lithograph_cp_relationships WHERE commit_id=x'" + checkpoint + "'")[0][0]
        require(actual == count, "fixture does not have the claimed checkpoint Relationship count")
        nodes = inventory.sql("SELECT count(*) FROM _lithograph_cp_nodes WHERE commit_id=x'" + checkpoint + "'")[0][0]
    finally:
        inventory.close()
    samples, first, fresh, work, hits = [], [], [], [], []
    warm_raw, warm_work = [], []
    plan = None

    def timed_read(db):
        started = time.perf_counter_ns()
        result = db.execute(SCALE, {"id": "r:1"}, options)
        elapsed = (time.perf_counter_ns()-started)/1000
        require(result["rows"] == [[0]], "fixed scale result changed")
        return elapsed

    for connection in range(connections):
        db = sqlite.connect(extension, path, True)
        try:
            # Keep the first invocation separate and visible, without init,
            # integrity or EXPLAIN prewarming. Every connection then contributes
            # the requested number of genuinely warm, fully consumed queries.
            first.append(timed_read(db))
            connection_samples = [timed_read(db) for _ in range(repeats)]
            warm_raw.append(connection_samples)
            samples.extend(connection_samples)
            connection_work = []
            for identity, expected in (("r:1", [[0]]), ("r:9000000000000000000", [])):
                db.start_work()
                result = db.execute(SCALE, {"id": identity}, options)
                measured = db.stop_work()
                require(result["rows"] == expected, "VM workload result/multiplicity changed")
                connection_work.append(dict(measured, matched=bool(expected)))
                if connection == 0:
                    profiled = db.execute("PROFILE " + SCALE, {"id": identity}, options)
                    require(profiled["rows"] == expected, "PROFILE changed result")
                    hits.append(profiled["summary"]["metrics"]["dbHits"])
            warm_work.append(connection_work)
            if connection == 0:
                work = connection_work
                plan = db.execute("EXPLAIN " + SCALE, {"id": "r:1"}, options)["rows"][0][0]
                require(("RelationshipIdSeek" in plan) == seek, "unexpected scale access plan")
        finally:
            db.close()
    for _ in range(reopens):
        db = sqlite.connect(extension, path, True)
        try:
            fresh.append(timed_read(db))
        finally:
            db.close()
    fresh_work = []
    for identity, expected in (("r:1", [[0]]), ("r:9000000000000000000", [])):
        db = sqlite.connect(extension, path, True)
        try:
            db.start_work()
            result = db.execute(SCALE, {"id": identity}, options)
            measured = db.stop_work()
            require(result["rows"] == expected, "new-connection VM result changed")
            fresh_work.append(dict(measured, matched=bool(expected)))
        finally:
            db.close()
    require(before == file_hash(path), "read worker changed synthetic source")
    peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return {"kind": "relationship_scale", "relationships": count, "nodes": nodes,
            "connections": connections, "repeats": repeats, "reopens": reopens,
            "sqlite": sqlite.version, "sqlite_library": sqlite.path, "platform": platform.platform(),
            "extension_sha256": file_hash(extension), "fixture_sha256": before,
            "snapshot": options["at"],
            "seek": seek, "plan": plan, "actual_sqlite_work": work, "profile_db_hits": hits,
            "warm_sqlite_work": warm_work,
            "fresh_sqlite_work": fresh_work,
            "connection_first": percentiles(first), "connection_first_us": first,
            "warm": percentiles(samples), "warm_raw_us": warm_raw,
            "first_and_warm": percentiles(first+samples),
            "fresh_connections": percentiles(fresh), "fresh_raw_us": fresh,
            "fresh_vm_connections": len(fresh_work), "inventory_connections": 1,
            "total_opened_connections": connections+reopens+len(fresh_work)+1,
            "process_peak_rss_bytes": peak if platform.system() == "Darwin" else peak*1024}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sqlite-library", help="stock SQLite 3.45+ library (default: selected CLI prefix or system lookup)")
    parser.add_argument("--semantics-only", metavar="EXTENSION", help="run only the lightweight semantic fixture")
    commands = parser.add_subparsers(dest="command")
    semantic = commands.add_parser("semantics")
    semantic.add_argument("extension")
    seed_parser = commands.add_parser("seed")
    seed_parser.add_argument("extension")
    seed_parser.add_argument("database")
    seed_parser.add_argument("--relationships", type=int, required=True)
    seed_parser.add_argument("--checkpoint", required=True)
    scale_parser = commands.add_parser("measure")
    scale_parser.add_argument("extension")
    scale_parser.add_argument("--fixture", action="append", required=True, help="RELATIONSHIPS=PATH")
    scale_parser.add_argument("--before-extension")
    scale_parser.add_argument("--before-max-relationships", type=int, default=100000)
    scale_parser.add_argument("--connections", type=int, default=5)
    scale_parser.add_argument("--repeats", type=int, default=20)
    scale_parser.add_argument("--reopens", type=int, default=100)
    args = parser.parse_args()
    require(bool(args.semantics_only) != bool(args.command), "select a command or --semantics-only EXTENSION")
    if args.semantics_only:
        args.command, args.extension = "semantics", args.semantics_only
    sqlite = select_sqlite(args.sqlite_library)
    if args.command == "semantics":
        print(json.dumps(semantics(sqlite, args.extension), sort_keys=True))
    elif args.command == "seed":
        print(json.dumps(seed(sqlite, args.extension, args.database, args.relationships, args.checkpoint), sort_keys=True))
    else:
        require(args.connections > 0 and args.repeats > 0 and args.reopens > 0,
                "positive connection/repeat/reopen counts required")
        require(0 <= args.before_max_relationships <= 100000, "scan baseline is capped at 100k Relationships")
        results = []
        for fixture in args.fixture:
            count, path = fixture.split("=", 1)
            count = int(count)
            result = measure_one(sqlite, args.extension, path, count,
                                 args.connections, args.repeats, args.reopens, True)
            results.append(result)
            print(json.dumps(result, sort_keys=True), flush=True)
            if args.before_extension and count <= args.before_max_relationships:
                # Keep the previous scan explicitly at one warm connection,
                # one warm repetition and one reopen; its first query is also
                # retained separately rather than hidden as a warm-up.
                print(json.dumps(measure_one(sqlite, args.before_extension, path, count, 1, 1, 1, False),
                                 sort_keys=True), flush=True)
        for mode in ("actual_sqlite_work", "fresh_sqlite_work"):
            for slot in range(2):
                values = [r[mode][slot]["vm_steps"] for r in results]
                # Allow logarithmic B-tree depth/schema differences, reject a
                # candidate-domain scan over the 1k -> 1M expansion.
                require(max(values) <= min(values)*3, "RelationshipId query VM grew with unrelated data")


if __name__ == "__main__":
    main()
