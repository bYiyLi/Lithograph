#!/usr/bin/env python3
"""Automatic checkpoint publication against live SQLite read guards.

Only public writes create canonical state. Every database is disposable, and
derived-state removal is confined to a closed synthetic fixture.
"""

import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import time

from phase21_predicate_routing_sql import execute, require, rows
from phase22_access_work import Latency, cgroup, file_hash, open_db


READ = "MATCH (n:Probe) RETURN n.k,n.v"
WRITE = "MATCH (n:Probe) WHERE elementId(n)='n:1' SET n.v=$v FINISH"


def head(db):
    return db.execute("SELECT commit_id FROM main._lithograph_branches WHERE name='main'").fetchone()[0]


def begin_reader(db):
    cursor = db.execute("SELECT event,data FROM lithograph_rows(?)", (READ,))
    observed = []
    for event, data in cursor:
        if event == "row":
            observed.append(json.loads(data))
            break
    require(len(observed) == 1, "read guard never produced its first row")
    return cursor, observed


def finish_reader(cursor, observed):
    terminal = False
    for event, data in cursor:
        if event == "row":
            observed.append(json.loads(data))
        elif event == "summary":
            terminal = True
    cursor.close()
    require(terminal, "pinned read did not reach its success summary")
    require(sorted(observed) == [[i, 0] for i in range(256)], "pinned read drifted after publication")


def same_connection_boundary(db):
    cursor, observed = begin_reader(db)
    before = head(db)
    try:
        execute(db, WRITE, {"v": 1})
    except sqlite3.Error as error:
        require("active Lithograph read cursor" in str(error), f"wrong writer rejection: {error}")
    else:
        raise AssertionError("same-connection writer bypassed the live read guard")
    require(head(db) == before, "rejected writer moved the Branch")
    finish_reader(cursor, observed)


def wal_publication(extension, path):
    writer = open_db(extension, str(path))
    require(writer.execute("PRAGMA journal_mode=WAL").fetchone() == ("wal",), "WAL setup")
    writer.execute("SELECT lithograph_init()").fetchone()
    execute(writer, "UNWIND range(0,255) AS i CREATE (:Probe {k:i,v:0}) FINISH")
    same_connection_boundary(writer)
    reader = open_db(extension, str(path), True)
    cursor, observed = begin_reader(reader)
    original = head(writer)
    wait, hold = Latency(), Latency()
    maintenance, previous, peak_wal = [], 0, 0
    # This exceeds the policy's forced chain depth. Commit content sampling
    # avoids relying on any particular ID or wall-clock timestamp.
    for index in range(1, 769):
        waiting = time.perf_counter_ns()
        writer.execute("BEGIN IMMEDIATE")
        acquired = time.perf_counter_ns()
        wait.add((acquired-waiting)/1000)
        execute(writer, WRITE, {"v": index % 2})
        writer.execute("COMMIT")
        elapsed = (time.perf_counter_ns()-acquired)/1000
        hold.add(elapsed)
        current = writer.execute("SELECT count(*) FROM main._lithograph_checkpoints").fetchone()[0]
        if current != previous:
            maintenance.append({"at": index, "checkpoints": current, "writer_hold_us": elapsed})
            previous = current
        wal = Path(str(path)+"-wal")
        peak_wal = max(peak_wal, wal.stat().st_size if wal.exists() else 0)
    automatic = writer.execute(
        "SELECT count(*) FROM main._lithograph_checkpoints "
        "WHERE json_extract(CAST(metadata AS TEXT),'$.automatic')=1"
    ).fetchone()[0]
    require(0 < automatic <= 4, "authorized writer did not publish a bounded automatic checkpoint")
    require(head(writer) != original, "writer did not advance while another connection read")
    finish_reader(cursor, observed)
    reader.close()
    require(rows(writer, "MATCH (n:Probe) WHERE elementId(n)='n:1' RETURN n.v") == [[0]], "current result")
    writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchall()
    require(writer.execute("PRAGMA quick_check").fetchone() == ("ok",), "WAL integrity")
    writer.close()
    return {"automatic_checkpoints": automatic, "writer_wait": wait.report(),
            "writer_hold": hold.report(), "checkpoint_events": maintenance,
            "peak_wal_bytes": peak_wal}


def rollback_journal(extension, path):
    reader = open_db(extension, str(path))
    require(reader.execute("PRAGMA journal_mode=DELETE").fetchone() == ("delete",), "DELETE setup")
    reader.execute("SELECT lithograph_init()").fetchone()
    execute(reader, "UNWIND range(0,255) AS i CREATE (:Probe {k:i,v:0}) FINISH")
    writer = open_db(extension, str(path))
    writer.execute("PRAGMA busy_timeout=1")
    cursor, observed = begin_reader(reader)
    before = head(writer)
    try:
        execute(writer, WRITE, {"v": 1})
    except sqlite3.Error as error:
        if error.sqlite_errorcode not in (sqlite3.SQLITE_BUSY, sqlite3.SQLITE_LOCKED):
            # A failed outer SAVEPOINT RELEASE can prevent its cleanup RELEASE
            # too. The documented full-rollback/quarantine contract then takes
            # precedence; discard that connection and verify from a new one.
            require("LITHOGRAPH_INTERNAL_ERROR" in str(error)
                    and "full SQLite rollback executed" in str(error), str(error))
            require(not writer.in_transaction, "full rollback left a transaction active")
            writer.close()
            writer = open_db(extension, str(path))
    else:
        raise AssertionError("rollback-journal writer published across an active reader")
    require(head(writer) == before, "blocked writer left a partial Commit")
    finish_reader(cursor, observed)
    execute(writer, WRITE, {"v": 1})
    require(rows(reader, "MATCH (n:Probe) WHERE elementId(n)='n:1' RETURN n.v") == [[1]], "writer recovery")
    require(reader.execute("PRAGMA quick_check").fetchone() == ("ok",), "rollback-journal integrity")
    writer.close()
    reader.close()


def readonly_reopen(extension, path):
    before = file_hash(path)
    db = open_db(extension, str(path), True)
    db.execute("PRAGMA query_only=ON")
    for prefix in ("", "EXPLAIN ", "PROFILE "):
        result = execute(db, prefix + "MATCH (n:Probe) WHERE elementId(n)='n:1' RETURN n.v")
        if prefix == "EXPLAIN ":
            require(bool(result["rows"]), "read-only EXPLAIN")
        else:
            require(result["rows"] == [[0]], "read-only result")
    require(sorted(rows(db, READ)) == [[i, 0] for i in range(256)], "read-only rows/reopen")
    db.close()
    require(file_hash(path) == before, "read-only query modified main")


def cold_branch_lazy_projection(extension):
    db = open_db(extension, ":memory:")
    db.execute("SELECT lithograph_init()").fetchone()
    execute(db, "CREATE (:Keep {k:7,v:0}) FINISH")
    checkpoints = []
    for _ in range(4096):
        execute(db, "CALL lithograph.commit.create() YIELD commit RETURN commit")
        checkpoints = [row[0] for row in db.execute(
            "SELECT hex(commit_id) FROM _lithograph_checkpoints ORDER BY created_at,commit_id")]
        if len(checkpoints) == 4:
            break
    require(len(checkpoints) == 4, "did not seed four automatic checkpoints")
    first = checkpoints[0]
    execute(db, "CALL lithograph.branch.create('cold',$from)", {"from": "commit/" + first.lower()})
    query = ("MATCH (n:Keep) WHERE elementId(n)='n:1' "
             "SET n.v=n.v+1 RETURN n.k,n.v,labels(n)")
    for index in range(1, 1025):
        # Both SQL adapters must consume the lazy, post-publication projection.
        result = (rows(db, query, options={"branch": "cold"}) if index % 2 else
                  execute(db, query, options={"branch": "cold"})["rows"])
        require(result == [[7, index, ["Keep"]]], "cold Branch projection lost its base")
        current = {row[0] for row in db.execute("SELECT hex(commit_id) FROM _lithograph_checkpoints")}
        require(first in current, "publication evicted its own projection base")
        require(len(current) <= 4, "protected base exceeded automatic retention")
        if current != set(checkpoints):
            break
    else:
        raise AssertionError("cold Branch never reached automatic publication")
    require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "cold Branch integrity")
    db.close()


def first_automatic_checkpoint(db):
    for _ in range(2000):
        execute(db, "CALL lithograph.commit.create() YIELD commit RETURN commit")
        if db.execute("SELECT count(*) FROM _lithograph_checkpoints").fetchone()[0]:
            return
    raise AssertionError("did not reach the first automatic checkpoint")


def version_program_retains_its_original_snapshot(extension):
    for adapter in ("scalar", "rows"):
        db = open_db(extension, ":memory:")
        db.execute("SELECT lithograph_init()").fetchone()
        execute(db, "CREATE (:Keep {k:7}) FINISH")
        first_automatic_checkpoint(db)
        query = ("MATCH (n:Keep) WHERE elementId(n)='n:1' UNWIND range(1,2600) AS i "
                 "CALL lithograph.commit.create() YIELD commit RETURN n.k")
        result = rows(db, query) if adapter == "rows" else execute(db, query)["rows"]
        require(result == [[7]] * 2600, "multiple Version Procedures invalidated the pinned Snapshot")
        require(db.execute("SELECT count(*) FROM _lithograph_checkpoints").fetchone()[0] <= 4,
                "Version program exceeded automatic retention")
        require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "Version program integrity")
        db.close()


def reset_gc_retains_its_original_snapshot(extension):
    for checkpoint in (False, True):
        for adapter in ("scalar", "rows"):
            db = open_db(extension, ":memory:")
            db.execute("SELECT lithograph_init()").fetchone()
            root = execute(db, "RETURN 1")["summary"]["commit"]
            execute(db, "CREATE (:Keep {k:7}) FINISH")
            if checkpoint:
                first_automatic_checkpoint(db)
            before_commits = db.execute("SELECT count(*) FROM _lithograph_commits").fetchone()[0]
            query = ("MATCH (n:Keep) CALL lithograph.reset($root) YIELD to "
                     "CALL lithograph.gc() YIELD commits,checkpoints RETURN n.k,commits,checkpoints")
            result = (rows(db, query, {"root": root}) if adapter == "rows" else
                      execute(db, query, {"root": root})["rows"])
            require(result == [[7, 0, 0]], "same-query GC discarded the pinned Commit/Checkpoint")
            require(execute(db, "RETURN 1")["summary"]["commit"] == root, "Reset did not move Branch")
            result = execute(db, "CALL lithograph.gc() YIELD commits,checkpoints RETURN commits,checkpoints")["rows"]
            require(result == [[before_commits - 1, int(checkpoint)]], "query-local GC roots leaked after release")
            require(rows(db, "MATCH (n:Keep) RETURN n.k") == [], "Reset graph differs after GC")
            require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "Reset/GC integrity")
            db.close()


def checkpoint_metadata_types(extension):
    db = open_db(extension, ":memory:")
    try:
        db.execute("SELECT lithograph_init()").fetchone()
        execute(db, "UNWIND range(0,31) AS i CREATE (:Common:Rare {v:i}) FINISH")
        first_automatic_checkpoint(db)
        checkpoint, valid = db.execute(
            "SELECT commit_id,metadata FROM main._lithograph_checkpoints"
        ).fetchone()
        query = "MATCH (n:Common:Rare) RETURN n.v ORDER BY n.v"
        expected = [[value] for value in range(32)]
        original_head = head(db)
        # Derived corruption is isolated to this synthetic database. Even valid
        # JSON stored with the wrong SQLite type must take bounded fallback.
        for metadata in ("broken", valid.decode(), "x" * (16 * 1024 * 1024)):
            db.execute("UPDATE main._lithograph_checkpoints SET metadata=? WHERE commit_id=?",
                       (metadata, checkpoint))
            before = db.total_changes
            explained = execute(db, "EXPLAIN " + query)["rows"]
            require("LabelIndexScan" in explained[0][0], "invalid metadata fallback plan")
            require(rows(db, "EXPLAIN " + query) == explained, "metadata rows EXPLAIN")
            require(execute(db, query)["rows"] == rows(db, query) == expected,
                    "metadata canonical read oracle")
            profile = execute(db, "PROFILE " + query)
            require(profile["rows"] == rows(db, "PROFILE " + query) == expected,
                    "metadata PROFILE oracle")
            require(any(op["operator"] == "LabelIndexScan" and op["dbHits"] > 0
                        for op in profile["summary"]["profile"]["operators"]),
                    "metadata fallback did not execute the reported scan")
            require(head(db) == original_head and db.total_changes == before,
                    "invalid metadata read wrote main or moved Branch")
        for _ in range(256):
            execute(db, "CALL lithograph.commit.create() YIELD commit RETURN commit")
            if head(db)[0] & 0x0f == 0:
                break
        else:
            raise AssertionError("did not exercise maintenance with invalid metadata")
        require(rows(db, query) == expected, "maintenance metadata fallback changed graph")
        db.execute("UPDATE main._lithograph_checkpoints SET metadata=? WHERE commit_id=?",
                   (valid, checkpoint))
        require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "metadata fixture integrity")
    finally:
        db.close()


def main(extension):
    with tempfile.TemporaryDirectory(prefix="lithograph-phase22-checkpoint-") as directory:
        root = Path(directory)
        publication = wal_publication(extension, root/"wal.sqlite3")
        rollback_journal(extension, root/"delete.sqlite3")
        readonly_reopen(extension, root/"wal.sqlite3")
    cold_branch_lazy_projection(extension)
    version_program_retains_its_original_snapshot(extension)
    reset_gc_retains_its_original_snapshot(extension)
    checkpoint_metadata_types(extension)
    print(json.dumps({"stage": "S22.2", "sqlite": sqlite3.sqlite_version,
                      **publication, "cgroup": cgroup(),
                      "checks": ["live-wal-read", "same-connection-writer-boundary",
                                 "rollback-journal-writer-rollback", "readonly-reopen",
                                 "cold-branch-lazy-projection", "version-program-snapshot-pin",
                                 "reset-gc-snapshot-pin", "checkpoint-metadata-types"]}))


if __name__ == "__main__":
    main(str(Path(sys.argv[1]).resolve()))
