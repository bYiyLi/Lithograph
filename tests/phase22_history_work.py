#!/usr/bin/env python3
"""Disposable Phase 22 history/checkpoint workload with complete read oracles.

All paths must be new. Only copied synthetic fixtures have their derived rows
removed to exercise fallback. No canonical internal table is mutated directly.
"""

import argparse
import json
import platform
from pathlib import Path
import resource
import shutil
import sqlite3
import subprocess
import sys
import time

from phase22_access_work import Latency, cgroup, file_hash as digest, open_db
from phase21_predicate_routing_sql import execute, require

CHECKPOINT_TABLES = ("_lithograph_cp_properties", "_lithograph_cp_relationships",
                     "_lithograph_cp_labels", "_lithograph_cp_nodes", "_lithograph_checkpoints")
QUERY = "MATCH (n:Keep) WHERE elementId(n)='n:1' RETURN n.v"


def timed_read(db, expected):
    started = time.perf_counter_ns()
    rows = []
    terminal = False
    for event, data in db.execute("SELECT event,data FROM lithograph_rows(?)", (QUERY,)):
        if event == "row":
            rows.append(json.loads(data))
        elif event == "summary":
            terminal = True
    require(terminal and rows == [[expected]], f"incomplete/history result: {rows}")
    return (time.perf_counter_ns()-started)/1000


def checkpoint_count(db):
    return db.execute("SELECT count(*) FROM main._lithograph_checkpoints").fetchone()[0]


def seed(extension, path, nodes, commits, payload_bytes, payload_distribution):
    require(not path.exists(), "history seed must be a new disposable database")
    db = open_db(extension, str(path))
    db.execute("SELECT lithograph_init()")
    execute(db, f"UNWIND range(0,{nodes-1}) AS i CREATE (:Keep {{k:i,v:0}}) FINISH")
    if payload_bytes:
        if payload_distribution == "single":
            execute(db, "MATCH (n:Keep) WHERE elementId(n)='n:1' SET n.payload=$payload FINISH",
                    {"payload": "p"*payload_bytes})
        else:
            per_node = (payload_bytes+nodes-1)//nodes
            execute(db, "MATCH (n:Keep) SET n.payload=$payload FINISH", {"payload": "p"*per_node})
            payload_bytes = per_node*nodes
    latency = Latency()
    maintenance = []
    previous = checkpoint_count(db)
    started = time.monotonic()
    for at in range(1, commits+1):
        writer = time.perf_counter_ns()
        result = execute(db, "MATCH (n:Keep) WHERE elementId(n)='n:1' SET n.v=$v FINISH", {"v": at % 2})
        elapsed = (time.perf_counter_ns()-writer)/1000
        latency.add(elapsed)
        current = checkpoint_count(db)
        if current != previous:
            maintenance.append({"at": at, "checkpoints": current, "writer_us": elapsed})
            previous = current
    timed_read(db, commits % 2)
    require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "history seed integrity")
    automatic = db.execute(
        "SELECT count(*) FROM main._lithograph_checkpoints WHERE json_valid(CAST(metadata AS TEXT)) "
        "AND json_extract(CAST(metadata AS TEXT),'$.automatic')=1"
    ).fetchone()[0]
    require(automatic <= 4, "automatic checkpoint retention exceeded its bound")
    db.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchall()
    db.close()
    return {"kind": "history_seed", "nodes": nodes, "commits": commits,
            "payload_bytes": payload_bytes, "elapsed_s": time.monotonic()-started,
            "payload_distribution": payload_distribution,
            "commit": result["summary"]["commit"], "writer_invocation": latency.report(),
            "automatic_checkpoints": automatic, "checkpoint_events": maintenance,
            "bytes": path.stat().st_size, "sha256": digest(path)}


def copy_without_checkpoints(extension, source, destination):
    require(not destination.exists(), "history copy destination already exists")
    shutil.copyfile(source, destination)
    db = open_db(extension, str(destination))
    # Deliberate derived-state loss, confined to this disposable fixture copy.
    for table in CHECKPOINT_TABLES:
        db.execute(f"DELETE FROM main.{table}")
    require(checkpoint_count(db) == 0, "failed to remove fixture checkpoints")
    db.close()


def read_samples(extension, path, expected, reopens):
    original = digest(path)
    warm, fresh = Latency(), Latency()
    for _ in range(5):
        db = open_db(extension, str(path), True)
        for _ in range(20):
            warm.add(timed_read(db, expected))
        db.close()
    for _ in range(reopens):
        db = open_db(extension, str(path), True)
        fresh.add(timed_read(db, expected))
        require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "reopen integrity")
        db.close()
    require(original == digest(path), "historical read modified main")
    return {"warm": warm.report(), "reopen": fresh.report(), "sha256": original}


def physical_read(vm_tool, extension, path, expected):
    result = subprocess.run(
        [str(vm_tool), str(extension), str(path), QUERY+" - $expected",
         json.dumps({"expected": expected}), "{}"], check=True, text=True, capture_output=True)
    return json.loads(result.stdout)


def build_checkpoint(tool, path):
    before = path.stat().st_size
    started = time.monotonic()
    # A fresh worker has exactly one child, giving this build's own RSS peak
    # rather than the parent's cumulative maximum across earlier children.
    result = subprocess.run([sys.executable, "-B", __file__, "--checkpoint-worker", str(tool), str(path)],
                            check=True, text=True, capture_output=True)
    return dict(json.loads(result.stdout), wall_seconds=time.monotonic()-started,
                file_bytes_before=before, file_bytes_after=path.stat().st_size)


def checkpoint_worker(tool, path):
    result = subprocess.run([tool, path], check=True, text=True, capture_output=True)
    data = json.loads(result.stdout)
    peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    data["build_peak_rss_bytes"] = peak if platform.system() == "Darwin" else peak*1024
    print(json.dumps(data))


def compare(args, path, commits):
    base = path.with_name(path.stem+"-manual.sqlite3")
    missing = path.with_name(path.stem+"-missing.sqlite3")
    require(not base.exists(), "manual checkpoint copy already exists")
    shutil.copyfile(path, base)
    copy_without_checkpoints(args.extension, path, missing)
    first_build = build_checkpoint(args.checkpoint_tool, base)
    # Add and remove a temporary property: the final graph is exactly the same
    # at every history depth and for every checkpoint state being compared.
    db = open_db(args.extension, str(base))
    execute(db, "MATCH (n:Keep) WHERE elementId(n)='n:1' SET n.checkpointProbe=1 FINISH")
    execute(db, "MATCH (n:Keep) WHERE elementId(n)='n:1' REMOVE n.checkpointProbe FINISH")
    db.close()
    second_build = build_checkpoint(args.checkpoint_tool, base)
    require(second_build["ancestor_copies"] == 1 and second_build["layers_replayed"] == 2,
            "second checkpoint did not reuse its compatible first-parent base")
    samples = {}
    for name, source, expected in (("automatic", path, commits % 2), ("missing", missing, commits % 2),
                                   ("manual", base, commits % 2)):
        work = physical_read(args.vm_tool, args.extension, source, expected)
        samples[name] = dict(read_samples(args.extension, source, expected, args.reopens), **work)
    require(samples["manual"]["vm_steps"] < samples["missing"]["vm_steps"],
            "checkpoint did not reduce actual SQLite VM work")
    return {"kind": "history_compare", "commits": commits, "first_build": first_build,
            "second_build": second_build, "reads": samples}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("extension", type=lambda s: Path(s).resolve())
    parser.add_argument("directory", type=lambda s: Path(s).resolve())
    parser.add_argument("--checkpoint-tool", required=True, type=lambda s: Path(s).resolve())
    parser.add_argument("--vm-tool", required=True, type=lambda s: Path(s).resolve())
    parser.add_argument("--nodes", type=int, default=1000)
    parser.add_argument("--commits", type=int, nargs="+", default=[10, 100, 1000, 10000])
    parser.add_argument("--payload-bytes", type=int, default=0)
    parser.add_argument("--payload-distribution", choices=["single", "nodes"], default="single")
    parser.add_argument("--reopens", type=int, default=100)
    args = parser.parse_args()
    require(args.nodes > 0 and min(args.commits) > 0 and args.payload_bytes >= 0, "invalid workload size")
    args.directory.mkdir(parents=True, exist_ok=True)
    print(json.dumps({"kind": "environment", "sqlite": sqlite3.sqlite_version,
                      "extension_sha256": digest(args.extension), "cgroup": cgroup()}), flush=True)
    for commits in args.commits:
        path = args.directory/f"history-{commits}.sqlite3"
        print(json.dumps(seed(args.extension, path, args.nodes, commits, args.payload_bytes,
                              args.payload_distribution)), flush=True)
        print(json.dumps(compare(args, path, commits)), flush=True)


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "--checkpoint-worker":
        checkpoint_worker(sys.argv[2], sys.argv[3])
    else:
        main()
