#!/usr/bin/env python3
"""S22.3 NodeId/ready-index scale and bounded full-stream resource workload.

Seed only new synthetic 1k/10k/100k paths through public writes, then build a
manual checkpoint without moving main. The existing 1M fixture is read-only
and pinned to its verified index-generation anchor, never its later main head.
Default point samples are 5 connections x 20 warm reads and 100 independently
opened first queries PER case. Long reads validate every ID/value with a small
bitset, with no ORDER BY, LIMIT, count replacement, or retained result list.

Examples (run seeding outside the later constrained resource container):
  python3 -B tests/phase22_statistics_work.py EXT OUT --checkpoint-tool TOOL
  python3 -B tests/phase22_statistics_work.py EXT OUT --reuse --skip-long
  python3 -B tests/phase22_statistics_work.py EXT OUT --reuse --long-only
  python3 -B tests/phase22_statistics_work.py EXT OUT --sizes 1000 --smoke
"""

import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import sqlite3
import statistics
import subprocess
import sys
import time

from phase21_predicate_routing_sql import execute, require
from phase22_access_work import cgroup, file_hash, open_db
from phase22_relationship_work import TRACE, percentiles, select_sqlite

ROOT = Path(__file__).resolve().parent.parent
MILLION_CP = "f84a434a72de50b525e5574388790dcb84158c1f50ea04ef7dfab766ea87d957"
MILLION_ANCHOR = "44f99d73163c36023b4b4f755bff56254919d14cd49b3f037d85a91258152e8e"
FORMAT = "phase22-statistics-work-v1"


def emit(value):
    print(json.dumps(value, sort_keys=True), flush=True)


def peak_rss():
    value = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return value if platform.system() == "Darwin" else value * 1024


def sidecars(path):
    return {suffix: p.stat().st_size if p.exists() else 0
            for suffix in ("-wal", "-shm", "-journal")
            for p in (Path(str(path) + suffix),)}


def closed_source(path):
    require(Path(path).is_file(), f"missing synthetic fixture: {path}")
    sizes = sidecars(path)
    require(sizes["-wal"] == 0 and sizes["-journal"] == 0,
            "source has a live WAL/rollback journal; close its writer first")
    return sizes


def source_identity(directory):
    retained = directory / "statistics-source.json"
    try:
        commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True,
                                         stderr=subprocess.DEVNULL).strip()
        diff = subprocess.check_output(["git", "diff", "--binary", "HEAD", "--"], cwd=ROOT,
                                       stderr=subprocess.DEVNULL)
    except (FileNotFoundError, subprocess.CalledProcessError):
        require(retained.is_file(), "runtime has no Git; copy retained statistics-source.json with seeds")
        return dict(json.loads(retained.read_text()), source_identity_origin="retained seed metadata; no runtime Git")
    digest = hashlib.sha256(diff)
    names = subprocess.check_output(
        ["git", "ls-files", "--others", "--exclude-standard", "-z"], cwd=ROOT).split(b"\0")
    for name in sorted(filter(None, names)):
        path = ROOT / os.fsdecode(name)
        if path.is_file():
            digest.update(name + b"\0")
            with path.open("rb") as source:
                for chunk in iter(lambda: source.read(1024 * 1024), b""):
                    digest.update(chunk)
    identity = {"git_commit": commit, "dirty_tree_sha256": digest.hexdigest(),
                "source_identity_origin": "runtime Git tracked diff and untracked source content"}
    retained.write_text(json.dumps(identity, indent=2, sort_keys=True) + "\n")
    return identity


def main_head(db):
    return db.execute("SELECT lower(hex(commit_id)) FROM main._lithograph_branches "
                      "WHERE name='main'").fetchone()[0]


def seed(args, count, path, manifest):
    require(not path.exists() and not manifest.exists(), "seed paths must be new; use --reuse")
    started = time.monotonic()
    db = open_db(args.extension, str(path))
    try:
        db.execute("SELECT lithograph_init()").fetchone()
        result = execute(db, f"UNWIND range(0,{count-1}) AS i CREATE (:Common {{v:i}}) FINISH")
        require(result["summary"]["counters"]["nodesCreated"] == count, "seed Node count")
        execute(db, "CREATE CONSTRAINT statistics_v_type FOR (n:Common) REQUIRE n.v IS :: INTEGER")
        execute(db, "CREATE RANGE INDEX statistics_v FOR (n:Common) ON (n.v)")
        head = main_head(db)
        generation, anchor, complete, owners, entries = db.execute(
            "SELECT generation_id,lower(hex(anchor_commit)),complete,indexed_entities,entry_count "
            "FROM main._lithograph_index_generations ORDER BY generation_id DESC LIMIT 1").fetchone()
        require(complete == 1 and owners == entries == count and anchor == head,
                "public CREATE INDEX did not leave the expected ready generation")
        db.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchall()
    finally:
        db.close()
    tool = subprocess.run([str(args.checkpoint_tool), str(path)], check=True,
                          text=True, capture_output=True)
    checkpoint = json.loads(tool.stdout)
    require(checkpoint["commit"] == "commit/" + head, "manual CP moved to another Commit")
    require(checkpoint["node_count"] == count and checkpoint["relationship_count"] == 0,
            "manual CP source cardinalities")
    db = open_db(args.extension, str(path), True)
    try:
        require(main_head(db) == head, "manual CP moved main")
        require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "seed integrity")
    finally:
        db.close()
    closed_source(path)
    fixture = {"format": FORMAT, "nodes": count, "relationships": 0, "label": "Common",
               "property": "v", "value_offset": -1, "index": "statistics_v",
               "generation": generation, "checkpoint": head, "anchor": anchor,
               "database": str(path), "seed_extension_sha256": file_hash(args.extension),
               "seed_seconds": time.monotonic() - started, "checkpoint_build": checkpoint,
               "seed_sha256": file_hash(path)}
    manifest.write_text(json.dumps(fixture, indent=2, sort_keys=True) + "\n")
    emit(dict(fixture, kind="statistics_seed"))
    return fixture


def fixture_for(args, count):
    if count == 1_000_000:
        return {"format": FORMAT, "nodes": count, "relationships": count,
                "label": "ScaleNode", "property": "scaleId", "value_offset": 0,
                "index": "scale_id", "generation": 1, "checkpoint": MILLION_CP,
                "anchor": MILLION_ANCHOR, "database": str(args.million_database)}
    path = args.directory / f"statistics-{count}.sqlite3"
    manifest = args.directory / f"statistics-{count}.fixture.json"
    if not args.reuse:
        return seed(args, count, path, manifest)
    fixture = json.loads(manifest.read_text())
    require(fixture["format"] == FORMAT and fixture["nodes"] == count, "wrong seed manifest")
    fixture["database"] = str(path)
    require(file_hash(path) == fixture["seed_sha256"], "reused seed changed since checkpointing")
    return fixture


def inspect_fixture(extension, fixture):
    """Untimed inventory on a separate connection; no query worker is warmed."""
    path, cp, anchor = fixture["database"], fixture["checkpoint"], fixture["anchor"]
    closed_source(path)
    require(all(len(value) == 64 and all(c in "0123456789abcdef" for c in value)
                for value in (cp, anchor)), "invalid fixture Commit")
    db = open_db(extension, path, True)
    try:
        checkpoint = db.execute("SELECT count(*) FROM main._lithograph_checkpoints "
                                "WHERE commit_id=?", (bytes.fromhex(cp),)).fetchone()[0]
        require(checkpoint == 1, "expected immutable checkpoint missing")
        counts = {name: db.execute(f"SELECT count(*) FROM main.{table} WHERE commit_id=?",
                                  (bytes.fromhex(cp),)).fetchone()[0]
                  for name, table in (("nodes", "_lithograph_cp_nodes"),
                                      ("relationships", "_lithograph_cp_relationships"))}
        require(counts == {key: fixture[key] for key in counts}, "actual CP counts differ")
        generation = db.execute(
            "SELECT lower(hex(anchor_commit)),complete,indexed_entities,entry_count,"
            "CAST(definition_blob AS TEXT) FROM main._lithograph_index_generations "
            "WHERE generation_id=?", (fixture["generation"],)).fetchone()
        require(generation and generation[:4] == (anchor, 1, fixture["nodes"], fixture["nodes"]),
                "wrong ready generation/anchor/counts")
        definition = json.loads(generation[4])["definition"]
        require(definition["kind"] == "range" and definition["name"] == fixture["index"]
                and definition["target"]["label"] == fixture["label"]
                and definition["target"]["properties"] == [fixture["property"]],
                "ready generation is not the specified single Range Index")
        actual_entries = db.execute("SELECT count(*) FROM main._lithograph_index_entries "
                                    "WHERE generation_id=?", (fixture["generation"],)).fetchone()[0]
        require(actual_entries == fixture["nodes"], "actual index entry count differs")
        # The big fixture's generation was created after its CP. Prove that the
        # anchor path adds schema only, so CP counts describe this pinned graph.
        current, layers = anchor, []
        while current != cp:
            require(len(layers) < 128, "unbounded/unrelated CP-to-anchor lineage")
            parent, layer = db.execute("SELECT lower(hex(parent1)),layer_id "
                                       "FROM main._lithograph_commits WHERE id=?",
                                       (bytes.fromhex(current),)).fetchone()
            require(parent and parent != current, "checkpoint is not a first-parent ancestor")
            layers.append(layer)
            current = parent
        overlay = {}
        for table in ("node", "label", "rel", "property"):
            overlay[table] = sum(db.execute(f"SELECT count(*) FROM main._lithograph_{table}_delta "
                                           "WHERE layer_id=?", (layer,)).fetchone()[0] for layer in layers)
        require(not any(overlay.values()), "generation anchor has unaccounted graph Delta")
        settings = {key: db.execute("PRAGMA " + key).fetchone()[0]
                    for key in ("journal_mode", "synchronous", "cache_size", "temp_store", "page_size")}
        return {"actual_checkpoint_counts": counts, "actual_index_entries": actual_entries,
                "definition": definition, "main_head_observed": main_head(db),
                "pinned_snapshot": "commit/" + anchor, "cp_to_anchor_layers": len(layers),
                "cp_to_anchor_graph_delta": overlay, "sqlite_settings": settings,
                "sqlite_settings_scope": "Python inventory/C API rows; external C VM uses unmodified tool defaults",
                "sqlite_compile_options": [row[0] for row in db.execute("PRAGMA compile_options")]}
    finally:
        db.close()


def point_cases(fixture):
    label, prop, offset = fixture["label"], fixture["property"], fixture["value_offset"]
    prefix = f"MATCH (n:{label}) WHERE "
    projection = f" RETURN n.{prop}-$expected,elementId(n),n.{prop}"
    return {"node_id": (prefix + "elementId(n)=$id" + projection,
                        {"id": "n:1", "expected": 1 + offset}, [[0, "n:1", 1 + offset]], "NodeIdSeek"),
            "ready_index": (prefix + f"n.{prop}=$x" + projection,
                            {"x": 7, "expected": 7}, [[0, f"n:{7-offset}", 7]], "IndexSeek")}


def checked_point(db, case, options):
    query, params, expected, _ = case
    started = time.perf_counter_ns()
    result = db.execute(query, params, options, surface="rows")
    elapsed = (time.perf_counter_ns() - started) / 1000
    require(result["rows"] == expected, f"complete point ID/value oracle failed: {result['rows']}")
    return elapsed


def c_vm(args, fixture, case):
    query, params, _, _ = case
    measured = subprocess.run([str(args.vm_tool), str(args.extension), fixture["database"],
                               query, json.dumps(params), json.dumps({"at": "commit/" + fixture["anchor"]})],
                              check=True, text=True, capture_output=True)
    return json.loads(measured.stdout)


def rows_vm(db, case, options):
    """Retain bounded SORT diagnostics, without tracing the latency samples."""
    db.api.sqlite3_sql.argtypes = [ctypes.c_void_p]
    db.api.sqlite3_sql.restype = ctypes.c_char_p
    db.start_work()
    original = db.trace_callback
    sorted_sql, failures = {}, []

    def profile(kind, context, statement, details):
        try:
            sorts = db.api.sqlite3_stmt_status(statement, 2, 0) if kind == 2 else 0
            if sorts:
                sql = db.api.sqlite3_sql(statement).decode()
                require(len(sql) <= 8192 and (sql in sorted_sql or len(sorted_sql) < 32),
                        "SORT diagnostic capacity exceeded")
                entry = sorted_sql.setdefault(sql, {"sorts": 0, "vm_steps": 0, "fullscan_steps": 0})
                entry["sorts"] += sorts
                entry["vm_steps"] += db.api.sqlite3_stmt_status(statement, 4, 0)
                entry["fullscan_steps"] += db.api.sqlite3_stmt_status(statement, 1, 0)
        except BaseException as error:
            failures.append(str(error))
        return original(kind, context, statement, details)

    callback = TRACE(profile)
    db.check(db.api.sqlite3_trace_v2(db.ptr, 2, callback, None))
    try:
        checked_point(db, case, options)
    finally:
        measured = db.stop_work()
    require(not failures, f"invalid SORT diagnostics: {failures}")
    measured["sorted_statements"] = [dict(values, sql_template=sql,
        explain_query_plan=db.sql("EXPLAIN QUERY PLAN " + sql)) for sql, values in sorted_sql.items()]
    return measured


def point_worker(args, fixture, sqlite):
    options = {"at": "commit/" + fixture["anchor"]}
    result = {}
    for name, case in point_cases(fixture).items():
        warm, fresh, first, opens, hits = [], [], [], [], []
        for _ in range(args.connections):
            started = time.perf_counter_ns()
            db = sqlite.connect(args.extension, fixture["database"], True)
            opens.append((time.perf_counter_ns() - started) / 1000)
            try:
                first.append(checked_point(db, case, options))
                warm.extend(checked_point(db, case, options) for _ in range(args.repeats))
            finally:
                db.close()
        for _ in range(args.reopens):
            started = time.perf_counter_ns()
            db = sqlite.connect(args.extension, fixture["database"], True)
            opens.append((time.perf_counter_ns() - started) / 1000)
            try:
                fresh.append(checked_point(db, case, options))
            finally:
                db.close()
        db = sqlite.connect(args.extension, fixture["database"], True)
        try:
            checked_point(db, case, options)
            warm_work = rows_vm(db, case, options)
            profiled = db.execute("PROFILE " + case[0], case[1], options)
            require(profiled["rows"] == case[2], "PROFILE changed complete ID/value oracle")
            hits.append(profiled["summary"]["metrics"]["dbHits"])
            plan = db.execute("EXPLAIN " + case[0], case[1], options)["rows"][0][0]
            require(case[3] in plan, f"required physical path absent: {plan}")
        finally:
            db.close()
        fresh_work = c_vm(args, fixture, case)
        require(fresh_work["sqlite"] == sqlite.version, "C VM/API/Python SQLite runtimes differ")
        db = sqlite.connect(args.extension, fixture["database"], True)
        try:
            fresh_rows_work = rows_vm(db, case, options)
        finally:
            db.close()
        result[name] = {"query": case[0], "parameters": case[1], "complete_oracle": case[2],
                        "warm_rows_adapter": percentiles(warm), "first_on_warm_connections": percentiles(first),
                        "fresh_first_rows_adapter": percentiles(fresh), "open_load_config": percentiles(opens),
                        "warm_c_rows_vm": warm_work, "fresh_c_rows_vm": fresh_rows_work,
                        "fresh_c_scalar_vm": fresh_work,
                        "profile_db_hits": hits, "physical_plan": plan}
    return {"kind": "statistics_points", "cases": result,
            "sample_policy": {"connections": args.connections, "warm_per_connection": args.repeats,
                              "fresh_first_queries_per_case": args.reopens, "smoke": args.smoke},
            "timing_boundary": "parameter/options encode through complete SQL rows decode; open/load separate",
            "cache_state": "same-connection warm and independent new connections; OS cache not purged"}


def complete_stream(db, fixture, memory):
    count = fixture["nodes"]
    seen = bytearray((count + 7) // 8)
    query = f"MATCH (n:{fixture['label']}) RETURN elementId(n),n.{fixture['property']}"
    options = {"at": "commit/" + fixture["anchor"], "memoryLimitBytes": memory}
    started = time.perf_counter_ns()
    first_event = first_row = summary = None
    rows = total_ids = total_values = xor_ids = 0
    for event, data in db.execute("SELECT event,data FROM lithograph_rows(?,?,?)",
                                  (query, "{}", json.dumps(options))):
        require(summary is None, "stream event after terminal summary")
        if first_event is None:
            first_event = time.perf_counter_ns() - started
            require(event == "columns", "stream did not begin with columns")
        else:
            require(event != "columns", "duplicate columns event")
        if event == "row":
            if first_row is None:
                first_row = time.perf_counter_ns() - started
            identity, value = json.loads(data)
            require(isinstance(identity, str) and identity.startswith("n:"), "non-Node identity")
            node = int(identity[2:])
            require(identity == f"n:{node}" and 1 <= node <= count, "Node outside complete oracle")
            require(type(value) is int and value == node + fixture["value_offset"], "wrong ID/value pair")
            slot, mask = (node - 1) // 8, 1 << ((node - 1) % 8)
            require(not seen[slot] & mask, "duplicate stream identity")
            seen[slot] |= mask
            rows += 1
            total_ids += node
            total_values += value
            xor_ids ^= node
        elif event == "summary":
            require(summary is None, "duplicate terminal summary")
            summary = json.loads(data)
        else:
            require(event == "columns", f"unexpected stream event: {event}")
    require(summary is not None and rows == count, "long stream was incomplete")
    require(total_ids == count * (count + 1) // 2, "complete identity bag sum")
    require(xor_ids == (count, 1, count + 1, 0)[count % 4], "complete identity bag XOR")
    require(total_values == total_ids + count * fixture["value_offset"], "complete property bag sum")
    return {"rows": rows, "unique_ids": rows, "sum_ids": total_ids, "xor_ids": xor_ids,
            "sum_values": total_values, "oracle_bitset_bytes": len(seen),
            "first_event_us": first_event / 1000, "first_row_us": first_row / 1000,
            "full_us": (time.perf_counter_ns() - started) / 1000,
            "metrics": summary["metrics"]}


def long_worker(args, fixture):
    db = open_db(args.extension, fixture["database"], True)
    options = {"at": "commit/" + fixture["anchor"]}
    recovery_case = point_cases(fixture)["node_id"]

    def recover():
        require(execute(db, recovery_case[0], recovery_case[1], options)["rows"] == recovery_case[2],
                "same-connection point read failed after cleanup")
        require(not db.in_transaction, "read failure left a transaction active")

    try:
        drains = [complete_stream(db, fixture, args.memory_limit_bytes) for _ in range(3)]
        query = f"MATCH (n:{fixture['label']}) RETURN elementId(n),n.{fixture['property']}"
        calls = 0

        def interrupt():
            nonlocal calls
            calls += 1
            if calls == 8:
                db.interrupt()
            return 0

        cancelled = False
        cursor = None
        started = time.perf_counter_ns()
        rows_before_interrupt = 0
        try:
            cursor = db.execute("SELECT event,data FROM lithograph_rows(?,?,?)",
                                (query, "{}", json.dumps(dict(options, memoryLimitBytes=args.memory_limit_bytes))))
            require(cursor.fetchone()[0] == "columns", "interrupt cursor columns")
            require(cursor.fetchone()[0] == "row", "interrupt cursor first real row")
            rows_before_interrupt = 1
            db.set_progress_handler(interrupt, 64)
            for _ in cursor:
                pass
        except sqlite3.Error as error:
            require(error.sqlite_errorcode == sqlite3.SQLITE_INTERRUPT, str(error))
            cancelled = True
        finally:
            db.set_progress_handler(None, 0)
            if cursor is not None:
                cursor.close()
        cancel_us = (time.perf_counter_ns() - started) / 1000
        require(cancelled and calls >= 8 and rows_before_interrupt == 1,
                "active long cursor did not reach real sqlite3_interrupt")
        recover()
        try:
            execute(db, recovery_case[0], recovery_case[1], dict(options, memoryLimitBytes=64))
        except sqlite3.Error as error:
            require("LITHOGRAPH_RESOURCE_ERROR" in str(error), str(error))
        else:
            raise AssertionError("64-byte resource fixture unexpectedly succeeded")
        recover()
        cursor = db.execute("SELECT event,data FROM lithograph_rows(?,?,?)", (query, "{}", json.dumps(options)))
        require(cursor.fetchone()[0] == "columns" and cursor.fetchone()[0] == "row", "early-close first row")
        cursor.close()
        recover()
        times = [entry["full_us"] for entry in drains]
        return {"kind": "statistics_long", "adapter": "Python sqlite3/lithograph_rows",
                "drains": drains, "median_full_us": statistics.median(times), "max_full_us": max(times),
                "interrupt_us": cancel_us, "interrupt_progress_calls": calls,
                "rows_before_interrupt_enabled": rows_before_interrupt,
                "resource_error_and_same_connection_recovery": True, "early_close_and_recovery": True,
                "resource_error_stage": "entry quota of 64 bytes",
                "memory_limit_bytes": args.memory_limit_bytes,
                "oracle": "order-independent unique ID bitset plus every ID/value pair; no retained row list"}
    finally:
        db.close()


def worker(args):
    fixture = args.worker_fixture
    sqlite = select_sqlite(args.sqlite_library)
    before, rss_before, group_before = file_hash(fixture["database"]), peak_rss(), cgroup()
    result = point_worker(args, fixture, sqlite) if args.worker == "point" else long_worker(args, fixture)
    require(file_hash(fixture["database"]) == before, "read worker changed main SHA256")
    closed_source(fixture["database"])
    group_after = cgroup()
    for field in ("oom", "oom_kill"):
        def events(group):
            return dict(line.split() for line in group.get("memory.events", "").splitlines())
        require(events(group_before).get(field, "0") == events(group_after).get(field, "0"),
                f"cgroup {field} increased")
    result.update({"fixture": fixture, "extension_sha256": file_hash(args.extension),
                   "main_sha256_before": before, "main_sha256_after": before,
                   "main_bytes": Path(fixture["database"]).stat().st_size,
                   "sidecars_after": sidecars(fixture["database"]), "sqlite": sqlite.version,
                   "sqlite_library": sqlite.path, "platform": platform.platform(),
                   "process_peak_rss_bytes": peak_rss(), "peak_rss_growth_bytes": max(0, peak_rss() - rss_before),
                   "cgroup_before": group_before, "cgroup_after": group_after})
    emit(result)


def run_worker(args, fixture, mode, extension=None):
    command = [sys.executable, "-B", str(Path(__file__).resolve()), str(extension or args.extension),
               str(args.directory), "--worker", mode, "--worker-fixture", json.dumps(fixture),
               "--vm-tool", str(args.vm_tool), "--memory-limit-bytes", str(args.memory_limit_bytes)]
    if args.sqlite_library:
        command += ["--sqlite-library", args.sqlite_library]
    if args.smoke:
        command += ["--smoke"]
    completed = subprocess.run(command, text=True, capture_output=True, timeout=1800)
    require(completed.returncode == 0, "isolated worker failed:\n" + completed.stdout + completed.stderr)
    result = json.loads(completed.stdout)
    emit(result)
    return result


def growth_gate(results):
    for lower, upper in zip(results, results[1:]):
        if upper["fixture"]["nodes"] != lower["fixture"]["nodes"] * 10:
            continue
        for case in ("node_id", "ready_index"):
            for adapter in ("warm_c_rows_vm", "fresh_c_rows_vm", "fresh_c_scalar_vm"):
                old = lower["cases"][case][adapter]["vm_steps"]
                new = upper["cases"][case][adapter]["vm_steps"]
                require(new <= 2 * old + 1000, f"10x unrelated-data VM regression: {case}/{adapter}: {old}->{new}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extension", type=lambda p: Path(p).resolve())
    parser.add_argument("directory", type=lambda p: Path(p).resolve())
    parser.add_argument("--checkpoint-tool", type=lambda p: Path(p).resolve(),
                        default=ROOT / "target/release/lithograph-phase21-checkpoint")
    parser.add_argument("--vm-tool", type=lambda p: Path(p).resolve(), default=ROOT / "target/phase21/vm-work")
    parser.add_argument("--million-database", type=lambda p: Path(p).resolve(),
                        default=ROOT / "target/phase22-access-scale-1m/scale.sqlite")
    parser.add_argument("--sizes", nargs="+", type=int, choices=[1000, 10000, 100000, 1000000],
                        default=[1000, 10000, 100000, 1000000])
    parser.add_argument("--reuse", action="store_true", help="read retained seeds; never recreate/modify them")
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--skip-long", action="store_true", help="point/VM samples only")
    group.add_argument("--long-only", action="store_true", help="3 drains, interrupt, RESOURCE and cleanup only")
    group.add_argument("--seed-only", action="store_true", help="construct/retain seed and CP; do not benchmark")
    parser.add_argument("--before-extension", type=lambda p: Path(p).resolve())
    parser.add_argument("--before-max-nodes", type=int, default=1000)
    parser.add_argument("--memory-limit-bytes", type=int, default=8 * 1024 * 1024)
    parser.add_argument("--sqlite-library")
    parser.add_argument("--smoke", action="store_true", help="1x1 warm/1 fresh; NOT an acceptance sample")
    parser.add_argument("--worker", choices=["point", "long"], help=argparse.SUPPRESS)
    parser.add_argument("--worker-fixture", type=json.loads, help=argparse.SUPPRESS)
    args = parser.parse_args()
    args.connections, args.repeats, args.reopens = (1, 1, 1) if args.smoke else (5, 20, 100)
    require(args.memory_limit_bytes > 64 and args.before_max_nodes >= 0, "invalid resource/baseline limits")
    if args.worker:
        worker(args)
        return
    require(args.extension.is_file() and args.vm_tool.is_file(), "extension/VM tool missing")
    require(args.checkpoint_tool.is_file() or args.reuse, "checkpoint tool missing")
    args.directory.mkdir(parents=True, exist_ok=True)
    emit(dict(source_identity(args.directory), kind="statistics_environment", format=FORMAT,
              platform=platform.platform(), cpu=platform.processor(), cpu_count=os.cpu_count(),
              sqlite_python=sqlite3.sqlite_version, extension_sha256=file_hash(args.extension),
              vm_tool_sha256=file_hash(args.vm_tool), cgroup=cgroup(), fixture_seed="deterministic-range-v1",
              fixture_formula="Common Node i+1 has v=i; reused ScaleNode i has scaleId=i",
              smoke=args.smoke, build_profile="caller-supplied artifact; no compilation in harness"))
    points, long_groups = [], 0
    for count in sorted(set(args.sizes)):
        fixture = fixture_for(args, count)
        before = file_hash(fixture["database"])
        emit(dict(inspect_fixture(args.extension, fixture), kind="statistics_inventory", fixture=fixture,
                  main_sha256=before, inventory_is_untimed_separate_connection=True))
        if args.seed_only:
            continue
        if not args.long_only:
            points.append(run_worker(args, fixture, "point"))
            if args.before_extension and count <= args.before_max_nodes:
                run_worker(args, fixture, "point", args.before_extension)
        if not args.skip_long:
            run_worker(args, fixture, "long")
            long_groups += 1
        require(file_hash(fixture["database"]) == before, "workload changed immutable source")
    growth_gate(points)
    emit({"kind": "statistics_complete", "sizes": sorted(set(args.sizes)),
          "point_groups": len(points), "growth_gate": "2x prior VM + 1000 per 10x unrelated data",
          "long_groups": long_groups, "short_query_acceptance_samples": bool(points) and not args.smoke,
          "cgroup": cgroup()})


if __name__ == "__main__":
    main()
