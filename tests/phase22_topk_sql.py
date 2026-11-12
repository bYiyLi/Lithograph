#!/usr/bin/env python3
"""S22.6 public SQL TopK smoke and bounded shared-fixture measurements.

Default: public init/CREATE/DDL on a disposable in-memory 1k graph. With
--database, open an existing shared S6 fixture read-only; never seed/rebuild it.
Timing samples are untraced. PROFILE and exact host SQLite stmt_status work
are separate diagnostics, never latency/P95 samples. No work runs on import.
"""

import argparse
from contextlib import contextmanager
import ctypes as c
import json
from pathlib import Path
import platform
import resource
import sys
import time

from phase22_relationship_work import SqlError, file_hash, require, select_sqlite
from phase22_hash_work import timing_summary


QUERY = ("MATCH (n:S6Probe) WHERE n.k >= 0 AND n.k < 1000000 "
         "RETURN n.k ORDER BY n.k ASC SKIP 257 LIMIT 20")
ORACLE = QUERY.replace("WHERE n.k >= 0 AND n.k < 1000000",
                       "WHERE (n.k >= 0 AND n.k < 1000000) OR false")
PARAMS = {}
EXPECTED = [[value] for value in range(257, 277)]
MEMORY = 536870912


def save(path, value):
    with Path(path).open("x") as output:
        json.dump(value, output, indent=2, sort_keys=True)
        output.write("\n")


def typed_rows(actual, expected):
    require(actual == expected, f"complete ordered values differ: {actual!r} / {expected!r}")
    require(all(isinstance(row, list) and len(row) == 1 and type(row[0]) is int
                for row in actual), "all projected values must be INTEGER, not FLOAT/Boolean/null")


def capture(db, query, options, destination, expected=EXPECTED, traced=False, error=None, surface="rows"):
    """A bounded step loop on the original retained public rows statement."""
    save(destination.with_suffix(".input.json"), {"query": query, "params": PARAMS, "options": options,
                                               "expected_rows": expected, "traced": traced, "expected_error": error,
                                               "surface": surface})
    stmt = (db.rows_stmt if surface == "rows" else db.scalar_stmt).ptr
    initial_reset = db.api.sqlite3_reset(stmt)
    resets = getattr(db, "topk_expected_resets", {})
    require(initial_reset in (0, resets.get(surface, 0)), "unexpected initial cursor reset error")
    db.check(db.api.sqlite3_clear_bindings(stmt))
    for index, text in enumerate((query, json.dumps(PARAMS), json.dumps(options)), 1):
        encoded = text.encode()
        db.check(db.api.sqlite3_bind_text(stmt, index, encoded, len(encoded), c.c_void_p(-1)))
    events, rows = [], []
    columns = summary = first = first_cpu = None
    result = {"query": query, "params": PARAMS, "options": options, "events": events,
              "rows": rows, "traced": traced, "surface": surface,
              "sqlite_done": False, "initial_reset_returncode": initial_reset}
    work_started = False
    if traced:
        db.start_work()
        work_started = True
    started, cpu = time.perf_counter_ns(), time.process_time_ns()
    full = full_cpu = None
    try:
        while True:
            rc = db.api.sqlite3_step(stmt)
            if rc == 101:
                full, full_cpu = time.perf_counter_ns() - started, time.process_time_ns() - cpu
                result["sqlite_done"] = True
                break
            result["last_step_returncode"] = rc
            db.check(rc, 100)
            if surface == "scalar":
                require(not events and db.api.sqlite3_column_count(stmt) == 1
                        and db.api.sqlite3_column_type(stmt, 0) == 3, "one TEXT scalar envelope required")
                first, first_cpu = time.perf_counter_ns() - started, time.process_time_ns() - cpu
                raw = db.api.sqlite3_column_text(stmt, 0).decode()
                events.append({"surface": "scalar", "data": raw})
                envelope = json.loads(raw)
                columns, actual, summary = envelope["columns"], envelope["rows"], envelope["summary"]
                require(isinstance(actual, list) and len(actual) <= 20, "fixed scalar result bound exceeded")
                rows.extend(actual)
                continue
            require(db.api.sqlite3_column_count(stmt) == 2, "two public event/data columns required")
            require(all(db.api.sqlite3_column_type(stmt, i) == 3 for i in (0, 1)), "TEXT event/data required")
            event, raw = (db.api.sqlite3_column_text(stmt, i).decode() for i in (0, 1))
            if event == "row" and first is None:
                first, first_cpu = time.perf_counter_ns() - started, time.process_time_ns() - cpu
            events.append({"event": event, "data": raw})
            value = json.loads(raw)
            if columns is None:
                require(event == "columns" and isinstance(value, list), "columns must be first")
                columns = value
            elif event == "row":
                require(summary is None, "row after summary")
                rows.append(value)
                require(len(rows) <= 20, "fixed result bound exceeded")
            else:
                require(event == "summary" and summary is None and isinstance(value, dict),
                        "unexpected/duplicate event or non-object summary")
                summary = value
        require(error is None, f"expected error was suppressed: {error}")
        require(columns == ["n.k"] and summary is not None, "columns/terminal summary differ")
        typed_rows(rows, expected)
        require(summary["queryType"] == "read" and summary["commit"] == options["at"], "read pin/type differs")
        require(summary["metrics"]["rows"] == len(expected), "summary row count differs")
        require(all(value == 0 for value in summary["counters"].values()), "read mutated graph/schema")
    except BaseException as failure:
        if full is None:
            full, full_cpu = time.perf_counter_ns() - started, time.process_time_ns() - cpu
        result["error"] = repr(failure)
        if not (error and isinstance(failure, SqlError) and error in str(failure)):
            raise
        require(summary is None and not result["sqlite_done"], "failed execution published success")
        result["expected_error_observed"] = error
    finally:
        result.update(first_data_us=None if first is None else first / 1000,
                      first_data_cpu_us=None if first_cpu is None else first_cpu / 1000,
                      full_us=full / 1000, full_cpu_us=full_cpu / 1000,
                      columns=columns, summary=summary)
        # Save raw/partial evidence before reset/trace assertions can fail.
        reset_rc = db.api.sqlite3_reset(stmt)
        result["reset_returncode"] = reset_rc
        resets[surface] = reset_rc if result.get("expected_error_observed") else 0
        db.topk_expected_resets = resets
        if work_started:
            try:
                result["host_sqlite_work"] = db.stop_work()
            except BaseException as failure:
                result["trace_error"] = repr(failure)
                save(destination, result)
                raise
        save(destination, result)
        if error is None:
            db.check(reset_rc)
        else:
            require(reset_rc in (0, result.get("last_step_returncode")), "expected error cleanup returned a different error")
    return result


def check_profile(sample, ordered, consumed=277):
    summary = sample["summary"]
    operators = summary["profile"]["operators"]
    require(sum(op["dbHits"] for op in operators) == summary["metrics"]["dbHits"], "PROFILE dbHits sum differs")
    topk = [op for op in operators if op["operator"] == "OrderedTopK"]
    sorts = [op for op in operators if op["operator"] == "ExternalSort"]
    if ordered:
        indexes = [op for op in operators if op["operator"] == "IndexSeek"]
        require(len(topk) == len(indexes) == 1 and not sorts, "actual ordered numeric path required")
        require(topk[0]["rows"] == indexes[0]["rows"] == consumed, "TopK stopped before/after skip+limit")
    else:
        require(not topk and len(sorts) == 1, "fallback must execute the original Sort")


def setup(db, output):
    operations = []
    queries = ["UNWIND range(0,999) AS i CREATE (:S6Probe:S6Visible {k:999-i}) FINISH",
               "CREATE CONSTRAINT s6_probe_k_type FOR (n:S6Probe) REQUIRE n.k IS :: INTEGER",
               "CREATE CONSTRAINT s6_probe_k_key FOR (n:S6Probe) REQUIRE n.k IS NODE KEY"]
    save(output / "init.json", {"sql": "SELECT lithograph_init()", "rows": db.sql("SELECT lithograph_init()")})
    for index, query in enumerate(queries):
        save(output / f"setup-next-{index}.json", {"query": query})
        result = db.execute(query, options={"memoryLimitBytes": MEMORY}, surface="rows")
        operations.append({"query": query, "result": result})
        save(output / f"setup-result-{index}.json", operations[-1])
    pin = operations[-1]["result"]["summary"]["commit"]
    require(isinstance(pin, str) and pin.startswith("commit/"), "public DDL did not publish a pin")
    return pin


def schema(db, options, output):
    queries = {
        "constraints": "SHOW CONSTRAINTS YIELD name,type,entityType,labelsOrTypes,properties,ownedIndex,propertyType "
                       "RETURN name,type,entityType,labelsOrTypes,properties,ownedIndex,propertyType ORDER BY name",
        "indexes": "SHOW INDEXES YIELD name,type,entityType,labelsOrTypes,properties,owningConstraint "
                   "RETURN name,type,entityType,labelsOrTypes,properties,owningConstraint ORDER BY name",
    }
    catalog = {name: db.execute(query, options=options) for name, query in queries.items()}
    save(output / "public-schema.json", catalog)
    constraints, indexes = catalog["constraints"]["rows"], catalog["indexes"]["rows"]
    require(["s6_probe_k_type", "NODE_PROPERTY_TYPE", "NODE", ["S6Probe"], ["k"], None, "INTEGER"] in constraints,
            "public INTEGER constraint differs")
    require(["s6_probe_k_key", "NODE_KEY", "NODE", ["S6Probe"], ["k"], "s6_probe_k_key", None] in constraints,
            "public single-property key/exists constraint differs")
    require(["s6_probe_k_key", "RANGE", "NODE", ["S6Probe"], ["k"], "s6_probe_k_key"] in indexes,
            "public key Range backing differs")


def smoke_fallbacks(db, options, output):
    cases = [("descending", QUERY.replace("ASC", "DESC"), options,
              [[value] for value in range(742, 722, -1)]),
             ("view", QUERY, {**options, "graphView": {"requireAllLabels": ["S6Probe"]}}, EXPECTED),
             ("hidden", QUERY, {**options, "readAccess": {"query": {"denyProperties": ["k"]}}}, [])]
    for name, query, selected, expected in cases:
        sample = capture(db, "PROFILE " + query, selected, output / f"fallback-{name}.json", expected)
        check_profile(sample, False)
    db.sql("PRAGMA query_only=ON")
    try:
        sample = capture(db, "PROFILE " + QUERY, options, output / "fallback-query-only.json")
        check_profile(sample, False)
    finally:
        db.sql("PRAGMA query_only=OFF")
    alias = QUERY.replace("RETURN n.k ORDER", "RETURN n.k AS n ORDER")
    capture(db, alias, options, output / "fallback-alias-error.json", error="LITHOGRAPH_TYPE_ERROR")
    capture(db, QUERY, options, output / "same-handle-after-error.json")


def runtime_metadata(db, args, sqlite, path):
    version, source_id = db.sql("SELECT sqlite_version(),sqlite_source_id()")[0]
    require(not args.sqlite_source_id or args.sqlite_source_id == source_id, "SQLite SourceID differs")
    return {"version": version, "source_id": source_id, "library": sqlite.path,
            "compile_options": [row[0] for row in db.sql("PRAGMA compile_options")],
            "python": sys.version, "platform": platform.platform(),
            "cli_or_headers_used": False, "fixture": str(path), "probe_nodes": args.nodes,
            "source_membership_guard": "required externally; not claimed by this runner",
            "file_fixture_sqlite_open_readonly": bool(args.database),
            "extension_sha256": file_hash(args.extension), "sqlite_library_sha256": file_hash(sqlite.path)}


@contextmanager
def readonly_connection(args, sqlite, directory):
    """Close every warm/fresh/diagnostic connection, preserving a primary failure."""
    directory.mkdir()
    db, primary = None, None
    cleanup = {"opened": False, "closed": False}
    try:
        db = sqlite.connect(args.extension, args.database.resolve(), True)
        cleanup["opened"] = True
        db.sql("BEGIN DEFERRED")
        yield db
    except BaseException as failure:
        primary = failure
        save(directory / "failure.json", {"error": repr(failure), "no_retry": True})
        raise
    finally:
        try:
            if db is not None:
                if not db.api.sqlite3_get_autocommit(db.ptr):
                    db.sql("ROLLBACK")
                cleanup["autocommit_before_close"] = bool(db.api.sqlite3_get_autocommit(db.ptr))
                require(cleanup["autocommit_before_close"], "read transaction did not close")
                db.close()
                cleanup["closed"] = True
        except BaseException as failure:
            cleanup["error"] = repr(failure)
            if primary is None:
                raise
        finally:
            save(directory / "cleanup.json", cleanup)


def short_sampling(args, sqlite):
    """The existing 5x20 +100fresh contract, with ordered typed TopK oracles."""
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    options = {"at": args.at, "memoryLimitBytes": MEMORY}
    before_stat = args.database.stat()
    with readonly_connection(args, sqlite, output / "runtime") as db:
        environment = runtime_metadata(db, args, sqlite, args.database.resolve())
        save(output / "runtime.json", environment)
    results = {}

    def record(db, path):
        sample = capture(db, QUERY, options, path, surface=surface)
        return {key: sample[key] for key in ("first_data_us", "first_data_cpu_us", "full_us", "full_cpu_us")} | {
            "raw": str(path.relative_to(output))}

    try:
        for surface in ("scalar", "rows"):
            root = output / surface
            root.mkdir()
            first, warm, fresh = [], [], []
            if args.before:
                with readonly_connection(args, sqlite, root / "ordinary") as db:
                    first.append(record(db, root / "ordinary" / "sample.json"))
            else:
                for connection in range(5):
                    directory = root / f"warm-{connection}"
                    with readonly_connection(args, sqlite, directory) as db:
                        handles = [db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value]
                        first.append(record(db, directory / "first.json"))
                        for iteration in range(20):
                            warm.append(record(db, directory / f"sample-{iteration}.json"))
                        require(handles == [db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value], "warm handles changed")
                for connection in range(100):
                    directory = root / f"fresh-{connection}"
                    with readonly_connection(args, sqlite, directory) as db:
                        fresh.append(record(db, directory / "sample.json"))
            diagnostic = root / "diagnostic"
            with readonly_connection(args, sqlite, diagnostic) as db:
                vm = capture(db, QUERY, options, diagnostic / "fresh-vm.json", traced=True, surface=surface)
                warm_vm = profile = None
                if not args.before:
                    warm_vm = capture(db, QUERY, options, diagnostic / "warm-vm.json", traced=True, surface=surface)
                    profile = capture(db, "PROFILE " + QUERY, options, diagnostic / "profile.json", surface=surface)
                    check_profile(profile, True)
                plan = db.execute("EXPLAIN " + QUERY, PARAMS, options)
                save(diagnostic / "plan.json", plan)
                text = plan["rows"][0][0]
                require(("OrderedTopK" in text) == (not args.before), "wrong artifact physical path")
                require(("ExternalSort" in text) == args.before, "unexpected retained/removed Sort")
                schema(db, options, diagnostic)
            results[surface] = {"connection_first": first, "warm_raw": warm, "fresh_raw": fresh,
                                "first_summary": timing_summary(first), "warm_summary": timing_summary(warm),
                                "fresh_summary": timing_summary(fresh), "fresh_vm": vm, "warm_vm": warm_vm,
                                "profile_sample": profile, "plan": text,
                                "complete_primary_executions": 2 if args.before else 208}
        after = {"extension_sha256": file_hash(args.extension), "sqlite_library_sha256": file_hash(sqlite.path)}
        save(output / "artifacts-after.json", after)
        require(all(environment[key] == value for key, value in after.items()), "artifact path drifted")
        current = args.database.stat()
        require((before_stat.st_size, before_stat.st_mtime_ns) == (current.st_size, current.st_mtime_ns),
                "read-only fixture main size/mtime changed")
        report = {"status": "passed", "before": args.before, "runtime": environment, "pin": args.at,
                  "query": QUERY, "fixed_skip": 257, "fixed_limit": 20, "oracle": EXPECTED,
                  "surfaces": results, "sampling": ("one ordinary +one independent exact VM per surface; no P95"
                    if args.before else "5 connection-first +5x20warm +100fresh untraced per surface; VM/PROFILE separate"),
                  "vm_scope": "exact host connection nested stmt_status; private SortSpill excluded",
                  "os_page_cache_coldness": "unknown; fresh means a new SQLite connection",
                  "process_peak_rss_bytes": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024}
        save(output / "report.json", report)
        print(json.dumps({"status": "passed", "output": str(output), "before": args.before,
                          "primary_executions_per_surface": 2 if args.before else 208}), flush=True)
    except BaseException as failure:
        save(output / "failure.json", {"error": repr(failure), "no_retry": True})
        raise


def run(args, sqlite):
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    path = str(args.database.resolve()) if args.database else ":memory:"
    fixture_before = None if not args.database else args.database.stat()
    db = None
    failed = False
    try:
        db = sqlite.connect(args.extension, path, readonly=bool(args.database))
        environment = runtime_metadata(db, args, sqlite, path)
        save(output / "runtime.json", environment)
        pin = args.at if args.database else setup(db, output)
        options = {"at": pin, "memoryLimitBytes": MEMORY}
        handles = [db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value]
        db.sql("BEGIN DEFERRED")
        samples = {"topk": [], "sort_oracle": []}
        for name, query in (("topk", QUERY), ("sort_oracle", ORACLE)):
            for index in range(args.samples):
                samples[name].append(capture(db, query, options, output / f"{name}-timing-{index}.json"))
            diagnostic = capture(db, "PROFILE " + query, options, output / f"{name}-profile.json")
            check_profile(diagnostic, name == "topk" and not args.before)
            capture(db, query, options, output / f"{name}-vm.json", traced=True)
        scalar = db.execute(QUERY, PARAMS, options)
        save(output / "scalar.json", scalar)
        require(scalar["columns"] == ["n.k"], "scalar columns differ")
        typed_rows(scalar["rows"], EXPECTED)
        plan = db.execute("EXPLAIN " + QUERY, PARAMS, options)
        save(output / "plan.json", plan)
        require(("OrderedTopK" in plan["rows"][0][0]) == (not args.before), "EXPLAIN disagrees with requested artifact")
        if not args.database:
            smoke_fallbacks(db, options, output)
        schema(db, options, output)
        require(handles == [db.scalar_stmt.ptr.value, db.rows_stmt.ptr.value], "retained statement handles changed")
        db.sql("ROLLBACK")
        require(db.api.sqlite3_get_autocommit(db.ptr), "read scope leaked a transaction")
        after = {"extension_sha256": file_hash(args.extension), "sqlite_library_sha256": file_hash(sqlite.path)}
        save(output / "artifacts-after.json", after)
        require(all(environment[key] == value for key, value in after.items()), "loaded artifact path drifted")
        if fixture_before is not None:
            current = args.database.stat()
            require((fixture_before.st_size, fixture_before.st_mtime_ns) == (current.st_size, current.st_mtime_ns),
                    "read-only fixture main file size/mtime changed")
        summary = {"status": "passed", "before": args.before, "pin": pin, "runtime": environment,
                   "fixed_skip": 257, "fixed_limit": 20, "complete_ordered_oracle": EXPECTED,
                   "timing_samples": samples, "latency_p95": None,
                   "sampling": "bounded untraced samples; PROFILE/VM separate; not a P95/SLA acceptance",
                   "vm_scope": "exact stock stmt_status totals on this connection, including nested engine SQL; private SortSpill excluded",
                   "same_retained_rows_and_scalar_handles": True,
                   "process_peak_rss_bytes": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024}
    except BaseException as failure:
        failed = True
        save(output / "failure.json", {"error": repr(failure), "no_retry": True})
        raise
    finally:
        cleanup = {"connection_opened": db is not None}
        try:
            if db is not None:
                cleanup["autocommit_before_close"] = bool(db.api.sqlite3_get_autocommit(db.ptr))
                if not cleanup["autocommit_before_close"]:
                    db.sql("ROLLBACK")
                db.close()
                cleanup["closed"] = True
        except BaseException as failure:
            cleanup["error"] = repr(failure)
            if not failed:
                raise
        finally:
            save(output / "cleanup.json", cleanup)
    save(output / "report.json", summary)
    print(json.dumps({"status": "passed", "output": str(output), "pin": pin,
                      "samples_per_path": args.samples, "probe_nodes": args.nodes}), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("extension", type=Path)
    parser.add_argument("--sqlite-library", required=True)
    parser.add_argument("--sqlite-source-id")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--database", type=Path)
    parser.add_argument("--nodes", type=int, choices=[1000, 10000, 100000, 1000000], default=1000)
    parser.add_argument("--at")
    parser.add_argument("--samples", type=int, choices=range(1, 21), default=1)
    parser.add_argument("--before", action="store_true", help="require original Sort for a native Before artifact")
    parser.add_argument("--short", action="store_true", help="fixed After 5x20warm+100fresh; Before ordinary1+VM1")
    args = parser.parse_args()
    require(platform.system() == "Linux", "execute only on an authorized native Linux target; no Mac/VM execution")
    require(bool(args.database) == bool(args.at), "--database and immutable --at pin must be supplied together")
    require(args.database or args.nodes == 1000, "standalone public smoke is bounded to 1k")
    require(not args.database or args.database.is_file(), "existing shared fixture required")
    require(not args.short or (args.database and args.samples == 1), "--short requires a shared fixture and fixed sampling")
    if args.at:
        require(args.at.startswith("commit/") and len(args.at) == 71
                and all(char in "0123456789abcdef" for char in args.at[7:]), "canonical immutable Commit pin required")
    sqlite = select_sqlite(args.sqlite_library)
    (short_sampling if args.short else run)(args, sqlite)


if __name__ == "__main__":
    main()
