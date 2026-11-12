#!/usr/bin/env python3
"""S22.1 reproducible physical-access and bounded-runtime workload.

Seed/checkpoint construction runs separately from each query worker. Workers
open the immutable fixture read-only; all output rows are consumed, never kept.
"""

import argparse
import concurrent.futures
import hashlib
import json
import platform
import random
from pathlib import Path
import resource
import shutil
import tempfile
import sqlite3
import statistics
import threading
import time

from phase21_predicate_routing_sql import execute, require

VIEW = {"graphView": {"requireAllLabels": ["Rare"]}}
QUERY = "MATCH (n:Common) WHERE n.v=$x RETURN n.k ORDER BY n.k"


def file_hash(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


class Latency:
    """Bounded deterministic reservoir; totals/max cover every invocation."""

    def __init__(self):
        self.count = 0
        self.total = self.maximum = 0
        self.values = []
        self.random = random.Random(22)

    def add(self, micros):
        self.count += 1
        self.total += micros
        self.maximum = max(self.maximum, micros)
        if len(self.values) < 8192:
            self.values.append(micros)
        else:
            slot = self.random.randrange(self.count)
            if slot < len(self.values):
                self.values[slot] = micros

    def report(self):
        values = sorted(self.values)
        return {"invocations": self.count, "sample_count": len(values),
                "mean_us": self.total / max(1, self.count), "max_us": self.maximum,
                "sample_p50_us": statistics.median(values) if values else None,
                "sample_p95_us": values[min(len(values)-1, int(len(values)*.95))] if values else None}


def open_db(extension, path, readonly=False):
    db = sqlite3.connect(f"file:{Path(path).resolve()}?mode=ro" if readonly else path,
                         uri=readonly, isolation_level=None)
    db.enable_load_extension(True)
    db.execute("SELECT load_extension(?,?)", (str(Path(extension).resolve()), "sqlite3_lithograph_init"))
    db.enable_load_extension(False)
    db.execute("PRAGMA cache_size=-2000")
    return db


def seed(extension, path, size):
    require(not Path(path).exists(), "seed requires a new disposable path")
    db = open_db(extension, path)
    db.execute("SELECT lithograph_init()").fetchone()
    started = time.perf_counter()
    result = execute(db, f"UNWIND range(0,{size-1}) AS i CREATE (:Common {{v:0,k:i}}) FINISH")
    require(result["summary"]["counters"]["nodesCreated"] == size, "seed cardinality")
    execute(db, "MATCH (n:Common) WHERE elementId(n)='n:1' OR elementId(n)='n:2' SET n:Rare FINISH")
    execute(db, "CREATE CONSTRAINT value_type FOR (n:Common) REQUIRE n.v IS :: INTEGER")
    result = execute(db, "CREATE RANGE INDEX values_range FOR (n:Common) ON (n.v)")
    db.close()
    return {"kind": "seed", "size": size, "seed_seconds": time.perf_counter()-started,
            "commit": result["summary"]["commit"]}


def drain(db, text, options, expected):
    start = time.perf_counter_ns()
    first = None
    count = checksum = 0
    summary = None
    for event, data in db.execute("SELECT event,data FROM lithograph_rows(?,?,?)", (text, '{"x":0}', json.dumps(options))):
        if event == "row":
            if first is None:
                first = time.perf_counter_ns()-start
            row = json.loads(data)
            checksum += row[0]
            count += 1
        elif event == "summary":
            summary = json.loads(data)
    require(summary is not None and count == expected, "incomplete/incorrect stream")
    require(checksum == expected*(expected-1)//2, "stream values/multiplicity")
    return {"first_row_us": None if first is None else first/1000,
            "full_us": (time.perf_counter_ns()-start)/1000, "rows": count,
            "db_hits": summary["metrics"]["dbHits"]}


def cgroup():
    root = Path("/sys/fs/cgroup")
    return {name: (root/name).read_text().strip() for name in
            ("memory.max", "memory.swap.max", "memory.peak", "memory.events") if (root/name).exists()}


def measure(extension, path, size, connections, repeats, reopens, access_query=QUERY):
    samples = []
    before = file_hash(path)
    plans = []
    for _ in range(connections):
        db = open_db(extension, path, True)
        # First invocation is measured too; EXPLAIN cannot hide cache setup.
        for _ in range(repeats):
            samples.append(drain(db, access_query, VIEW, 2))
        plans.append(execute(db, "EXPLAIN " + access_query, {"x": 0}, VIEW)["rows"][0][0])
        db.close()
    fresh = []
    for _ in range(reopens):
        db = open_db(extension, path, True)
        fresh.append(drain(db, access_query, VIEW, 2))
        db.close()
    db = open_db(extension, path, True)
    long = [drain(db, "MATCH (n:Common) RETURN n.k", {"memoryLimitBytes": 8*1024*1024}, size) for _ in range(3)]
    # A large lawful Cartesian is consumed only until a real host interrupt.
    cursor = db.execute("SELECT event,data FROM lithograph_rows(?)", ("MATCH (a:Common),(b:Common) RETURN a.k+b.k",))
    timer = threading.Timer(0.03, db.interrupt)
    timer.start()
    cancelled = False
    start = time.perf_counter()
    try:
        for _ in cursor:
            pass
    except sqlite3.Error as error:
        require(error.sqlite_errorcode == sqlite3.SQLITE_INTERRUPT, str(error))
        cancelled = True
    finally:
        timer.join()
        cursor.close()
    cancel_seconds = time.perf_counter()-start
    require(cancelled, "cancel fixture completed before interruption")
    require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "reopen integrity")
    db.close()
    require(file_hash(path) == before, "read path modified main")
    def stats(values):
        times = sorted(value["full_us"] for value in values)
        return {"n": len(times), "first_row_p50_us": statistics.median(v["first_row_us"] for v in values), "first_row_p95_us": sorted(v["first_row_us"] for v in values)[min(len(values)-1, int(len(values)*0.95))], "p50_us": statistics.median(times),
                "p95_us": times[min(len(times)-1, int(len(times)*0.95))], "max_us": max(times)} if times else {}
    peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return {"kind": "query", "access_query": access_query, "size": size, "platform": platform.platform(), "sqlite": sqlite3.sqlite_version,
            "extension_sha256": file_hash(extension),
            "fixture_sha256": before, "plans": sorted(set(plans)), "warm": stats(samples), "new_connection": stats(fresh),
            "access_db_hits": sorted(set(s["db_hits"] for s in samples)), "long_streams": long,
            "cancel_seconds": cancel_seconds, "peak_rss_bytes": peak if platform.system()=="Darwin" else peak*1024,
            "cgroup": cgroup()}


def mixed_stress(extension, source, size, seconds, readers, scratch):
    """One writer and independent readers; the source graph stays immutable."""
    original = file_hash(source)
    Path(scratch).mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="phase22-", dir=scratch) as directory:
        path = str(Path(directory)/"mixed.sqlite3")
        shutil.copyfile(source, path)
        db = open_db(extension, path)
        require(db.execute("PRAGMA journal_mode=WAL").fetchone() == ("wal",), "WAL fixture")
        base = execute(db, "MATCH (n:Rare) SET n.hit=0 FINISH")["summary"]["commit"]
        db.close()
        deadline = time.monotonic()+seconds
        started = time.monotonic()
        stop = threading.Event()

        def reader(_):
            db = open_db(extension, path, True)
            counts = {"reads": 0, "early_close": 0, "interrupts": 0}
            latency = Latency()
            try:
                while time.monotonic() < deadline and not stop.is_set():
                    latency.add(drain(db, QUERY, VIEW, 2)["full_us"])
                    require(execute(db, QUERY, {"x": -1}, VIEW)["rows"] == [], "zero result")
                    require(execute(db, QUERY, {"x": 0}, dict(VIEW, at=base))["rows"] == [[0], [1]], "historical result")
                    cursor = db.execute("SELECT event,data FROM lithograph_rows(?)", ("MATCH (n:Common) RETURN n.k",))
                    require(cursor.fetchone()[0] == "columns" and cursor.fetchone()[0] == "row", "early row")
                    cursor.close()
                    counts["early_close"] += 1
                    counts["reads"] += 3
                    if counts["early_close"] % 64 == 1:
                        cursor = db.execute("SELECT event,data FROM lithograph_rows(?)", ("MATCH (a:Common),(b:Common) RETURN a.k+b.k",))
                        timer = threading.Timer(0.03, db.interrupt)
                        timer.start()
                        try:
                            for _ in cursor:
                                pass
                            raise AssertionError("interrupt workload completed")
                        except sqlite3.Error as error:
                            require(error.sqlite_errorcode == sqlite3.SQLITE_INTERRUPT, str(error))
                            counts["interrupts"] += 1
                        finally:
                            timer.join()
                            cursor.close()
                return dict(counts, read_latency=latency.report(), unexpected_failures=0)
            except BaseException:
                stop.set()
                raise
            finally:
                db.close()

        def writer():
            db = open_db(extension, path)
            counts = {"writes": 0, "failed_writes": 0}
            wait, hold = Latency(), Latency()
            peak_wal = 0
            try:
                while time.monotonic() < deadline and not stop.is_set():
                    value = counts["writes"]+1
                    waiting = time.perf_counter_ns()
                    db.execute("BEGIN IMMEDIATE")
                    acquired = time.perf_counter_ns()
                    wait.add((acquired-waiting)/1000)
                    result = execute(db, "WITH $hit AS seed MATCH (n:Rare) SET n.hit=seed RETURN n.k ORDER BY n.k", {"hit": value})
                    db.execute("COMMIT")
                    hold.add((time.perf_counter_ns()-acquired)/1000)
                    require(result["rows"] == [[0], [1]], "write result")
                    try:
                        execute(db, "WITH 1 AS seed MATCH (n:Rare) SET n.hit=999999 SET n.hit=1/0 FINISH")
                        raise AssertionError("failed write succeeded")
                    except sqlite3.Error as error:
                        require("TYPE_ERROR" in str(error), str(error))
                    require(execute(db, "MATCH (n:Rare) RETURN n.hit ORDER BY n.hit")["rows"] == [[value], [value]], "failed write published")
                    counts["writes"] += 1
                    counts["failed_writes"] += 1
                    wal = Path(path+"-wal")
                    peak_wal = max(peak_wal, wal.stat().st_size if wal.exists() else 0)
                    stop.wait(0.1)
                return dict(counts, writer_wait=wait.report(), writer_hold=hold.report(),
                            peak_wal_bytes=peak_wal, unexpected_failures=0)
            except BaseException:
                stop.set()
                raise
            finally:
                if db.in_transaction:
                    db.rollback()
                db.close()

        with concurrent.futures.ThreadPoolExecutor(max_workers=readers+1) as pool:
            futures = [pool.submit(reader, i) for i in range(readers)] + [pool.submit(writer)]
            outcomes = [future.result() for future in futures]
        require(all(outcome.get("reads", outcome.get("writes", 0)) > 0 for outcome in outcomes), "empty stress worker")
        db = open_db(extension, path)
        require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "stress integrity")
        require(execute(db, "MATCH (n:Common) RETURN count(*)")["rows"] == [[size]], "stress graph cardinality")
        db.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchall()
        db.close()
        peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        result = {"kind": "mixed_stress", "readers": readers, "seconds": seconds, "size": size,
                  "elapsed_seconds": time.monotonic()-started,
                  "outcomes": outcomes, "cgroup": cgroup(),
                  "peak_rss_bytes": peak if platform.system()=="Darwin" else peak*1024,
                  "main_bytes": Path(path).stat().st_size,
                  "extension_sha256": file_hash(extension)}
    require(file_hash(source) == original, "stress changed original")
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("seed", "query", "stress"))
    parser.add_argument("extension")
    parser.add_argument("database")
    parser.add_argument("size", type=int)
    parser.add_argument("--access-query", default=QUERY)
    parser.add_argument("--connections", type=int, default=5)
    parser.add_argument("--repeats", type=int, default=20)
    parser.add_argument("--reopens", type=int, default=100)
    parser.add_argument("--seconds", type=int, default=60)
    parser.add_argument("--readers", type=int, default=4)
    parser.add_argument("--scratch-root", default="target/phase22-s1/stress")
    args = parser.parse_args()
    result = None
    if args.mode == "seed":
        result = seed(args.extension, args.database, args.size)
    elif args.mode == "query":
        result = measure(args.extension, args.database, args.size, args.connections, args.repeats, args.reopens, args.access_query)
    if args.mode == "stress":
        result = mixed_stress(args.extension, args.database, args.size, args.seconds, args.readers, args.scratch_root)
    print(json.dumps(result, sort_keys=True))
