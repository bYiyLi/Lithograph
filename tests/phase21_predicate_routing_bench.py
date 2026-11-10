#!/usr/bin/env python3
"""Reproducible Phase 21 workload with and without a derived checkpoint.

The seed and checkpoint are built in separate processes from the query worker.
This keeps fixture construction and checkpoint creation out of query RSS.
Both modes run against equivalent immutable Commit content.
"""

from __future__ import annotations

import json
import os
import platform
import resource
import shutil
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path


def cypher(db: sqlite3.Connection, query: str, params=None) -> dict:
    return json.loads(
        db.execute(
            "SELECT lithograph(?,?)", (query, json.dumps(params or {}))
        ).fetchone()[0]
    )


def measured(db: sqlite3.Connection, query: str, params=None) -> tuple[dict, float]:
    started = time.perf_counter()
    result = cypher(db, query, params)
    return result, (time.perf_counter() - started) * 1000


def cgroup_metrics() -> dict:
    result = {}
    for key in ("memory.max", "memory.peak", "memory.swap.max", "memory.events"):
        path = Path("/sys/fs/cgroup") / key
        if path.exists():
            result[key] = path.read_text().strip()
    return result


def seed_worker(extension: str, database: str, size: int) -> dict:
    path = Path(database)
    with sqlite3.connect(path) as db:
        db.execute("PRAGMA cache_size=-2000")
        db.enable_load_extension(True)
        db.load_extension(str(Path(extension).resolve()))
        db.enable_load_extension(False)
        db.execute("SELECT lithograph_init()").fetchone()
        seed = (
            f"UNWIND range(0,{size-1}) AS k "
            "CREATE (:Bench {k:k}) FINISH"
        )
        created, seed_ms = measured(db, seed)
        assert created["summary"]["counters"]["nodesCreated"] == size
        count, _ = measured(db, "MATCH (n:Bench) RETURN count(n)")
        assert count["rows"] == [[size]], count
        # Make the closed fixture self-contained before the second copy.
        db.execute("PRAGMA journal_mode=DELETE").fetchone()
    return {
        "size": size,
        "seed_ms": round(seed_ms, 2),
        "sqlite": sqlite3.sqlite_version,
        "database_bytes": path.stat().st_size,
        "commit": created["summary"]["commit"],
    }


def query_worker(
    extension: str, database: str, expected_checkpoint: int, expected_node_count: int
) -> dict:
    path = Path(database)
    with sqlite3.connect(path) as db:
        db.execute("PRAGMA cache_size=-2000")
        db.enable_load_extension(True)
        db.load_extension(str(Path(extension).resolve()))
        db.enable_load_extension(False)
        db.execute("SELECT lithograph_init()").fetchone()

        checkpoint_count = db.execute(
            "SELECT count(*) FROM main._lithograph_checkpoints"
        ).fetchone()[0]
        metadata_row = db.execute(
            "SELECT metadata FROM main._lithograph_checkpoints LIMIT 1"
        ).fetchone()
        metadata = metadata_row[0] if metadata_row else None
        checkpoint_nodes = json.loads(metadata)["nodes"] if metadata else 0
        layer_count = db.execute(
            "SELECT count(*) FROM main._lithograph_layers"
        ).fetchone()[0]
        assert checkpoint_count == expected_checkpoint, (
            checkpoint_count,
            expected_checkpoint,
        )
        assert checkpoint_nodes == (expected_node_count if expected_checkpoint else 0)
        node_count = expected_node_count

        params = {"a": "n:1", "b": "n:2"}
        seek_query = (
            "MATCH (a:Bench), (b:Bench) "
            "WHERE elementId(a)=$a AND elementId(b)=$b "
            "RETURN a.k,b.k"
        )
        explain = cypher(db, "EXPLAIN " + seek_query, params)["rows"][0][0]
        assert explain.count("NodeIdSeek") == 2, explain
        assert explain.index("Filter") < explain.index("Cartesian"), explain
        profiled, first_ms = measured(db, "PROFILE " + seek_query, params)
        assert profiled["rows"] == [[0, 1]]
        seek_hits = profiled["summary"]["metrics"]["dbHits"]
        assert seek_hits <= 12, seek_hits
        access = [
            op["dbHits"]
            for op in profiled["summary"]["profile"]["operators"]
            if op["operator"] == "NodeIdSeek"
        ]
        assert len(access) == 2 and all(hit > 0 for hit in access), access

        warmed = []
        for _ in range(5):
            result, elapsed = measured(db, seek_query, params)
            assert result["rows"] == [[0, 1]]
            assert result["summary"]["metrics"]["dbHits"] <= 12
            warmed.append(elapsed)

        property_query = (
            "MATCH (a:Bench), (b:Bench) "
            "WHERE a.k=0 AND b.k=1 RETURN a.k,b.k"
        )
        property_plan = cypher(db, "EXPLAIN " + property_query)["rows"][0][0]
        assert property_plan.index("Filter") < property_plan.index("Cartesian")
        selected, property_ms = measured(db, "PROFILE " + property_query)
        assert selected["rows"] == [[0, 1]], selected
        property_hits = selected["summary"]["metrics"]["dbHits"]
        assert property_hits <= 15 * node_count + 200, (property_hits, node_count)

        missing, missing_ms = measured(
            db,
            "PROFILE MATCH (a:Bench), (b:Bench) "
            "WHERE elementId(a)=$a AND elementId(b)=$b RETURN a.k,b.k",
            {"a": "n:999999999", "b": "n:2"},
        )
        assert missing["rows"] == [] and missing["summary"]["metrics"]["dbHits"] <= 12

        cancel_ms = None
        if node_count >= 10000:
            timer = threading.Timer(0.001, db.interrupt)
            started = time.perf_counter()
            timer.start()
            try:
                measured(db, "MATCH (n:Bench) RETURN n.k")
            except sqlite3.Error as error:
                if getattr(error, "sqlite_errorcode", None) != sqlite3.SQLITE_INTERRUPT:
                    raise
                cancel_ms = (time.perf_counter() - started) * 1000
            finally:
                timer.join()
            assert cancel_ms is not None, "large read must observe host cancellation"
            recovered, _ = measured(
                db,
                "MATCH (n:Bench) WHERE elementId(n)='n:1' RETURN n.k",
            )
            assert recovered["rows"] == [[0]], "connection must remain usable after cancellation"

        write, write_ms = measured(
            db,
            "MATCH (a:Bench), (b:Bench) "
            "WHERE elementId(a)=$a AND elementId(b)=$b "
            "CREATE (a)-[:BENCH_LINK]->(b) FINISH",
            params,
        )
        assert write["summary"]["counters"]["relationshipsCreated"] == 1
        count_relationship, _ = measured(
            db, "MATCH ()-[r:BENCH_LINK]->() RETURN count(r)"
        )
        assert count_relationship["rows"] == [[1]]

        peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        peak_bytes = peak if sys.platform == "darwin" else peak * 1024
        return {
            "checkpointed": bool(checkpoint_count),
            "checkpoint_count": checkpoint_count,
            "checkpoint_nodes": checkpoint_nodes,
            "node_count": node_count,
            "layers": layer_count,
            "first_profile_ms": round(first_ms, 2),
            "read_p50_ms": round(statistics.median(warmed), 2),
            "read_max_ms": round(max(warmed), 2),
            "node_seek_db_hits": seek_hits,
            "node_seek_operator_db_hits": access,
            "property_pushdown_db_hits": property_hits,
            "property_profile_ms": round(property_ms, 2),
            "missing_profile_ms": round(missing_ms, 2),
            "cancelled": cancel_ms is not None,
            "cancel_ms": round(cancel_ms, 2) if cancel_ms is not None else None,
            "write_ms": round(write_ms, 2),
            "max_rss_mib": round(peak_bytes / (1024 * 1024), 2),
            "database_bytes": path.stat().st_size,
            "temp_pages": db.execute("PRAGMA temp.page_count").fetchone()[0],
            "cgroup": cgroup_metrics(),
        }


def run_json(
    command: list[str], timeout: int = 600, cwd: Path | None = None
) -> dict:
    result = subprocess.run(
        command,
        check=False,
        capture_output=True,
        text=True,
        timeout=timeout,
        cwd=cwd,
    )
    if result.returncode:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(command)}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return json.loads(result.stdout)


def main() -> None:
    if len(sys.argv) == 5 and sys.argv[1] == "--seed-worker":
        print(json.dumps(seed_worker(sys.argv[2], sys.argv[3], int(sys.argv[4]))))
        return
    if len(sys.argv) == 6 and sys.argv[1] == "--query-worker":
        print(
            json.dumps(
                query_worker(
                    sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5])
                ),
                sort_keys=True,
            )
        )
        return
    if len(sys.argv) < 3:
        raise SystemExit(
            "usage: phase21_predicate_routing_bench.py <extension> <sizes ...>"
        )

    root = Path(__file__).resolve().parents[1]
    extension = str(Path(sys.argv[1]).resolve())
    python = sys.executable
    for size in map(int, sys.argv[2:]):
        started = time.perf_counter()
        with tempfile.TemporaryDirectory(prefix="lithograph-phase21-") as temp:
            base = Path(temp) / "base.sqlite3"
            no_checkpoint = Path(temp) / "no-checkpoint.sqlite3"
            checkpointed = Path(temp) / "checkpointed.sqlite3"
            seed = run_json(
                [python, str(Path(__file__).resolve()), "--seed-worker", extension, str(base), str(size)],
                cwd=root,
            )
            shutil.copy2(base, no_checkpoint)
            shutil.copy2(base, checkpointed)

            checkpoint_started = time.perf_counter()
            helper = os.environ.get("LITHOGRAPH_PHASE21_CHECKPOINT_HELPER")
            checkpoint_command = (
                [helper, str(checkpointed)]
                if helper
                else [
                    "cargo",
                    "run",
                    "--locked",
                    "--quiet",
                    "-p",
                    "lithograph-test-support",
                    "--bin",
                    "lithograph-phase21-checkpoint",
                    "--",
                    str(checkpointed),
                ]
            )
            checkpoint = run_json(checkpoint_command, timeout=600, cwd=root)
            checkpoint_ms = (time.perf_counter() - checkpoint_started) * 1000
            assert checkpoint["node_count"] == size, checkpoint

            uncheckpointed_result = run_json(
                [
                    python,
                    str(Path(__file__).resolve()),
                    "--query-worker",
                    extension,
                    str(no_checkpoint),
                    "0",
                    str(size),
                ],
                cwd=root,
            )
            checkpointed_result = run_json(
                [
                    python,
                    str(Path(__file__).resolve()),
                    "--query-worker",
                    extension,
                    str(checkpointed),
                    "1",
                    str(size),
                ],
                cwd=root,
            )

        report = {
            "size": size,
            "sqlite": seed["sqlite"],
            "platform": platform.platform(),
            "seed_ms": seed["seed_ms"],
            "checkpoint_ms": round(checkpoint_ms, 2),
            "checkpoint": checkpoint,
            "uncheckpointed": uncheckpointed_result,
            "checkpointed": checkpointed_result,
            "total_elapsed_s": round(time.perf_counter() - started, 2),
        }
        print(json.dumps(report, sort_keys=True), flush=True)


if __name__ == "__main__":
    main()
