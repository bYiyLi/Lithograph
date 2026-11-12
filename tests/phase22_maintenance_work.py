#!/usr/bin/env python3
"""Checkpoint crash recovery and two builds on a copied FX-HISTORY fixture.

The input is a closed, synthetic Keep.v=0 database without checkpoints. Only
new private copies are written or crashed. Run in the 1 GiB/no-swap Linux
runtime separately from compilation; the checkpoint tool reports its SQLite.
"""

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import time

from phase21_predicate_routing_sql import execute, require
from phase22_access_work import cgroup, file_hash, open_db
from phase22_history_work import build_checkpoint, timed_read


def emit(value):
    print(json.dumps(value), flush=True)


def crash_and_recover(extension, tool, source, path, original):
    require(not path.exists(), "crash path must be new")
    shutil.copyfile(source, path)
    child = subprocess.Popen([str(tool.resolve()), str(path.resolve())],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    deadline = time.monotonic() + 60
    journal = Path(str(path) + "-journal")
    injected = False
    try:
        while child.poll() is None and time.monotonic() < deadline:
            try:
                journal_bytes = journal.stat().st_size
            except FileNotFoundError:
                journal_bytes = 0
            if journal_bytes > 10000:
                # Kill only the child created above, after real journal writes.
                child.kill()
                injected = True
                break
            time.sleep(.01)
        output, error = child.communicate(timeout=60)
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
    require(injected and child.returncode == -9,
            "crash did not interrupt a live maintenance writer: " + output + error)
    db = open_db(extension, str(path))
    require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "crash quick_check")
    require(db.execute("SELECT count(*) FROM _lithograph_checkpoints").fetchone() == (0,),
            "partial checkpoint survived crash")
    timed_read(db, 0)
    db.close()
    require(file_hash(path) == original, "crash recovery changed canonical database")
    emit({"kind": "maintenance_crash_recovery", "exit": child.returncode,
          "sha256": original, "cgroup": cgroup()})


def build_twice(extension, tool, source, path):
    require(not path.exists(), "build path must be new")
    shutil.copyfile(source, path)
    for number in (1, 2):
        result = build_checkpoint(tool, path)
        require(result["builds"] == 1, "maintenance did not build")
        if number == 2:
            require(result["ancestor_copies"] == 1 and result["layers_replayed"] == 2,
                    "maintenance did not reuse ancestor")
        db = open_db(extension, str(path))
        timed_read(db, 0)
        require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "maintenance quick_check")
        emit({"kind": "maintenance_build", "build": number, "measurement": result,
              "bytes": path.stat().st_size, "cgroup": cgroup()})
        if number == 1:
            execute(db, "MATCH (n:Keep) WHERE elementId(n)='n:1' SET n.probe=1 FINISH")
            execute(db, "MATCH (n:Keep) WHERE elementId(n)='n:1' REMOVE n.probe FINISH")
        db.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extension", type=Path)
    parser.add_argument("checkpoint_tool", type=Path)
    parser.add_argument("source", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    require(not args.destination.exists(), "maintenance directory must be new")
    for suffix in ("-wal", "-journal"):
        sidecar = Path(str(args.source) + suffix)
        require(not sidecar.exists() or sidecar.stat().st_size == 0, "input fixture is not closed")
    db = open_db(args.extension, str(args.source), True)
    require(db.execute("SELECT count(*) FROM _lithograph_checkpoints").fetchone() == (0,),
            "input fixture must not have checkpoints")
    timed_read(db, 0)
    db.close()
    original = file_hash(args.source)
    args.destination.mkdir(parents=True)
    crash_and_recover(args.extension, args.checkpoint_tool, args.source,
                      args.destination / "crash.sqlite3", original)
    build_twice(args.extension, args.checkpoint_tool, args.source,
                args.destination / "build.sqlite3")
    require(file_hash(args.source) == original, "maintenance modified source")


if __name__ == "__main__":
    main()
