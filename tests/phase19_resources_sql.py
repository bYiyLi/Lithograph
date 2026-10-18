#!/usr/bin/env python3
"""Phase 19 real SQLite resource regressions; synthetic in-memory data only."""

import argparse
import json
from pathlib import Path
import sqlite3
import threading
import time


MIB = 1024 * 1024


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def connect(extension):
    db = sqlite3.connect(":memory:", isolation_level=None)
    db.enable_load_extension(True)
    try:
        db.load_extension(str(extension))
    finally:
        db.enable_load_extension(False)
    db.execute("SELECT lithograph_init()").fetchone()
    return db


def scalar(db, query, options=None, params=None):
    return json.loads(db.execute("SELECT lithograph(?,?,?)", (
        query, json.dumps(params or {}), json.dumps(options or {}))).fetchone()[0])


def events(db, query, options=None, params=None):
    return db.execute("SELECT event,data FROM lithograph_rows(?,?,?)", (
        query, json.dumps(params or {}), json.dumps(options or {})))


def expect_error(operation, category, code=None):
    try:
        operation()
    except sqlite3.Error as error:
        require(category in str(error), f"expected {category}, got {error}")
        if code is not None:
            require(error.sqlite_errorcode == code,
                    f"expected SQLite code {code}, got {error.sqlite_errorcode}")
        return
    raise AssertionError(f"expected {category}")


def head(db):
    return db.execute(
        "SELECT commit_id FROM _lithograph_branches WHERE name='main'").fetchone()[0]


def test_options_and_ranges(db):
    for invalid in (None, 0, -1, 1.0, 1.5, True, "1024", [], {}, 9223372036854775808):
        for run in (lambda opts: scalar(db, "RETURN 1", opts),
                    lambda opts: list(events(db, "RETURN 1", opts))):
            expect_error(lambda: run({"memoryLimitBytes": invalid}), "INVALID_ARGUMENT")
    expect_error(lambda: scalar(db, "RETURN 1", {"memoryLimitBytes": 1}),
                 "RESOURCE_ERROR", sqlite3.SQLITE_TOOBIG)
    require(scalar(db, "RETURN 1", {"memoryLimitBytes": 9223372036854775807})["rows"] == [[1]],
            "maximum legal memory limit must be accepted")
    query = "RETURN size(range(1,500000)) AS c"
    for run in (lambda opts: scalar(db, query, opts),
                lambda opts: list(events(db, query, opts))):
        expect_error(lambda: run({"memoryLimitBytes": 16 * MIB}),
                     "RESOURCE_ERROR", sqlite3.SQLITE_TOOBIG)
        run({"memoryLimitBytes": 256 * MIB})
    require(scalar(db, query)["rows"] == [[500000]], "omitted budget changes behavior")
    require(scalar(db, query, {"memoryLimitBytes": 256 * MIB,
                              "readOnlyNoExternalIO": True})["rows"] == [[500000]],
            "read-only and resource options must combine")
    parameters = {"values": ["x" * 1024] * 10000}
    expect_error(lambda: scalar(db, "RETURN size($values)",
                               {"memoryLimitBytes": MIB}, parameters),
                 "RESOURCE_ERROR", sqlite3.SQLITE_TOOBIG)
    require(scalar(db, "RETURN 7")["rows"] == [[7]], "resource error poisons connection")


def test_rollback(db):
    base = head(db)
    for run in (lambda query: scalar(db, query, {"memoryLimitBytes": 16 * MIB}),
                lambda query: list(events(db, query, {"memoryLimitBytes": 16 * MIB}))):
        expect_error(lambda: run(
            "CREATE (:BudgetProbe) WITH 1 AS x RETURN size(range(1,500000))"),
            "RESOURCE_ERROR", sqlite3.SQLITE_TOOBIG)
        require(head(db) == base, "resource error published an ordinary write")
    db.execute("SELECT lithograph_tx_begin('{}')").fetchone()
    scalar(db, "CREATE (:StagedBudgetProbe) FINISH")
    expect_error(lambda: scalar(db, "RETURN size(range(1,500000))",
                               {"memoryLimitBytes": 16 * MIB}),
                 "RESOURCE_ERROR", sqlite3.SQLITE_TOOBIG)
    expect_error(lambda: db.execute("SELECT lithograph_tx_commit()").fetchone(),
                 "INVALID_ARGUMENT")
    require(head(db) == base, "resource error did not abort explicit transaction")
    require(scalar(db, "MATCH (n) RETURN count(n)")["rows"] == [[0]],
            "resource rollback left staged nodes")


def test_cursor_isolation(db):
    options = {"memoryLimitBytes": MIB}
    left = events(db, "UNWIND range(1,32) AS n RETURN n", options)
    right = events(db, "UNWIND range(1,32) AS n RETURN n", options)
    require(next(left)[0] == "columns" and next(right)[0] == "columns", "columns")
    for expected in range(1, 33):
        require(json.loads(next(left)[1]) == [expected], "left cursor")
        require(json.loads(next(right)[1]) == [expected], "right cursor")
    require(next(left)[0] == "summary" and next(right)[0] == "summary", "summaries")
    left.close()
    right.close()
    early = events(db, "UNWIND range(1,1000) AS n RETURN n", options)
    next(early)
    next(early)
    early.close()
    require(scalar(db, "RETURN 11", options)["rows"] == [[11]], "early close leaked budget")


def test_streaming_and_collector(db, row_count):
    # The range is intentionally materialized by Cypher; only final-result
    # retention differs between rows and scalar surfaces.
    query = (f"UNWIND range(1,{row_count}) AS n "
             f"RETURN {{n:n,payload:'{'x' * 128}'}} AS value")
    options = {"memoryLimitBytes": 256 * MIB}
    count = 0
    summary = False
    for event, data in events(db, query, options):
        if event == "row":
            count += 1
            require(json.loads(data)[0]["n"] == count, "stream row semantics")
        elif event == "summary":
            summary = True
    require(count == row_count and summary, "stream did not finish")
    if row_count >= 1000000:
        expect_error(lambda: scalar(db, query, options), "RESOURCE_ERROR", sqlite3.SQLITE_TOOBIG)
    require(scalar(db, "RETURN 13")["rows"] == [[13]], "collector failure poisoned connection")
    print(f"streamed {row_count} rows with a fixed 256 MiB budget")


def test_interrupt(db):
    query = ("RETURN reduce(s=0,x IN range(1,3000)|"
             "s+reduce(t=0,y IN range(1,3000)|t+y)) AS n")
    for surface in ("scalar", "rows"):
        interrupted = threading.Event()
        issued = []

        def cancel():
            time.sleep(0.05)
            issued.append(time.perf_counter())
            db.interrupt()
            interrupted.set()

        worker = threading.Thread(target=cancel)
        worker.start()
        started = time.perf_counter()
        try:
            try:
                if surface == "scalar":
                    scalar(db, query)
                else:
                    list(events(db, query))
            except sqlite3.DatabaseError as error:
                require(error.sqlite_errorcode == sqlite3.SQLITE_INTERRUPT,
                        f"interrupt primary code: {error}")
            else:
                raise AssertionError("expensive query completed before cancellation")
            finished = time.perf_counter()
        finally:
            worker.join()
        require(interrupted.is_set(), "interrupt was not sent")
        latency = finished - issued[0]
        require(0 <= latency < 0.5, f"{surface} interrupt took {latency:.3f}s")
        require(scalar(db, "RETURN 17")["rows"] == [[17]], "interrupt poisoned connection")
        print(f"{surface} interrupt latency {latency:.4f}s (total {finished-started:.4f}s)")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("extension", type=Path)
    parser.add_argument("--stream-rows", type=int, default=1000000)
    args = parser.parse_args()
    with connect(args.extension.resolve()) as db:
        test_options_and_ranges(db)
        test_rollback(db)
        test_cursor_isolation(db)
        test_streaming_and_collector(db, args.stream_rows)
        test_interrupt(db)
    print(f"Phase 19 resource SQL regressions passed (SQLite {sqlite3.sqlite_version})")


if __name__ == "__main__":
    main()
