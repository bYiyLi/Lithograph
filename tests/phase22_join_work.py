#!/usr/bin/env python3
"""S22.4 public SQL Join work on immutable synthetic fixtures.

Reuse the existing stock-SQLite ctypes adapter and its exact retained scalar /
rows statements. Seed only a NEW COW clone, using public reset/CREATE/Constraint
commands; never mutate the source fixture. Measurements retain complete fixed
1/24-row ID/value bags, not a count replacement. --before is harness sampling:
exactly three full executions per case (scalar VM, rows VM, rows PROFILE), with
no claim that those instrumented scans pass the After short-query latency gate.

First authorized smoke (no large seeds/benchmark):
  python3 -B tests/phase22_join_work.py --sqlite-library LIB seed-inl BEFORE \
    --source-manifest target/phase22-current/statistics-scale/statistics-1000.fixture.json \
    --database target/phase22-current/s4-join-work/inl-1000.sqlite3 \
    --manifest target/phase22-current/s4-join-work/inl-1000.json
  python3 -B tests/phase22_join_work.py --sqlite-library LIB smoke BEFORE --before \
    --manifest target/phase22-current/s4-join-work/inl-1000.json \
    --manifest target/phase22-current/s4-fixtures/smoke-1000.json \
    --output target/phase22-current/s4-join-work/smoke-before

Root schedules large measurements after the S4 artifact is frozen. Pressure /
seed require explicit --sqlite-library matching Python sqlite3 exactly. The
selfcontained --semantics-only EXT gate reuses the existing LITHOGRAPH_SQLITE3
library selector and never needs an ignored seed, checkpoint tool or manifest.
"""

import argparse
from collections import Counter
import ctypes as c
import json
import os
from pathlib import Path
import platform
import resource
import subprocess
import time

from phase22_access_work import cgroup
from phase22_relationship_work import SQLite, file_hash, require, select_sqlite, state


FORMAT = "phase22-join-work-v1"
BEFORE_SHA = "1de15a93b17447059e57e7eade5c6fa2ebc79aa9a477b225528d9642c1299dea"


def emit(value):
    print(json.dumps(value, sort_keys=True), flush=True)


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def sidecars(path):
    return {suffix: file.stat().st_size if file.exists() else 0
            for suffix in ("-wal", "-shm", "-journal")
            for file in (Path(str(path) + suffix),)}


def closed(path):
    require(Path(path).is_file(), f"missing fixture {path}")
    sizes = sidecars(path)
    require(sizes["-wal"] == sizes["-journal"] == 0, "source has live WAL/journal")
    return sizes


def new_path(path):
    require(not Path(path).exists() and not any(Path(str(path) + suffix).exists()
                for suffix in ("-wal", "-shm", "-journal")), f"path must be new: {path}")


def cow_clone(source, destination):
    new_path(destination)
    Path(destination).parent.mkdir(parents=True, exist_ok=True)
    if platform.system() == "Darwin":
        # macOS cp -c may silently fall back to a full copy; large fixtures must
        # fail closed when the filesystem cannot provide copy-on-write cloning.
        clonefile = c.CDLL(None, use_errno=True).clonefile
        clonefile.argtypes = [c.c_char_p, c.c_char_p, c.c_uint32]
        clonefile.restype = c.c_int
        if clonefile(os.fsencode(source), os.fsencode(destination), 0) != 0:
            raise OSError(c.get_errno(), "clonefile failed", str(destination))
        return "clonefile(2)"
    subprocess.run(["cp", "--reflink=always", "--", str(source), str(destination)],
                   check=True, capture_output=True, text=True)
    return "cp --reflink=always"


def bag(rows):
    return Counter(json.dumps(row, separators=(",", ":"), sort_keys=True) for row in rows)


def check_rows(rows, expected):
    require(bag(rows) == bag(expected), f"complete row bag differs: {rows!r} / {expected!r}")


def checkpoint_descriptor(value):
    value = value.removeprefix("commit/")
    require(len(value) == 64 and all(ch in "0123456789abcdef" for ch in value), "invalid anchor")
    return "commit/" + value


def peak_rss():
    value = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return value if platform.system() == "Darwin" else value * 1024


def oom_kills(group):
    return dict(line.split() for line in group.get("memory.events", "").splitlines()).get("oom_kill", "0")


def seed_inl(args, sqlite):
    """Only public graph writes on a new clone; original committed IDs are queried."""
    source_manifest = json.loads(args.source_manifest.read_text()) if args.source_manifest else {}
    source = Path(args.source or source_manifest.get("database", "")).resolve()
    anchor = checkpoint_descriptor(args.anchor or source_manifest.get("anchor", ""))
    count = args.nodes or source_manifest.get("nodes")
    require(count in (1000, 10000, 100000, 1000000), "use an existing approved S3 scale fixture")
    label, prop = ("ScaleNode", "scaleId") if count == 1000000 else ("Common", "v")
    source_sidecars = closed(source)
    source_sha = file_hash(source)
    if source_manifest.get("seed_sha256"):
        require(source_sha == source_manifest["seed_sha256"], "source manifest hash differs")
    new_path(args.database)
    new_path(args.manifest)
    require(args.database.resolve() != args.manifest.resolve(), "database and manifest must differ")
    inventory = sqlite.connect(args.extension, source, True)
    try:
        source_state = state(inventory)
    finally:
        inventory.close()
    clone = cow_clone(source, args.database)
    db = sqlite.connect(args.extension, args.database)
    try:
        db.execute("CALL lithograph.reset($target)", {"target": anchor})
        require(state(db)[0] == anchor.removeprefix("commit/"), "public reset did not pin anchor")
        original = db.execute(f"MATCH(i:{label}) WHERE i.{prop} IN [1,2,3,4] "
                              f"RETURN elementId(i),i.{prop}")["rows"]
        require(Counter(value for _, value in original) == Counter([1, 2, 3, 4]), "original keys are not singleton")
        outer = db.execute("UNWIND [1,1,2,2,3,3,4,4,null,999999999] AS key "
                           "CREATE(o:S4JoinOuter {key:key}) RETURN elementId(o),o.key")["rows"]
        db.execute("CREATE CONSTRAINT s4_outer_key FOR(o:S4JoinOuter) REQUIRE o.key IS :: INTEGER")
        added = db.execute(f"UNWIND [1,1,2,2,3,3,4,4] AS key CREATE(i:{label} {{{prop}:key}}) "
                           f"RETURN elementId(i),i.{prop}")["rows"]
        target = "commit/" + state(db)[0]
        current, delta, layers = target.removeprefix("commit/"), {}, 0
        while current != anchor.removeprefix("commit/"):
            require(layers < 8, "unexpectedly long seed delta")
            parent, layer = db.sql("SELECT lower(hex(parent1)),layer_id FROM _lithograph_commits "
                                   f"WHERE id=x'{current}'")[0]
            for kind in ("node", "label", "rel", "property"):
                delta[kind] = delta.get(kind, 0) + db.sql(
                    f"SELECT count(*) FROM _lithograph_{kind}_delta WHERE layer_id={layer}")[0][0]
            current, layers = parent, layers + 1
        require(sum(delta.values()) == 53, f"unexpected canonical raw delta: {delta}")
        oracle = [[0, oid, iid, key, value] for oid, key in outer for iid, value in original + added
                  if key is not None and key == value]
        require(len(outer) == 10 and len(added) == 8 and len(oracle) == 24, "seed multiplicity")
        require(len({id_ for id_, _ in outer + added}) == 18, "allocated IDs were reused")
        require(db.sql("PRAGMA quick_check") == [["ok"]], "clone quick_check")
        manifest = {"format": FORMAT, "kind": "inl", "database": str(args.database.resolve()),
                    "nodes": count, "anchor": target, "label": label, "property": prop,
                    "index": source_manifest.get("index", "scale_id" if count == 1000000 else "statistics_v"),
                    "outer_rows": outer, "original_inner_rows": original, "added_inner_rows": added,
                    "oracle": oracle, "raw_graph_delta": delta, "seed_layers": layers,
                    "seed_extension_sha256": file_hash(args.extension), "clone_method": clone,
                    "source": {"database": str(source), "sha256": source_sha, "state": source_state,
                               "anchor": anchor, "sidecars": source_sidecars}}
    finally:
        db.close()
    require(source_sha == file_hash(source) and closed(source) == source_sidecars, "source changed")
    inventory = sqlite.connect(args.extension, source, True)
    try:
        require(state(inventory) == source_state, "source Branch/history changed")
    finally:
        inventory.close()
    manifest["fixture_sha256"] = file_hash(args.database)
    manifest["sidecars"] = closed(args.database)
    args.manifest.parent.mkdir(parents=True, exist_ok=True)
    write_json(args.manifest, manifest)
    emit({**manifest, "kind": "join_inl_seed", "fixture_kind": manifest["kind"],
          "manifest": str(args.manifest)})


def ready_inl(args, sqlite):
    """A separate COW fixture; preserve the bounded-delta fixture as fallback evidence."""
    manifest, source, options = load_fixture(args.source_manifest)
    require(manifest["kind"] == "inl", "ready-inl requires our INL manifest")
    source_sha, source_sidecars = file_hash(source), closed(source)
    new_path(args.manifest)
    require(args.database.resolve() != args.manifest.resolve(), "database and manifest must differ")
    clone = cow_clone(source, args.database)
    db = sqlite.connect(args.extension, args.database)
    try:
        before_state, before_bytes = state(db), args.database.stat().st_size
        index = manifest.get("index", "scale_id" if manifest["nodes"] == 1000000 else "statistics_v")
        started, cpu = time.perf_counter_ns(), time.process_time_ns()
        rebuilt = db.execute("CALL lithograph.index.rebuild($name,'branch/main')", {"name": index})
        rebuild = {"wall_us": (time.perf_counter_ns() - started) / 1000,
                   "cpu_us": (time.process_time_ns() - cpu) / 1000,
                   "result": rebuilt, "before_bytes": before_bytes,
                   "after_bytes": args.database.stat().st_size}
        require(state(db) == before_state, "physical rebuild changed canonical Branch/history")
        target = options["at"].removeprefix("commit/")
        generations = db.sql("SELECT generation_id,lower(hex(anchor_commit)),complete,CAST(definition_blob AS TEXT) "
                             "FROM _lithograph_index_generations "
                             f"WHERE anchor_commit=x'{target}'")
        require(any(row[1] == target and row[2] == 1 and json.loads(row[3])["definition"]["name"] == index
                    for row in generations), "no matching ready generation at target")
    finally:
        db.close()
    require(file_hash(source) == source_sha and closed(source) == source_sidecars, "delta source changed")
    manifest.update(database=str(args.database.resolve()), index=index, clone_method=clone,
                    ready_rebuild=rebuild, ready_generations=generations,
                    bounded_delta_source={"database": str(source), "sha256": source_sha},
                    fixture_sha256=file_hash(args.database), sidecars=closed(args.database))
    args.manifest.parent.mkdir(parents=True, exist_ok=True)
    write_json(args.manifest, manifest)
    emit({"kind": "join_ready_seed", "database": str(args.database), "manifest": str(args.manifest),
          "rebuild": rebuild, "fixture_sha256": manifest["fixture_sha256"]})


def cases_for(manifest, parts, include_stress=False):
    if manifest.get("kind") == "inl":
        label, prop = manifest["label"], manifest["property"]
        prefix = f"MATCH(o:S4JoinOuter),(i:{label}) WHERE "
        output = f" RETURN 0,elementId(o),elementId(i),o.key,i.{prop}"
        equality = f"o.key=i.{prop}"
        return [{"name": "inl", "query": prefix + equality + output, "oracle": manifest["oracle"],
                 "canonical": prefix + f"({equality}) OR false" + output,
                 "operator": "IndexNestedLoop" if manifest.get("ready_rebuild") else None},
                {"name": "inl_null", "query": prefix + f"o.key IS NULL AND {equality}" + output,
                 "oracle": [], "operator": None},
                {"name": "inl_missing", "query": prefix + f"o.key=999999999 AND {equality}" + output,
                 "oracle": [], "operator": None},
                {"name": "inl_zero_outer", "query": f"MATCH(o:S4NeverOuter),(i:{label}) "
                 f"WHERE {equality} RETURN 1/0", "oracle": [], "operator": None}]
    require(manifest.get("kind") == "phase22_join_cycle_fixture", "unknown fixture manifest")
    cases = []
    for oracle in manifest["oracles"]:
        p = oracle["parts"]
        if p not in parts:
            continue
        projection = ",".join(["0"] + [f"elementId(n{i})" for i in range(p + 1)]
                              + [f"elementId(r{i})" for i in range(p)])
        patterns = [f"(n{i}:S4Chain)-[r{i}:S4Next]->(n{i+1}:{'S4Rare' if i+1==p else 'S4Chain'})"
                    for i in range(p)]
        for rotation in range(p):
            order = list(range(rotation, p)) + list(range(rotation - 1, -1, -1))
            query = "MATCH " + ",".join(patterns[i] for i in order) + " RETURN " + projection
            cases.append({"name": f"cycle_{p}_rotation_{rotation}", "query": query,
                          "oracle": [[0] + row for row in oracle["rows"]],
                          "operator": "cycle_loop", "physical_writing_order": order})
    if include_stress:
        rare = f"n:{manifest['rare_node']}"
        for count, rare_first in ((24, False), (64, True), (129, True)):
            patterns = ["(n:S4Chain)"] * count
            patterns[0 if rare_first else -1] = "(n:S4Rare)"
            cases.append({"name": f"budget_{count}_rare_first" if rare_first else "greedy_24_rare_last",
                          "query": "MATCH " + ",".join(patterns) + " RETURN 0,elementId(n)",
                          "oracle": [[0, rare]],
                          "operator": None if rare_first else "greedy_shared_loop"})
    require(cases, "no cycle cases selected")
    return cases


def load_fixture(path):
    manifest = json.loads(Path(path).read_text())
    database = Path(manifest.get("database", manifest.get("path", ""))).resolve()
    require(database.is_file(), "manifest database missing")
    anchor = checkpoint_descriptor(manifest.get("anchor", manifest.get("checkpoint_commit", "")))
    return manifest, database, {"at": anchor}


def consume(db, query, expected, options, surface="rows", profile=False,
            interrupt_after_first=False, early_close=False):
    """Drive the exact retained sqlite3_stmt; first means first DATA row, not columns."""
    statement = db.scalar_stmt if surface == "scalar" else db.rows_stmt
    bindings = (query, "{}", json.dumps(options))
    db.api.sqlite3_reset(statement.ptr)
    db.check(db.api.sqlite3_clear_bindings(statement.ptr))
    for index, value in enumerate(bindings, 1):
        data = value.encode()
        db.check(db.api.sqlite3_bind_text(statement.ptr, index, data, len(data), c.c_void_p(-1)))
    start, cpu = time.perf_counter_ns(), time.process_time_ns()
    first = first_cpu = None
    rows, summary, columns, terminal = [], None, None, False
    interrupted = False
    while True:
        rc = db.api.sqlite3_step(statement.ptr)
        if rc == 9 and interrupt_after_first:
            require(first is not None, "interrupted before first actual row")
            interrupted = True
            break
        if rc == 101:
            terminal = True
            break
        db.check(rc, 100)
        if surface == "scalar":
            first, first_cpu = time.perf_counter_ns() - start, time.process_time_ns() - cpu
            envelope = json.loads(db.api.sqlite3_column_text(statement.ptr, 0).decode())
            rows, summary = envelope["rows"], envelope["summary"]
        else:
            event = db.api.sqlite3_column_text(statement.ptr, 0).decode()
            data = db.api.sqlite3_column_text(statement.ptr, 1).decode()
            if event == "columns":
                columns = json.loads(data)
            elif event == "row":
                if first is None:
                    first, first_cpu = time.perf_counter_ns() - start, time.process_time_ns() - cpu
                rows.append(json.loads(data))
                require(len(rows) <= max(len(expected), 1), "unexpected output exceeds the fixed oracle bound")
                if interrupt_after_first:
                    db.api.sqlite3_interrupt(db.ptr)
                if early_close:
                    break
            elif event == "summary":
                summary = json.loads(data)
    full, full_cpu = time.perf_counter_ns() - start, time.process_time_ns() - cpu
    cleanup_us = cleanup_cpu_us = reset_rc = None
    if interrupt_after_first or early_close:
        cleanup_start, cleanup_cpu = time.perf_counter_ns(), time.process_time_ns()
        reset_rc = db.api.sqlite3_reset(statement.ptr)
        cleanup_us = (time.perf_counter_ns() - cleanup_start) / 1000
        cleanup_cpu_us = (time.process_time_ns() - cleanup_cpu) / 1000
        require(reset_rc in ((0, 9) if interrupt_after_first else (0,)), "unexpected cleanup/reset error")
        require(rows and (interrupted if interrupt_after_first else not terminal), "abort fixture did not abort")
    else:
        require(terminal and summary is not None, "query did not complete")
        check_rows(rows, expected)
    if profile:
        require(summary is not None and "profile" in summary, "PROFILE summary missing")
        require(sum(op["dbHits"] for op in summary["profile"]["operators"]) == summary["metrics"]["dbHits"],
                "PROFILE operator dbHits sum differs from summary.metrics.dbHits")
    return {"surface": surface, "first_data_us": None if first is None else first / 1000,
            "first_data_cpu_us": None if first_cpu is None else first_cpu / 1000,
            "full_us": full / 1000, "full_cpu_us": full_cpu / 1000,
            "row_count": len(rows), "columns": columns, "interrupted": interrupted,
            "early_closed": early_close, "metrics": summary.get("metrics") if summary else None,
            "cleanup_us": cleanup_us, "cleanup_cpu_us": cleanup_cpu_us, "cleanup_reset_rc": reset_rc,
            "profile": summary.get("profile") if profile and summary else None}


def measured(db, case, options, surface, traced=False, profile=False):
    if traced:
        db.start_work()
    try:
        result = consume(db, ("PROFILE " if profile else "") + case["query"], case["oracle"],
                         options, surface, profile=profile)
    finally:
        work = db.stop_work() if traced else None
    if work:
        result["sqlite_work"] = work
    return result


def timing_summary(samples):
    result = {}
    for key in ("first_data_us", "first_data_cpu_us", "full_us", "full_cpu_us"):
        values = sorted(sample[key] for sample in samples if sample[key] is not None)
        result[key] = None if not values else {
            "n": len(values), "p50": values[len(values) // 2],
            "p95": values[min(len(values) - 1, int(len(values) * .95))], "max": values[-1]}
    return result


def check_plan(plan, profile, case, before):
    if before or case["operator"] is None:
        return
    operators = profile["operators"]
    if case["operator"] == "cycle_loop":
        require(any(name in plan for name in ("NestedLoop", "IndexNestedLoop")), "cycle has no actual Join")
        require(any(op["operator"] == "AdjacencySeek" and op["dbHits"] > 0 for op in operators),
                "cycle has no real adjacency access")
    elif case["operator"] == "greedy_shared_loop":
        first = plan.splitlines()[0]
        require("LabelIndexScan" in first and 'label: "S4Rare"' in first,
                "Greedy did not begin with the rare label seed")
        require(any(op["operator"] == "NestedLoop" for op in operators), "Greedy omitted actual NestedLoop")
        seed = next((op for op in operators if op["id"] == int(first.split()[0])), None)
        require(seed and seed["operator"] == "LabelIndexScan" and seed["rows"] == 1
                and 0 < seed["dbHits"] <= 3, "Greedy rare seed is not one actual candidate")
    else:
        require("IndexNestedLoop" in plan, "INL did not select IndexNestedLoop")
        require(any(op["operator"] == "IndexNestedLoop" for op in operators), "PROFILE omitted IndexNestedLoop")
        require(any(op["operator"] == "IndexSeek" and op["dbHits"] > 0 for op in operators),
                "INL has no actual IndexSeek")


def preflight_plan(sqlite, args, database, options, case):
    """Reject a missing S4 producer before repeating any potentially wide fallback."""
    db = sqlite.connect(args.extension, database, True)
    try:
        plan = db.execute("EXPLAIN " + case["query"], options=options)["rows"][0][0]
        if case["operator"] in ("cycle_loop", "greedy_shared_loop"):
            require(any(name in plan for name in ("NestedLoop", "IndexNestedLoop")), "cycle producer missing before pressure")
            if case["operator"] == "greedy_shared_loop":
                first = plan.splitlines()[0]
                require("LabelIndexScan" in first and 'label: "S4Rare"' in first,
                        "Greedy rare seed missing before pressure")
        elif case["operator"]:
            require(case["operator"] in plan, "INL producer missing before pressure")
        return plan
    finally:
        db.close()


def before_case(sqlite, args, database, options, case):
    db = sqlite.connect(args.extension, database, True)
    try:
        handles = (db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value)
        scalar = measured(db, case, options, "scalar", traced=True)
        rows = measured(db, case, options, "rows", traced=True)
        profile = measured(db, case, options, "rows", profile=True)
        plan = db.execute("EXPLAIN " + case["query"], options=options)["rows"][0][0]
        require(handles == (db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value), "retained handles replaced")
        return {"sampling": "three full scans: scalar trace + rows trace + rows PROFILE; instrumented, not After latency gate",
                "full_executions": 3, "plan": plan, "scalar_vm": scalar,
                "rows_vm": rows, "profile_sample": profile, "retained_statement_handles": True}
    finally:
        db.close()


def after_case(sqlite, args, database, options, case):
    plan = preflight_plan(sqlite, args, database, options, case)
    result = {"sampling": "untraced first/warm/fresh; VM and PROFILE diagnostics separate",
              "untimed_separate_connection_preflight_plan": plan, "surfaces": {}}
    for surface in ("scalar", "rows"):
        first, warm, fresh = [], [], []
        for connection in range(args.connections):
            db = sqlite.connect(args.extension, database, True)
            try:
                handles = (db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value)
                first.append(measured(db, case, options, surface))
                warm.append([measured(db, case, options, surface) for _ in range(args.repeats)])
                require(handles == (db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value), "warm handles replaced")
            finally:
                db.close()
        for _ in range(args.reopens):
            db = sqlite.connect(args.extension, database, True)
            try:
                fresh.append(measured(db, case, options, surface))
            finally:
                db.close()
        db = sqlite.connect(args.extension, database, True)
        try:
            fresh_vm = measured(db, case, options, surface, traced=True)
            warm_vm = measured(db, case, options, surface, traced=True)
            profile = measured(db, case, options, surface, profile=True)
            plan = db.execute("EXPLAIN " + case["query"], options=options)["rows"][0][0]
            check_plan(plan, profile["profile"], case, False)
        finally:
            db.close()
        result["surfaces"][surface] = {"connection_first": first, "warm_raw": warm,
            "fresh_raw": fresh, "fresh_vm": fresh_vm, "warm_vm": warm_vm,
            "profile_sample": profile, "plan": plan,
            "first_summary": timing_summary(first),
            "warm_summary": timing_summary([sample for group in warm for sample in group]),
            "fresh_summary": timing_summary(fresh),
            "full_executions": args.connections * (1 + args.repeats) + args.reopens + 3}
    return result


def recovery(sqlite, args, database, options, case):
    require(len(case["oracle"]) > 1, "recovery needs a genuinely multi-row read")
    db = sqlite.connect(args.extension, database, True)
    db.api.sqlite3_interrupt.argtypes, db.api.sqlite3_interrupt.restype = [c.c_void_p], None
    try:
        before = state(db)
        original = (db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value)
        interrupt = consume(db, case["query"], case["oracle"], options, interrupt_after_first=True)
        measured(db, case, options, "rows")
        close = consume(db, case["query"], case["oracle"], options, early_close=True)
        measured(db, case, options, "rows")
        measured(db, case, options, "scalar")
        require(db.api.sqlite3_get_autocommit(db.ptr) == 1 and state(db) == before, "recovery changed state")
        require(original == (db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value), "recovery recreated handles")
        return {"interrupt": interrupt, "early_close": close, "recovered_same_handles": True,
                "autocommit": True, "state_unchanged": True}
    finally:
        db.close()


def run_fixture(args, sqlite, manifest_path):
    manifest, database, options = load_fixture(manifest_path)
    source_hash, source_sidecars = file_hash(database), closed(database)
    if manifest.get("fixture_sha256"):
        require(source_hash == manifest["fixture_sha256"], "Join fixture hash differs")
    inventory = sqlite.connect(args.extension, database, True)
    try:
        before_state = state(inventory)
    finally:
        inventory.close()
    case_list = cases_for(manifest, args.parts, include_stress=bool(args.case))
    scale = manifest.get("nodes", manifest.get("node_count"))
    if args.command == "measure":
        if args.before:
            require(scale <= args.before_max_nodes, "Before wide scan exceeds the explicitly scheduled sampling cap")
        if manifest["kind"] == "inl":
            require(manifest.get("ready_rebuild"), "INL performance requires the explicit current-head ready fixture")
            # NULL/missing/zero-outer are correctness controls in the 1k smoke
            # and selfcontained gate, not 410 wide fallback latency samples.
            case_list = case_list[:1]
    if args.case:
        case_list = [case for case in case_list if case["name"] in args.case]
    require(case_list, "no selected case in fixture")
    fixture_output = args.output / (manifest["kind"] + "-" + str(manifest.get("nodes", manifest.get("node_count"))))
    new_path(fixture_output)
    fixture_output.mkdir()
    if args.command == "smoke" and not args.case:
        # Minimal seed/oracle smoke, not the default multi-connection benchmark.
        case_list = [case_list[0], case_list[-1]] if manifest.get("kind") != "inl" else case_list
    samples = []
    for case in case_list:
        if args.command == "smoke":
            db = sqlite.connect(args.extension, database, True)
            try:
                sample = {surface: measured(db, case, options, surface, traced=True)
                          for surface in ("scalar", "rows")}
                if case.get("canonical"):
                    check_rows(db.execute(case["canonical"], options=options)["rows"], case["oracle"])
                sample["plan"] = db.execute("EXPLAIN " + case["query"], options=options)["rows"][0][0]
                if not args.before:
                    sample["profile_sample"] = measured(db, case, options, "rows", profile=True)
                    check_plan(sample["plan"], sample["profile_sample"]["profile"], case, False)
            finally:
                db.close()
        else:
            sample = before_case(sqlite, args, database, options, case) if args.before else after_case(sqlite, args, database, options, case)
        record = {"name": case["name"], "query": case["query"], "complete_oracle": case["oracle"], **sample}
        write_json(fixture_output / (case["name"] + ".json"), record)
        samples.append(record)
    recovered = None
    if manifest.get("kind") == "inl" and (args.command == "smoke" or not args.before):
        recovery_case = next((case for case in case_list if len(case["oracle"]) > 1), None)
        recovered = (recovery(sqlite, args, database, options, recovery_case) if recovery_case
                     else {"status": "not_applied_no_selected_multi_row_case"})
    inventory = sqlite.connect(args.extension, database, True)
    try:
        require(state(inventory) == before_state, "read changed Branch/history")
    finally:
        inventory.close()
    require(file_hash(database) == source_hash and sidecars(database) == source_sidecars, "read changed source/sidecars")
    return {"manifest": str(Path(manifest_path).resolve()), "database": str(database), "snapshot": options,
            "fixture_sha256": source_hash, "before_state": before_state, "sidecars": source_sidecars,
            "kind": manifest["kind"], "scale": manifest.get("nodes", manifest.get("node_count")),
            "samples": samples, "recovery": recovered}


def growth_gate(results):
    """Only After work is subjected to 10x <= 2x + 1000, never baseline scans."""
    groups, checks = {}, []
    for fixture in results:
        for case in fixture["samples"]:
            for surface, values in case["surfaces"].items():
                for slot in ("fresh_vm", "warm_vm"):
                    key = (fixture["kind"], case["name"], surface, slot)
                    groups.setdefault(key, []).append((fixture["scale"], values[slot]["sqlite_work"]["vm_steps"]))
    for key, values in groups.items():
        values.sort()
        for (small, low), (large, high) in zip(values, values[1:]):
            if large == small * 10:
                require(high <= low * 2 + 1000, f"Join VM growth gate: {key}: {values}")
                checks.append({"case": key, "small_scale": small, "large_scale": large,
                               "small_vm": low, "large_vm": high, "limit": low * 2 + 1000})
    return checks


def date_semantics(args, sqlite):
    """Keep unsupported dynamic Date probes on safe joins, in an isolated graph."""
    import tempfile

    with tempfile.TemporaryDirectory(prefix="lithograph-s4-date-sql-") as directory:
        path = Path(directory) / "date.sqlite3"
        db = sqlite.connect(args.extension, path)
        try:
            db.sql("SELECT lithograph_init()")
            inner = db.execute("UNWIND range(1,256) AS day "
                               "CREATE(i:DateInner{k:date({year:2026,ordinalDay:day})}) "
                               "RETURN elementId(i),i.k")["rows"]
            outer = db.execute("UNWIND[5,5,211,211,null] AS day CREATE(o:DateOuter{k:"
                               "CASE WHEN day IS NULL THEN null ELSE date({year:2026,ordinalDay:day}) END}) "
                               "RETURN elementId(o),o.k")["rows"]
            for target, variable in (("(i:DateInner)", "i"), ("(o:DateOuter)", "o")):
                db.execute(f"CREATE CONSTRAINT date_{variable}_type FOR{target} REQUIRE {variable}.k IS :: DATE")
            db.execute("CREATE RANGE INDEX date_inner_keys FOR(i:DateInner) ON(i.k)")
            started = time.perf_counter_ns()
            rebuild = db.execute("CALL lithograph.index.rebuild('date_inner_keys','branch/main')")
            setup = {"wall_us": (time.perf_counter_ns() - started) / 1000, "result": rebuild}
            require(len(inner) == 256 and len({id_ for id_, _ in inner}) == 256,
                    "Date inner setup IDs")
            require(len({json.dumps(value, sort_keys=True) for _, value in inner}) == 256,
                    "Date inner keys are not distinct")
            require(len(outer) == 5 and len({id_ for id_, _ in outer}) == 5
                    and sum(value is None for _, value in outer) == 1, "Date outer setup")
            oracle = [[0, oid, iid, key, value] for oid, key in outer for iid, value in inner
                      if key is not None and key == value]
            require(len(oracle) == 4, "Date duplicate-key bag")
            snapshot = state(db)
        finally:
            db.close()
        initial_hash, initial_sidecars = file_hash(path), closed(path)
        options = {"at": "commit/" + snapshot[0]}
        db = sqlite.connect(args.extension, path, True)
        try:
            cases = []
            for name, predicate in (("date_key_equality", "o.k=i.k"),
                                    ("date_key_equality_reversed", "i.k=o.k")):
                prefix = "MATCH(o:DateOuter),(i:DateInner) WHERE "
                output = " RETURN 0,elementId(o),elementId(i),o.k,i.k"
                case = {"query": prefix + predicate + output, "oracle": oracle}
                item = {"name": name, **case}
                for surface in ("scalar", "rows"):
                    item[surface] = measured(db, case, options, surface)
                item["plan"] = db.execute("EXPLAIN " + case["query"], options=options)["rows"][0][0]
                item["profile_sample"] = measured(db, case, options, "rows", profile=True)
                require("IndexNestedLoop" not in item["plan"], "Date selected unsupported dynamic IndexNestedLoop")
                require(not any(op["operator"] == "IndexNestedLoop"
                                for op in item["profile_sample"]["profile"]["operators"]),
                        "Date PROFILE executed unsupported dynamic IndexNestedLoop")
                canonical = prefix + "(" + predicate + ") OR false" + output
                item["canonical_query"] = canonical
                for surface in ("scalar", "rows"):
                    check_rows(db.execute(canonical, options=options, surface=surface)["rows"], oracle)
                cases.append(item)
            require(state(db) == snapshot and db.api.sqlite3_get_autocommit(db.ptr) == 1,
                    "Date semantic reads changed state")
        finally:
            db.close()
        require(file_hash(path) == initial_hash and sidecars(path) == initial_sidecars,
                "Date semantic reads changed main/sidecars")
    return {"cases": cases, "setup_rebuild": setup, "outer_rows": outer,
            "matched_inner_rows": [row for row in inner if any(row[0] == result[2] for result in oracle)],
            "inner_count": 256, "outer_count": 5, "complete_oracle": oracle,
            "snapshot": options, "readonly_hash_state_sidecars_unchanged": True}


def semantics(args, sqlite):
    """Small permanent public SQL gate; no ignored seeds, generator, or COW prerequisite."""
    import tempfile

    cases = []
    with tempfile.TemporaryDirectory(prefix="lithograph-s4-sql-") as directory:
        path = Path(directory) / "semantic.sqlite3"
        db = sqlite.connect(args.extension, path)
        try:
            db.sql("SELECT lithograph_init()")
            outer = db.execute("UNWIND[1,1,2,2,3,3,4,4,null,999999999] AS key "
                               "CREATE(o:CIJoinOuter{key:key}) RETURN elementId(o),o.key")["rows"]
            inner = db.execute("UNWIND[1,1,1,2,2,2,3,3,3,4,4,4] AS key "
                               "CREATE(i:CIJoinInner{key:key}) RETURN elementId(i),i.key")["rows"]
            db.execute("UNWIND range(100,163) AS key CREATE(:CIJoinInner{key:key})")
            rel = []
            for key in range(1, 5):
                end = "a" if key == 1 else "b"
                created = db.execute(
                    "CREATE(a:CIEndpoint),(b:CIEndpoint),(a)-[r0:CIJoinRel{key:$key}]->(b),"
                    "(a)-[r1:CIJoinRel{key:$key}]->(b),"
                    f"(a)-[r2:CIJoinRel{{key:$key}}]->({end}) "
                    "RETURN elementId(a),elementId(b),elementId(r0),elementId(r1),elementId(r2)",
                    {"key": key})["rows"][0]
                start, target = created[:2]
                rel.extend((start, rid, start if key == 1 and ordinal == 2 else target, key)
                           for ordinal, rid in enumerate(created[2:]))
            db.execute("MATCH(a:CIEndpoint),(b:CIEndpoint) WHERE elementId(a)=$a AND elementId(b)=$b "
                       "UNWIND range(100,163) AS key CREATE(a)-[:CIJoinRel{key:key}]->(b)",
                       {"a": rel[0][0], "b": rel[0][2]})
            for target, variable in [("(o:CIJoinOuter)", "o"), ("(i:CIJoinInner)", "i"), ("()-[r:CIJoinRel]-()", "r")]:
                db.execute(f"CREATE CONSTRAINT {variable}_key_type FOR {target} REQUIRE {variable}.key IS :: INTEGER")
            for query in ["CREATE RANGE INDEX ci_inner FOR(i:CIJoinInner) ON(i.key)",
                          "CREATE RANGE INDEX ci_rel FOR()-[r:CIJoinRel]-() ON(r.key)"]:
                db.execute(query)
            setup = []
            for name in ("ci_inner", "ci_rel"):
                started = time.perf_counter_ns()
                rebuild = db.execute("CALL lithograph.index.rebuild($name,'branch/main')", {"name": name})
                setup.append({"name": name, "wall_us": (time.perf_counter_ns() - started) / 1000,
                              "result": rebuild})
            require(len(rel) == 12 and len({row[1] for row in rel}) == 12, "Relationship setup IDs")
            node_rows = [[0, oid, iid, key, value] for oid, key in outer for iid, value in inner
                         if key is not None and key == value]
            rel_rows = [[0, oid, rid, start, end, key] for oid, key in outer for start, rid, end, value in rel
                        if key is not None and key == value]
            node_query = "MATCH(o:CIJoinOuter),(i:CIJoinInner) WHERE o.key=i.key RETURN 0,elementId(o),elementId(i),o.key,i.key"
            projection = " RETURN 0,elementId(o),elementId(r),elementId(a),elementId(b),o.key"
            directed = "MATCH(o:CIJoinOuter),(a)-[r:CIJoinRel]->(b) WHERE o.key=r.key" + projection
            incoming = "MATCH(o:CIJoinOuter),(b)<-[r:CIJoinRel]-(a) WHERE o.key=r.key" + projection
            first = next(oid for oid, key in outer if key == 1)
            undirected = f"MATCH(o:CIJoinOuter),(a)-[r:CIJoinRel]-(b) WHERE elementId(o)='{first}' AND o.key=r.key" + projection
            undirected_rows = [[0, first, rid, a, b, value] for start, rid, end, value in rel if value == 1
                               for a, b in ([(start, end)] if start == end else [(start, end), (end, start)])]
            loop_rows = [[0, rid] for start, rid, end, _ in rel if start == end]
            graph_query = node_query.replace("RETURN 0,elementId(o),elementId(i),o.key,i.key", "RETURN 0,o,i,o.key,i.key")
            masked_graph_rows = [[0, {"$type": "Node", "elementId": oid, "labels": ["CIJoinOuter"], "properties": {}},
                                 {"$type": "Node", "elementId": iid, "labels": ["CIJoinInner"], "properties": {}}, key, value]
                                for _, oid, iid, key, value in node_rows]
            cases = [
                {"name": "node_duplicate_bag", "query": node_query, "oracle": node_rows, "operator": "IndexNestedLoop"},
                {"name": "node_null", "query": node_query.replace("WHERE ", "WHERE o.key IS NULL AND "), "oracle": [], "operator": None},
                {"name": "node_missing", "query": node_query.replace("WHERE ", "WHERE o.key=999999999 AND "), "oracle": [], "operator": None},
                {"name": "zero_outer_error", "query": "MATCH(o:CINever),(i:CIJoinInner) WHERE o.key=i.key RETURN 1/0", "oracle": [], "operator": None},
                {"name": "relationship_outgoing", "query": directed, "oracle": rel_rows, "operator": "IndexNestedLoop"},
                {"name": "relationship_incoming", "query": incoming, "oracle": rel_rows, "operator": "IndexNestedLoop"},
                {"name": "relationship_undirected", "query": undirected, "oracle": undirected_rows, "operator": None},
                {"name": "self_loop_once", "query": "MATCH(a)-[r:CIJoinRel]-(a) WHERE r.key=1 RETURN 0,elementId(r)", "oracle": loop_rows, "operator": None},
                {"name": "result_mask_keeps_explicit_scalars", "query": graph_query, "oracle": masked_graph_rows,
                 "operator": "IndexNestedLoop", "options": {"readAccess": {"result": {"denyProperties": ["key"]}}}},
                {"name": "query_mask_prevents_property_join", "query": graph_query, "oracle": [],
                 "operator": None, "no_property_index": True,
                 "options": {"readAccess": {"query": {"denyProperties": ["key"]}}}}]
        finally:
            db.close()
        initial_hash = file_hash(path)
        db = sqlite.connect(args.extension, path, True)
        try:
            samples = []
            for case in cases:
                item = {"name": case["name"], "query": case["query"], "oracle": case["oracle"]}
                options = case.get("options", {})
                item["options"] = options
                for surface in ("scalar", "rows"):
                    item[surface] = measured(db, case, options, surface)
                item["plan"] = db.execute("EXPLAIN " + case["query"], options=options)["rows"][0][0]
                item["profile_sample"] = measured(db, case, options, "rows", profile=True)
                check_plan(item["plan"], item["profile_sample"]["profile"], case, args.before)
                if case.get("no_property_index"):
                    require("IndexSeek" not in item["plan"] and "IndexNestedLoop" not in item["plan"], "query mask bypassed in plan")
                    require(not any(op["operator"] in ("IndexSeek", "IndexNestedLoop")
                                    for op in item["profile_sample"]["profile"]["operators"]), "query mask bypassed in PROFILE")
                if "WHERE" in case["query"]:
                    left, right = case["query"].split(" WHERE ", 1)
                    predicate, projection = right.split(" RETURN ", 1)
                    canonical = left + " WHERE (" + predicate + ") OR false RETURN " + projection
                    for surface in ("scalar", "rows"):
                        check_rows(db.execute(canonical, options=options, surface=surface)["rows"], case["oracle"])
                samples.append(item)
        finally:
            db.close()
        require(file_hash(path) == initial_hash, "semantic reads changed main")
    date_fixture = date_semantics(args, sqlite)
    require(file_hash(args.extension) == args.artifact_sha256, "extension changed during semantic run")
    emit({"kind": "join_semantics", "sqlite": sqlite.version, "sqlite_library": sqlite.path,
          "extension_sha256": args.artifact_sha256, "before": args.before,
          "setup_rebuilds": setup, "cases": samples, "date_fixture": date_fixture, "status": "passed"})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sqlite-library", help="required for pressure/seed; semantics can select a matching stock library")
    parser.add_argument("--semantics-only", type=Path, help="selfcontained permanent SQL gate; no ignored fixtures")
    commands = parser.add_subparsers(dest="command")
    seed = commands.add_parser("seed-inl")
    seed.add_argument("extension", type=Path)
    seed.add_argument("--source-manifest", type=Path)
    seed.add_argument("--source", type=Path)
    seed.add_argument("--nodes", type=int)
    seed.add_argument("--anchor")
    seed.add_argument("--database", type=Path, required=True)
    seed.add_argument("--manifest", type=Path, required=True)
    ready = commands.add_parser("ready-inl")
    ready.add_argument("extension", type=Path)
    ready.add_argument("--source-manifest", type=Path, required=True)
    ready.add_argument("--database", type=Path, required=True)
    ready.add_argument("--manifest", type=Path, required=True)
    semantic = commands.add_parser("semantics")
    semantic.add_argument("extension", type=Path)
    semantic.add_argument("--before", action="store_true", help="oracle smoke only, no S4 physical-operator assertions")
    for command in ("smoke", "measure"):
        measure = commands.add_parser(command)
        measure.add_argument("extension", type=Path)
        measure.add_argument("--manifest", type=Path, action="append", required=True)
        measure.add_argument("--output", type=Path, required=True)
        measure.add_argument("--before", action="store_true")
        measure.add_argument("--before-max-nodes", type=int, choices=(1000, 10000), default=1000,
                             help="wide Before defaults to 1k; only Root-scheduled representative 10k can opt in")
        measure.add_argument("--parts", type=lambda s: [int(x) for x in s.split(",")], default=list(range(3, 9)))
        measure.add_argument("--case", action="append",
                             help="repeat exact case names; stress cases are available only when explicitly selected")
        measure.add_argument("--connections", type=int, default=5)
        measure.add_argument("--repeats", type=int, default=20)
        measure.add_argument("--reopens", type=int, default=100)
    args = parser.parse_args()
    require(bool(args.semantics_only) != bool(args.command), "choose a subcommand or --semantics-only")
    if args.semantics_only:
        args.extension, args.command, args.before = args.semantics_only, "semantics", False
    require(args.command == "semantics" or args.sqlite_library, "pressure/seed requires explicit --sqlite-library")
    sqlite = select_sqlite(args.sqlite_library) if args.command == "semantics" else SQLite(args.sqlite_library)
    require(args.extension.is_file(), "extension missing")
    args.artifact_sha256 = file_hash(args.extension)
    if getattr(args, "before", False):
        require(args.artifact_sha256 == BEFORE_SHA, "--before requires the frozen reviewed S3 artifact")
    if args.command in ("seed-inl", "ready-inl"):
        require(file_hash(args.extension) == BEFORE_SHA, "seed with the frozen reviewed S3 Before artifact")
        (seed_inl if args.command == "seed-inl" else ready_inl)(args, sqlite)
        return
    if args.command == "semantics":
        semantics(args, sqlite)
        return
    require(min(args.connections, args.repeats, args.reopens) > 0, "sampling counts must be positive")
    require(args.parts and set(args.parts) <= set(range(3, 9)), "cycle parts must be in 3..8")
    if args.case:
        available, selected_manifests = set(), []
        for path in args.manifest:
            manifest, _, _ = load_fixture(path)
            candidates = cases_for(manifest, args.parts, include_stress=True)
            if args.command == "measure" and manifest.get("kind") == "inl":
                candidates = candidates[:1]
            available.update(case["name"] for case in candidates)
            if any(case["name"] in args.case for case in candidates):
                selected_manifests.append(path)
        require(set(args.case) <= available, f"unknown case selectors: {sorted(set(args.case) - available)}")
        args.manifest = selected_manifests
    new_path(args.output)
    args.output.mkdir(parents=True)
    initial_group = cgroup()
    results = [run_fixture(args, sqlite, manifest) for manifest in args.manifest]
    growth = growth_gate(results) if args.command == "measure" and not args.before else []
    final_group = cgroup()
    require(oom_kills(initial_group) == oom_kills(final_group), "cgroup oom_kill changed")
    require(file_hash(args.extension) == args.artifact_sha256, "extension changed during query sampling")
    result = {"format": FORMAT, "command": args.command, "before": args.before,
              "artifact": str(args.extension.resolve()), "artifact_sha256": args.artifact_sha256,
              "sqlite": sqlite.version, "sqlite_library": sqlite.path, "platform": platform.platform(),
              "process_peak_rss_bytes": peak_rss(), "cgroup_before": initial_group,
              "cgroup_after": final_group, "fixtures": results,
              "growth_checks": growth,
              "growth_gate_status": ("measured" if growth else "not_measured_no_adjacent_10x_scales")
              if args.command == "measure" and not args.before else "not_applied_to_before_or_smoke",
              "cold_definition": "fresh connection/retained statements; no OS cache flush; inventory/hash untimed"}
    write_json(args.output / "results.json", result)
    emit({"kind": "join_work_complete", "output": str(args.output), "fixtures": len(results),
          "before": args.before, "sqlite": sqlite.version, "peak_rss_bytes": result["process_peak_rss_bytes"]})


if __name__ == "__main__":
    main()
