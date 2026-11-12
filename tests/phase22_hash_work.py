#!/usr/bin/env python3
"""S22.5 public SQL semantics and fixed-fixture Hash Join work.

The small public SQL gate has no ignored fixture dependency. Short measurements
reuse the exact retained stock-SQLite statements and the S4 sampling contract.
Long rows measurements use phase22_hash_stream.c in independent processes and
validate every actual ID/key row without collecting the complete result set.
No command builds an index or mutates an input fixture.
"""

import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import time

from phase22_access_work import cgroup
from phase22_join_work import (
    check_rows, closed, measured, new_path,
    recovery, sidecars,
)
from phase22_relationship_work import file_hash, require, select_sqlite, state


HASH_FIELDS = (
    "buildRows", "probeRows", "hashCollisions", "spillPartitions", "spillBatches",
    "spillVmSteps", "spillEncodedBytes", "memoryPeakBytes", "tempAllocatedBytes",
)


def save(path, value):
    with Path(path).open("x") as output:
        json.dump(value, output, indent=2, sort_keys=True)
        output.write("\n")


def hash_profile(summary, required=False, spilled=False):
    profile = summary["profile"]
    operators = profile["operators"]
    require(sum(op["dbHits"] for op in operators) == summary["metrics"]["dbHits"],
            "PROFILE operator dbHits sum differs from summary")
    joins = [op for op in operators if op["operator"] == "HashJoin"]
    if required:
        require(joins and any(op["rows"] > 0 for op in joins), "no actual HashJoin output")
    for op in joins:
        counters = op.get("hash")
        if counters is None and op["rows"] == 0:
            # A static slot whose input was empty need not instantiate Hash.
            continue
        require(isinstance(counters, dict), "actual HashJoin has no typed hash PROFILE counters")
        require(all(type(counters.get(field)) is int and counters[field] >= 0
                    for field in HASH_FIELDS), "incomplete/negative HashJoin PROFILE counters")
    if spilled:
        require(any(op.get("hash") and op["hash"]["spillPartitions"] > 0 and op["hash"]["spillBatches"] > 0
                    and op["hash"]["spillEncodedBytes"] > 0 for op in joins),
                "Grace success requires actual partitioned spill, not an error/fallback")
    return joins


def canonical(query):
    prefix, tail = query.split(" WHERE ", 1)
    predicate, projection = tail.split(" RETURN ", 1)
    return prefix + " WHERE (" + predicate + ") OR false RETURN " + projection


def semantic_execute(db, query, options, surface):
    if surface == "scalar":
        return db.execute(query, options=options)
    events = db.rows_stmt.execute((query, "{}", json.dumps(options)))
    require(len(events) >= 2 and events[0][0] == "columns" and events[-1][0] == "summary",
            "rows protocol did not completely consume columns through summary")
    require(all(event == "row" for event, _ in events[1:-1]), "unexpected/duplicate stream event")
    return {"columns": json.loads(events[0][1]), "rows": [json.loads(data) for _, data in events[1:-1]],
            "summary": json.loads(events[-1][1])}


def semantic_case(db, case, before):
    query, expected = case["query"], case["oracle"]
    options = case.get("options", {})
    columns = case.get("columns", ["0", "elementId(a)", "elementId(b)", "a.k", "b.k"])
    results = {}
    for surface in ("scalar", "rows"):
        envelope = semantic_execute(db, query, options, surface)
        require(envelope["columns"] == columns, "observable result columns differ")
        check_rows(envelope["rows"], expected)
        profiled = semantic_execute(db, "PROFILE " + query, options, surface)
        require(profiled["columns"] == columns, "PROFILE result columns differ")
        check_rows(profiled["rows"], expected)
        hash_profile(profiled["summary"], case.get("hash", False) and not before)
        original = semantic_execute(db, canonical(query), options, surface)
        require(original["columns"] == columns, "canonical result columns differ")
        check_rows(original["rows"], expected)
        results[surface] = {"rows": envelope["rows"], "profile": profiled["summary"]}
    plan = db.execute("EXPLAIN " + query, options=options)["rows"][0][0]
    if case.get("hash") and not before:
        require("HashJoin" in plan, "positive HashJoin absent from EXPLAIN")
    return {"name": case["name"], "query": query, "columns": columns, "oracle": expected,
            "options": options, "plan": plan, "surfaces": results}


def semantics(args, sqlite):
    """A complete public CREATE/Constraint/SQL gate, independent of target/ seeds."""
    with tempfile.TemporaryDirectory(prefix="lithograph-s5-sql-") as directory:
        path = Path(directory) / "semantic.sqlite3"
        db = sqlite.connect(args.extension, path)
        try:
            db.sql("SELECT lithograph_init()")
            version, source_id = db.sql("SELECT sqlite_version(),sqlite_source_id()")[0]
            environment = {"version": version, "source_id": source_id,
                           "compile_options": [row[0] for row in db.sql("PRAGMA compile_options")]}
            left_keys = [1, 1, 2, 2, 3, 3, 4, 4, None, None] + list(range(1000, 1246))
            right_keys = [key for key in range(1, 5) for _ in range(3)] + [None] + list(range(2000, 2243))
            require(len(left_keys) == len(right_keys) == 256, "fixed semantic dimensions")
            left = db.execute("UNWIND $keys AS k CREATE(a:S5Build:Visible{k:k}) "
                              "RETURN elementId(a),a.k,a", {"keys": left_keys})["rows"]
            right = db.execute("UNWIND $keys AS k CREATE(b:S5Probe:Visible{k:k}) "
                               "RETURN elementId(b),b.k,b", {"keys": right_keys})["rows"]
            for label in ("S5Build", "S5Probe"):
                db.execute(f"CREATE CONSTRAINT {label}_integer FOR(n:{label}) REQUIRE n.k IS :: INTEGER")
            require(db.execute("SHOW INDEXES YIELD name RETURN name")["rows"] == [],
                    "no Property Index is permitted in the Hash fixture")
            expected = [[0, aid, bid, key, value] for aid, key, _ in left for bid, value, _ in right
                        if key is not None and key == value]
            require(len(expected) == 24, "fixed complete many-to-many bag")
            query = "MATCH(a:S5Build),(b:S5Probe) WHERE a.k=b.k RETURN 0,elementId(a),elementId(b),a.k,b.k"
            graph_query = query.replace("RETURN 0,elementId(a),elementId(b),a.k,b.k",
                                        "RETURN 0,elementId(a),elementId(b),a.k,b.k,[a,b] AS nodes")
            graphs = {identity: graph for identity, _, graph in left + right}
            graph_rows = []
            for row in expected:
                masked = [dict(graphs[row[1]], properties={}), dict(graphs[row[2]], properties={})]
                graph_rows.append(row + [masked])
            at = "commit/" + state(db)[0]
            cases = [
                {"name": "node_duplicate_null_bag", "query": query, "oracle": expected, "hash": True},
                {"name": "symmetric_equality", "query": query.replace("a.k=b.k", "b.k=a.k"),
                 "oracle": expected, "hash": True},
                {"name": "written_part_swap", "query": query.replace("(a:S5Build),(b:S5Probe)",
                 "(b:S5Probe),(a:S5Build)"), "oracle": expected, "hash": True},
                {"name": "nullable_key_does_not_join", "query": query.replace("WHERE ",
                 "WHERE a.k IS NULL AND "), "oracle": []},
                {"name": "visible_view", "query": query, "oracle": expected,
                 "options": {"graphView": {"requireAllLabels": ["Visible"]}}},
                {"name": "excluded_view", "query": query, "oracle": [],
                 "options": {"graphView": {"excludeAnyLabels": ["Visible"]}}},
                {"name": "query_property_mask", "query": query, "oracle": [],
                 "options": {"readAccess": {"query": {"denyProperties": ["k"]}}}},
                {"name": "recursive_result_mask_keeps_scalar", "query": graph_query,
                 "oracle": graph_rows,
                 "columns": ["0", "elementId(a)", "elementId(b)", "a.k", "b.k", "nodes"],
                 "options": {"readAccess": {"result": {"denyProperties": ["k"]}}}},
                {"name": "zero_outer_skips_error", "query": "MATCH(a:S5Never),(b:S5Probe) "
                 "WHERE a.k=b.k RETURN 1/0 AS failing", "oracle": [], "columns": ["failing"]},
            ]
            baseline_state = state(db)
        finally:
            db.close()
        original_hash, original_sidecars = file_hash(path), closed(path)
        db = sqlite.connect(args.extension, path, True)
        try:
            original_handles = (db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value)
            samples = []
            for case in cases:
                case["options"] = {"at": at, **case.get("options", {})}
                samples.append(semantic_case(db, case, args.before))
            # Prove errors remain observable after a real nonempty join.
            for surface in ("scalar", "rows"):
                try:
                    db.execute(query.replace("RETURN 0,elementId(a),elementId(b),a.k,b.k", "RETURN 1/0"),
                               options={"at": at}, surface=surface)
                except RuntimeError as error:
                    require("LITHOGRAPH_TYPE_ERROR" in str(error), f"wrong error: {error}")
                else:
                    raise AssertionError("nonempty Hash projection error was swallowed")
                check_rows(db.execute(query, options={"at": at}, surface=surface)["rows"], expected)
            recovered = recovery(sqlite, args, path, {"at": at},
                                 {"query": query, "oracle": expected, "operator": None})
            require(state(db) == baseline_state, "semantic read changed head/history")
            require(original_handles == (db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value),
                    "semantic gate replaced retained scalar/rows handles")
        finally:
            db.close()
        require(file_hash(path) == original_hash and sidecars(path) == original_sidecars,
                "semantic read changed fixture or sidecars")
        return {"status": "passed", "kind": "phase22-hash-public-semantics", "before": args.before,
                "sqlite": environment, "sqlite_library": sqlite.path,
                "extension_sha256": file_hash(args.extension), "node_counts": [256, 256],
                "property_indexes": 0, "cases": samples,
                "nonempty_projection_error_and_same_handle_recovery": True, "recovery": recovered,
                "readonly_fixture_hash_head_sidecars_unchanged": True}


def load_manifest(path):
    manifest = json.loads(path.read_text())
    require(manifest.get("kind") == "phase22-hash-fixture-v1", "wrong Hash fixture format")
    bilateral = manifest["distribution"] == "bilateral-skew"
    require((manifest["node_count"] == 200002 and manifest["hot_rows"] == 100001) if bilateral
            else manifest["node_count"] in (1000, 10000, 100000, 1000000), "unapproved scale/distribution")
    database = Path(manifest["path"]).resolve()
    require(manifest["commit"] == manifest["checkpoint_commit"], "fixture CP must be exact head")
    digest = manifest.get("fixture_sha256", "")
    require(len(digest) == 64 and all(char in "0123456789abcdef" for char in digest),
            "SQL pressure requires the frozen SHA256 published by this harness seed command")
    for case in manifest["oracles"]:
        recipe = case["recipe"]
        kind = "bilateral-hot-equijoin-v1" if bilateral else "offset-equijoin-v1"
        require(recipe["kind"] == kind and recipe["first_node"] == manifest["first_node"],
                "unsupported/mismatched complete-row oracle")
        expected = 24 if case["name"] == "small24" else 400002 if bilateral else manifest["node_count"]
        require(case["expected_rows"] == expected, "manifest whole-row cardinality differs from recipe")
    return manifest, database


def check_frozen_fixture(args, sqlite, manifest, database):
    sides = closed(database)
    digest = file_hash(database)
    require(database.stat().st_size == manifest["database_bytes"] and digest == manifest["fixture_sha256"],
            "actual fixture bytes/digest differ from the frozen manifest")
    db = sqlite.connect(args.extension, database, True)
    try:
        current = state(db)
        require(current[0] == manifest["commit"].removeprefix("commit/"),
                "actual main head differs from the frozen fixture Commit")
        checkpoint = db.sql("SELECT json_extract(CAST(metadata AS TEXT),'$.nodes'),"
                            "json_extract(CAST(metadata AS TEXT),'$.relationships') "
                            "FROM _lithograph_checkpoints WHERE lower(hex(commit_id))='"
                            +current[0]+"'")
        require(checkpoint == [[manifest["node_count"], 0]], "actual exact-head CP dimensions differ")
        require(db.execute("SHOW INDEXES YIELD name RETURN name", options={"at": manifest["commit"]})["rows"] == [],
                "Hash pressure input acquired an undeclared Property Index")
    finally:
        db.close()
    require(file_hash(database) == digest and sidecars(database) == sides,
            "fixture guard changed main/sidecars; normalize a legitimate first-reader SHM transition during seed")
    return digest, sides, current


def seed_fixture(args, sqlite):
    """Publish the generator's immutable fixture with a stdlib SHA256 pin."""
    require(args.nodes == 200002 if args.distribution == "bilateral-skew"
            else args.nodes in (1000, 10000, 100000, 1000000), "unapproved fixture scale/distribution")
    harness_sha = file_hash(Path(__file__))
    for path in (args.database, args.manifest):
        new_path(path)
        path.parent.mkdir(parents=True, exist_ok=True)
    helper_sha = file_hash(args.fixture_helper)
    prefix = args.manifest.with_name(args.manifest.name+".generator")
    argv = [str(args.fixture_helper.resolve()), str(args.database.resolve()), str(args.nodes),
            "--distribution", args.distribution]
    record = child_process(argv, prefix)
    manifest = json.loads(prefix.with_suffix(".stdout").read_text())
    require(manifest["kind"] == "phase22-hash-fixture-v1" and manifest["node_count"] == args.nodes
            and manifest["distribution"] == args.distribution
            and manifest["commit"] == manifest["checkpoint_commit"], "generator fixture contract differs")
    require(Path(manifest["path"]).resolve() == args.database.resolve(), "generator returned another path")
    # A readonly metadata reopen may establish normal SQLite SHM. Capture that
    # transition before publication, never silently rebase a measured fixture.
    before_sides = closed(args.database)
    db = sqlite.connect(args.extension, args.database, True)
    try:
        head = state(db)
    finally:
        db.close()
    manifest["fixture_sha256"] = file_hash(args.database)
    manifest["published_sidecars"] = closed(args.database)
    manifest["seed_readonly_probe_sidecar_transition"] = {"before": before_sides, "after": manifest["published_sidecars"]}
    require(file_hash(Path(__file__)) == harness_sha, "seed harness changed during setup; preserve DB/logs without misattributed publication")
    manifest["seed_publication"] = {"generator": str(args.fixture_helper.resolve()), "generator_sha256": helper_sha,
        "process": record, "extension_sha256": file_hash(args.extension), "harness_sha256": harness_sha,
        "hash_source": "stdlib hashlib.sha256, immediately after fresh generator success and readonly metadata reopen"}
    require(head[0] == manifest["commit"].removeprefix("commit/")
            and file_hash(args.fixture_helper) == helper_sha, "seed helper/head changed")
    check_frozen_fixture(args, sqlite, manifest, args.database.resolve())
    save(args.manifest, manifest)
    print(json.dumps({"status": "passed", "manifest": str(args.manifest.resolve()),
                      "fixture_sha256": manifest["fixture_sha256"]}), flush=True)


def small_case(manifest):
    source = next(case for case in manifest["oracles"] if case["name"] == "small24")
    require(source["expected_rows"] == len(source["rows"]) == 24, "small complete oracle")
    return {"name": "small24", "query": source["query"], "oracle": source["rows"], "operator": None}


def timing_summary(samples):
    result = {}
    for key in ("first_data_us", "first_data_cpu_us", "full_us", "full_cpu_us"):
        values = sorted(sample[key] for sample in samples if sample[key] is not None)
        result[key] = {"n": len(values), "p50": values[(len(values)-1)//2] if values else None,
                       "p95": values[(len(values)*95+99)//100-1] if len(values) >= 100 else None,
                       "max": max(values) if values else None}
    return result


def short_sampling(args, sqlite, path, options, case, raw_path):
    """Keep completed raw samples even if a later invocation fails."""
    with raw_path.open("x") as raw:
        def record(db, surface, role, connection=0, iteration=0, traced=False, profile=False):
            started = time.time_ns()
            try:
                sample = measured(db, case, options, surface, traced=traced, profile=profile)
            except BaseException as error:
                raw.write(json.dumps({"surface": surface, "role": role, "connection": connection,
                    "iteration": iteration, "unix_start_ns": started, "error": repr(error)})+"\n")
                raw.flush()
                raise
            raw.write(json.dumps({"surface": surface, "role": role, "connection": connection,
                "iteration": iteration, "unix_start_ns": started, "sample": sample})+"\n")
            raw.flush()
            return sample

        result = {"sampling": "untraced 5x20 warm+100fresh; warm-first and VM/PROFILE separate", "surfaces": {}}
        for surface in ("scalar", "rows"):
            first, warm, fresh = [], [], []
            for connection in range(5):
                db = sqlite.connect(args.extension, path, True)
                try:
                    handles = (db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value)
                    first.append(record(db, surface, "warm_first", connection))
                    warm.append([record(db, surface, "warm", connection, iteration) for iteration in range(20)])
                    require(handles == (db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value), "retained handles changed")
                finally:
                    db.close()
            for connection in range(100):
                db = sqlite.connect(args.extension, path, True)
                try:
                    fresh.append(record(db, surface, "fresh", connection))
                finally:
                    db.close()
            db = sqlite.connect(args.extension, path, True)
            try:
                fresh_vm = record(db, surface, "fresh_vm", traced=True)
                warm_vm = record(db, surface, "warm_vm", traced=True)
                profiled = record(db, surface, "profile", profile=True)
                plan = db.execute("EXPLAIN "+case["query"], options=options)["rows"][0][0]
            finally:
                db.close()
            result["surfaces"][surface] = {"connection_first": first, "warm_raw": warm, "fresh_raw": fresh,
                "fresh_vm": fresh_vm, "warm_vm": warm_vm, "profile_sample": profiled, "plan": plan,
                "first_summary": timing_summary(first), "warm_summary": timing_summary([item for group in warm for item in group]),
                "fresh_summary": timing_summary(fresh), "full_executions": 208}
        return result


def short_fixture(args, sqlite, manifest_path):
    manifest, path = load_manifest(manifest_path)
    before_hash, before_sides, initial_state = check_frozen_fixture(args, sqlite, manifest, path)
    case = small_case(manifest)
    if args.before:
        require(manifest["node_count"] == 1000, "Before is restricted to the safe 1k fixture")
    options = {"at": manifest["commit"]}
    if args.memory_limit_bytes:
        options["memoryLimitBytes"] = args.memory_limit_bytes
    if args.command == "smoke":
        db = sqlite.connect(args.extension, path, True)
        try:
            sample = {surface: measured(db, case, options, surface, traced=True)
                      for surface in ("scalar", "rows")}
            sample["profile_sample"] = measured(db, case, options, "rows", profile=True)
            sample["plan"] = db.execute("EXPLAIN " + case["query"], options=options)["rows"][0][0]
        finally:
            db.close()
    else:
        sample = short_sampling(args, sqlite, path, options, case,
                                args.output/f"{manifest['distribution']}-{manifest['node_count']}.short.raw.jsonl")
    profile_samples = ([sample["profile_sample"]] if args.command == "smoke"
                       else [surface["profile_sample"] for surface in sample["surfaces"].values()])
    for item in profile_samples:
        hash_profile({"profile": item["profile"], "metrics": item["metrics"]})
    recovered = recovery(sqlite, args, path, options, case)
    db = sqlite.connect(args.extension, path, True)
    try:
        require(state(db) == initial_state, "short read changed fixture state")
    finally:
        db.close()
    require(file_hash(path) == before_hash and sidecars(path) == before_sides,
            "short read changed immutable fixture")
    return {"manifest": str(manifest_path.resolve()), "scale": manifest["node_count"],
            "distribution": manifest["distribution"], "case": case, "sample": sample,
            "recovery": recovered, "fixture_sha256": before_hash, "sidecars": before_sides,
            "actual_small_producer_policy": "cost may choose NestedLoop or HashJoin; full large query separately requires HashJoin"}


def short_growth(results):
    groups, checks = {}, []
    for fixture in results:
        for surface, sample in fixture["sample"]["surfaces"].items():
            for role in ("fresh_vm", "warm_vm"):
                key = (fixture["distribution"], surface, role)
                groups.setdefault(key, []).append((fixture["scale"], sample[role]["sqlite_work"]["vm_steps"]))
    for key, values in groups.items():
        values.sort()
        for (small, low), (large, high) in zip(values, values[1:]):
            if large == 10*small:
                require(high <= 2*low+1000, f"fixed 24-row unrelated-data VM growth: {key}: {values}")
                checks.append({"group": key, "small": small, "large": large,
                               "small_vm": low, "large_vm": high, "limit": 2*low+1000})
    return checks


def child_process(argv, prefix):
    """Retain every attempt and use wait4 for this child, never cumulative RSS."""
    started = time.perf_counter_ns()
    with prefix.with_suffix(".stdout").open("x") as output, prefix.with_suffix(".stderr").open("x") as errors:
        child = subprocess.Popen(argv, stdout=output, stderr=errors)
        _, status, usage = os.wait4(child.pid, 0)
        child.returncode = os.waitstatus_to_exitcode(status)
    record = {"argv": argv, "returncode": child.returncode,
              "process_wall_us": (time.perf_counter_ns()-started)/1000,
              "process_cpu_us": (usage.ru_utime+usage.ru_stime)*1e6,
              "peak_rss_bytes": usage.ru_maxrss if platform.system() == "Darwin" else usage.ru_maxrss*1024,
              "unix_end_ns": time.time_ns()}
    save(prefix.with_suffix(".process.json"), record)
    require(child.returncode == 0, f"child failed; original stdout/stderr/process retained: {prefix}")
    return record


def actual_sql_runtime(sqlite, extension):
    db = sqlite.connect(extension)
    try:
        version, source = db.sql("SELECT sqlite_version(),sqlite_source_id()")[0]
        return {"version": version, "source_id": source,
                "compile_options": [row[0] for row in db.sql("PRAGMA compile_options")]}
    finally:
        db.close()


def long_fixture(args, sqlite, manifest_path, runtime):
    manifest, database = load_manifest(manifest_path)
    if args.before:
        require(manifest["node_count"] == 1000, "large Before Cartesian is not authorized")
    require(not args.require_spill or not args.before, "Before cannot prove the new Grace implementation")
    require(not args.require_spill or args.case == "full", "Grace success uses the complete full fixture query")
    folder = args.output / f"{manifest['distribution']}-{manifest['node_count']}"
    new_path(folder)
    folder.mkdir()
    before_hash, before_sidecars, initial_state = check_frozen_fixture(args, sqlite, manifest, database)
    expected_source = runtime["source_id"] if args.surface == "rows" else args.expected_core_source_id
    require(expected_source, "Core pressure requires its own fixed --expected-core-source-id")
    helper_sha = file_hash(args.helper)
    save(folder / "context.json", {"manifest": str(manifest_path.resolve()), "manifest_sha256": file_hash(manifest_path),
        "fixture_sha256": before_hash, "sidecars": before_sidecars, "state": initial_state,
        "surface": args.surface, "expected_sqlite_source_id": expected_source,
        "helper": str(args.helper.resolve()), "helper_sha256": helper_sha,
        "memory_limit_bytes": args.memory_limit_bytes, "SQLITE_TMPDIR": os.environ.get("SQLITE_TMPDIR"),
        "TMPDIR": os.environ.get("TMPDIR"), "raw_order": "three chronological independent processes, no P95",
        "cache_class": "fixture SHA256 read before and after; OS cache pre-read, not OS cold"})
    primary, diagnostics = [], []
    jobs = [(f"primary-{number}", False) for number in range(1, 4)] + [("profile-vm", True)]
    try:
        for name, profile in jobs:
            prefix = folder / name
            if args.surface == "rows":
                argv = [str(args.helper.resolve()), str(args.extension.resolve()), str(database), manifest["commit"],
                        str(manifest["node_count"]), str(manifest["first_node"]), str(manifest["hot_rows"]),
                        args.case, str(args.memory_limit_bytes or 0), "--sqlite-source-id", expected_source]
                if args.before:
                    argv.append("--before")
                if manifest["distribution"] == "bilateral-skew":
                    argv.append("--bilateral-hot")
                if profile:
                    argv += ["--profile", "--trace"]
                    if args.require_spill:
                        argv.append("--require-spill")
            else:
                argv = [str(args.helper.resolve()), "--manifest", str(manifest_path.resolve()),
                        "--output", str(prefix.with_suffix(".result.json").resolve()), "--case", args.case,
                        "--mode", "probe" if profile else "single", "--sqlite-source-id", expected_source]
                if args.before:
                    argv.append("--before")
                if args.memory_limit_bytes:
                    argv += ["--memory-limit-bytes", str(args.memory_limit_bytes)]
                if args.provenance:
                    argv += ["--provenance", str(args.provenance.resolve())]
            process = child_process(argv, prefix)
            if args.surface == "rows":
                payload = json.loads(prefix.with_suffix(".stdout").read_text())
                require(payload["status"] == "passed" and payload["complete_typed_bag_verified"], "incomplete C rows oracle")
                require(payload["sqlite"]["sourceId"] == expected_source
                        and payload["sqlite_source_id_checked_before_extension_load"], "wrong SQL linked runtime")
                if profile:
                    joins = hash_profile(payload["summary"], not args.before and args.case == "full", args.require_spill)
                sample = {"full_us": payload["full_wall_us"], "full_cpu_us": payload["full_cpu_us"],
                          "first_us": payload["first_data_wall_us"], "checksum": payload["fnv1a64_hex"],
                          "actual_rows": payload["rows"]}
            else:
                payload = json.loads(prefix.with_suffix(".result.json").read_text())
                require(payload["status"] == "passed" and payload["fixture_unchanged"], "Core work failed")
                samples = payload["measurement"]["samples"]
                require(len(samples) == 1, "independent primary/probe must execute exactly one full drain")
                item = samples[0]["sample"]
                require(item["sqlite"]["source_id"] == expected_source
                        and item["sqlite"]["sqlite_source_id_guard"], "wrong Core linked runtime")
                require(item["resource_used_after_close"] == 0 and item["counter_probe"] == profile,
                        "Core resource/counter sample boundary differs")
                require(item["oracle"]["complete_typed_id_value_bag_verified"], "Core complete typed bag not verified")
                if profile:
                    joins = hash_profile({"profile": item["profile"], "metrics": {"dbHits": item["db_hits"]}},
                                         not args.before and args.case == "full", args.require_spill)
                sample = {"full_us": item["full_us"], "full_cpu_us": item["full_cpu_us"],
                          "first_us": item["first_row_us"], "checksum": item["oracle"]["actual_row_order_blake3"],
                          "actual_rows": item["oracle"]["rows"]}
            expected = next(case["expected_rows"] for case in manifest["oracles"] if case["name"] == args.case)
            require(sample["actual_rows"] == expected,
                    "whole-row cardinality differs")
            if profile:
                sample["hash_metrics"] = [op["hash"] for op in joins if op.get("hash")]
                sample["explicit_quota_build_proof"] = [
                    {"build_rows": op["hash"]["buildRows"],
                     "minimum_retained_NodeId_bytes": op["hash"]["buildRows"]*8,
                     "memory_limit_bytes": args.memory_limit_bytes,
                     "build_larger_than_whole_quota_proven": bool(args.memory_limit_bytes
                         and op["hash"]["buildRows"]*8 > args.memory_limit_bytes),
                     "basis": "fixture full projection requires the build NodeId for every row; one i64 NodeId per retained build row, excluding key/container overhead"}
                    for op in joins if op.get("hash")]
            (diagnostics if profile else primary).append({"name": name, **sample, "process": process})
            save(prefix.with_suffix(".verified.json"), {"status": "passed", "primary": not profile, **sample})
            print(json.dumps({"event": "long_drain_completed", "name": name,
                              "surface": args.surface, "nodes": manifest["node_count"]}), flush=True)
    finally:
        db = sqlite.connect(args.extension, database, True)
        try:
            after_state = state(db)
        finally:
            db.close()
        after_hash, after_sidecars = file_hash(database), sidecars(database)
        save(folder / "postflight.json", {"fixture_sha256": after_hash, "sidecars": after_sidecars,
            "state": after_state, "helper_sha256": file_hash(args.helper),
            "unchanged": after_hash == before_hash and after_sidecars == before_sidecars and after_state == initial_state})
    require(after_hash == before_hash and after_sidecars == before_sidecars and after_state == initial_state,
            "long read changed immutable fixture")
    require(file_hash(args.helper) == helper_sha, "helper changed during long pressure")
    require(max(row["process"]["peak_rss_bytes"] for row in primary) <= 1024**3, "per-primary process RSS exceeded 1 GiB")
    raw = [row["full_us"] for row in primary]
    return {"manifest": str(manifest_path.resolve()), "scale": manifest["node_count"],
            "distribution": manifest["distribution"], "surface": args.surface, "case": args.case,
            "primary_raw": primary, "independent_profile_vm": diagnostics,
            "latency": {"raw_us": raw, "median_us": sorted(raw)[1], "max_us": max(raw), "p95": None},
            "before": args.before, "expected_source_id": expected_source,
            "complete_bag_and_fixture_unchanged": True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sqlite-library", help="matching Python/stock SQLite; pressure requires explicit path")
    parser.add_argument("--semantics-only", type=Path, metavar="EXT")
    parser.add_argument("--before", action="store_true", help="retained artifact sampling only; never product mode")
    commands = parser.add_subparsers(dest="command")
    seed = commands.add_parser("seed")
    seed.add_argument("extension", type=Path)
    seed.add_argument("--fixture-helper", type=Path, required=True)
    seed.add_argument("--database", type=Path, required=True)
    seed.add_argument("--manifest", type=Path, required=True)
    seed.add_argument("--nodes", type=int, required=True)
    seed.add_argument("--distribution", choices=("uniform", "skew", "bilateral-skew"), default="uniform")
    for name in ("smoke", "measure"):
        command = commands.add_parser(name)
        command.add_argument("extension", type=Path)
        command.add_argument("--manifest", type=Path, action="append", required=True)
        command.add_argument("--output", type=Path, required=True)
        command.add_argument("--before", action="store_true", default=argparse.SUPPRESS)
        command.add_argument("--memory-limit-bytes", type=int)
    long = commands.add_parser("long")
    long.add_argument("extension", type=Path)
    long.add_argument("--manifest", type=Path, action="append", required=True)
    long.add_argument("--output", type=Path, required=True)
    long.add_argument("--helper", type=Path, required=True)
    long.add_argument("--surface", choices=("rows", "core"), required=True)
    long.add_argument("--case", choices=("full", "small24"), default="full")
    long.add_argument("--before", action="store_true", default=argparse.SUPPRESS)
    long.add_argument("--memory-limit-bytes", type=int)
    long.add_argument("--require-spill", action="store_true")
    long.add_argument("--expected-core-source-id")
    long.add_argument("--provenance", type=Path)
    args = parser.parse_args()
    if args.semantics_only:
        require(args.command is None, "semantics-only cannot be combined with pressure")
        args.extension = args.semantics_only
        sqlite = select_sqlite(args.sqlite_library)
        print(json.dumps(semantics(args, sqlite), sort_keys=True))
        return
    require(args.command and args.sqlite_library, "pressure requires a command and explicit --sqlite-library")
    sqlite = select_sqlite(args.sqlite_library)
    if args.command == "seed":
        seed_fixture(args, sqlite)
        return
    require(args.memory_limit_bytes is None or args.memory_limit_bytes > 0,
            "an explicit memory quota must be positive; zero cannot silently omit the quota")
    args.connections, args.repeats, args.reopens = 5, 20, 100
    new_path(args.output)
    args.output.mkdir(parents=True)
    source = file_hash(Path(__file__))
    artifact = file_hash(args.extension)
    before_group = cgroup()
    results = []
    runtime = actual_sql_runtime(sqlite, args.extension)
    for index, manifest in enumerate(args.manifest):
        result = (long_fixture(args, sqlite, manifest, runtime) if args.command == "long"
                  else short_fixture(args, sqlite, manifest))
        save(args.output / f"{index:02d}-{result['distribution']}-{result['scale']}.json", result)
        results.append(result)
    require(file_hash(Path(__file__)) == source and file_hash(args.extension) == artifact,
            "harness or artifact changed during measurement")
    growth = short_growth(results) if args.command == "measure" and not args.before else []
    save(args.output / "summary.json", {"status": "passed", "command": args.command,
        "before": args.before, "sampling": ("3 independent complete primary processes +1 independent PROFILE/VM drain; no P95"
                                               if args.command == "long" else "single functional per surface plus PROFILE and separate recovery"
                                               if args.command == "smoke" else "fixed 5x20 warm +100 fresh; warm-first, VM/Profile and recovery separate"),
        "harness_sha256": source, "extension_sha256": artifact, "sqlite": sqlite.version,
        "sqlite_library": sqlite.path, "SQL_runtime": runtime,
        "cgroup_before": before_group, "cgroup_after": cgroup(),
        "fixed_output_VM_growth_checks": growth,
        "fixtures": [{key: value for key, value in result.items() if key not in ("sample", "recovery")}
                     for result in results]})


if __name__ == "__main__":
    main()
