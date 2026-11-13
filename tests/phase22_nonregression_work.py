#!/usr/bin/env python3
"""Paired Phase 22 write/version benchmarks on copies of one synthetic fixture.

No canonical internal table is modified directly. Each round starts with an
identical closed database, fully consumes outputs and verifies durable results.
Run outside CI: one full pair; repeat twice only upon regression.
The 10K conflicts, 40 resolution rounds and performance thresholds are unchanged.
"""

import argparse
import json
from pathlib import Path
import platform
import shutil
import sqlite3
import statistics
import subprocess
import time

from phase21_predicate_routing_sql import execute, require
from phase22_access_work import file_hash as digest, open_db


def copy_fixture(source, destination):
    # The release fixture is tens of GiB. APFS copy-on-write retains separate
    # SQLite files while avoiding six full physical copies on the M2 host.
    if platform.system() == "Darwin":
        subprocess.run(["/bin/cp", "-c", str(source), str(destination)], check=True)
    else:
        shutil.copyfile(source, destination)


def timed(operation):
    started = time.perf_counter_ns()
    value = operation()
    return value, (time.perf_counter_ns() - started) / 1000


def history(db):
    cursor = None
    commits = []
    while True:
        args = "'branch/main',64" + (",$cursor" if cursor else "")
        result = execute(db, "CALL lithograph.log(" + args +
                         ") YIELD commit,cursor RETURN commit,cursor", {"cursor": cursor})
        batch = result["rows"]
        require(len(batch) <= 64, "history page exceeded limit")
        commits.extend(row[0] for row in batch)
        if not batch or batch[-1][1] is None:
            break
        cursor = batch[-1][1]
    require(len(commits) >= 257 and len(set(commits)) == len(commits), "history completeness")
    return len(commits)


def sql_round(extension, fixture, destination):
    require(not destination.exists(), "SQL round path must be new")
    copy_fixture(fixture, destination)
    db = open_db(extension, str(destination))
    environment = {key: db.execute("PRAGMA " + key).fetchone()[0]
                   for key in ("journal_mode", "synchronous", "cache_size", "temp_store")}
    # Equal untimed history construction gives both versions a real paged DAG.
    for _ in range(256):
        execute(db, "CALL lithograph.commit.create() YIELD commit RETURN commit")
    before = execute(db, "RETURN 1")["summary"]["commit"]
    result, write_us = timed(lambda: execute(
        db, "UNWIND range(1,1000) AS i CREATE (:NonRegression {k:i}) FINISH"))
    require(result["summary"]["counters"]["nodesCreated"] == 1000, "batch cardinality")
    after = result["summary"]["commit"]
    require(before != after, "batch did not publish a commit")
    history_rows, history_us = timed(lambda: history(db))
    result, diff_us = timed(lambda: execute(
        db, "CALL lithograph.diff($before,$after) YIELD patch RETURN patch",
        {"before": before, "after": after}))
    operations = result["rows"][0][0]["operations"]
    require(len(operations) >= 1000, "diff did not consume complete patch")
    require(execute(db, "MATCH (n:NonRegression) RETURN count(n)")["rows"] == [[1000]],
            "batch durable graph differs")
    commit_count = db.execute("SELECT count(*) FROM main._lithograph_commits").fetchone()[0]
    started = time.perf_counter_ns()
    begin = json.loads(db.execute("SELECT lithograph_tx_begin('{}')").fetchone()[0])
    require(begin["baseCommit"] == after, "explicit transaction pinned wrong head")
    for offset in (0, 500):
        result = execute(db, "UNWIND range($start,$end) AS i CREATE (:ExplicitNonRegression {k:i}) FINISH",
                         {"start": offset, "end": offset + 499})
        require(result["summary"]["commit"] is None, "explicit statement committed early")
    require(execute(db, "MATCH (n:ExplicitNonRegression) RETURN count(n)")["rows"] == [[1000]],
            "explicit transaction did not observe prior staged writes")
    committed = json.loads(db.execute("SELECT lithograph_tx_commit()").fetchone()[0])
    explicit_us = (time.perf_counter_ns() - started) / 1000
    require(committed["commit"] != after and not db.in_transaction, "explicit commit failed")
    require(db.execute("SELECT count(*) FROM main._lithograph_commits").fetchone()[0] == commit_count + 1,
            "explicit transaction did not produce exactly one commit")
    require(db.execute("PRAGMA quick_check").fetchone() == ("ok",), "SQL round quick_check")
    db.close()
    db = open_db(extension, str(destination), True)
    require(execute(db, "MATCH (n:ExplicitNonRegression) RETURN count(n)")["rows"] == [[1000]],
            "reopened explicit result differs")
    db.close()
    return {"sqlite": sqlite3.sqlite_version, "environment": environment,
            "write_batch_commit_us": write_us, "history_us": history_us,
            "diff_us": diff_us, "explicit_transaction_us": explicit_us,
            "history_rows": history_rows, "patch_operations": len(operations)}


def merge_round(tool, fixture, destination):
    require(not destination.exists(), "merge round path must be new")
    copy_fixture(fixture, destination)
    output = subprocess.check_output([str(tool.resolve()), str(destination.resolve())], text=True)
    report = json.loads(output)
    require(report["conflict_count"] == 10000 and report["resolution_rounds"] == 40,
            "merge was not the required 10K conflict/40-round workload")
    require(report["gc_root_preserved"], "GC lost merge root")
    return report


def compare(samples, keys):
    result = {}
    for key in keys:
        before = statistics.median(sample["before"][key] for sample in samples)
        after = statistics.median(sample["after"][key] for sample in samples)
        # All callers normalize measurements to microseconds.
        threshold = max(1.2 * before, before + 10000)
        result[key] = {"before_us": before, "after_us": after,
                       "statistic": "single" if len(samples) == 1 else "median",
                       "sample_count": len(samples),
                       "threshold_us": threshold, "pass": after <= threshold}
    return result


def gate_results(samples, merges):
    result = {"sql_gates": compare(samples, ("write_batch_commit_us", "history_us", "diff_us",
                                            "explicit_transaction_us"))}
    if merges:
        keys = ("merge_start_millis", "conflict_scan_millis", "resolution_millis",
                "candidate_inspection_millis", "finalize_millis", "gc_root_millis")
        micros = [{name: {key: value * (1000 if key.endswith("_millis") else 1)
                         for key, value in pair[name].items() if isinstance(value, (int, float))}
                   for name in ("before", "after")} for pair in merges]
        result["merge_gates"] = compare(micros, keys + ("finalize_prepare_micros",
                                                       "finalize_writer_wait_micros",
                                                       "finalize_writer_hold_micros"))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("fixture", type=Path)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--merge-before", type=Path)
    parser.add_argument("--merge-after", type=Path)
    args = parser.parse_args()
    require(not args.destination.exists(), "benchmark directory must be new")
    require(bool(args.merge_before) == bool(args.merge_after), "both merge tools are required")
    args.destination.mkdir(parents=True)
    original = digest(args.fixture)
    samples, merges = [], []
    for index in range(3):
        paired, paired_merge = {}, {}
        # Alternate order rather than always giving one version the warm cache.
        for name in (("before", "after") if index % 2 == 0 else ("after", "before")):
            extension = getattr(args, name)
            paired[name] = sql_round(extension, args.fixture, args.destination / f"sql-{index}-{name}.sqlite3")
            print(json.dumps({"round": index, "version": name, "sql": paired[name]}), flush=True)
            if args.merge_before:
                tool = args.merge_before if name == "before" else args.merge_after
                paired_merge[name] = merge_round(tool, args.fixture,
                                                args.destination / f"merge-{index}-{name}.sqlite3")
                print(json.dumps({"round": index, "version": name, "merge": paired_merge[name]}), flush=True)
        samples.append(paired)
        if paired_merge:
            merges.append(paired_merge)
        if index == 0:
            first_pair_gates = gate_results(samples, merges)
            if all(gate["pass"] for group in first_pair_gates.values() for gate in group.values()):
                break
    require(original == digest(args.fixture), "benchmark modified source fixture")
    report = {"fixture_sha256": original, "before_sha256": digest(args.before),
              "after_sha256": digest(args.after), "samples": samples, "merge_samples": merges,
              "paired_rounds": len(samples),
              "sampling_policy": "one complete pair; retain it and add two pairs only upon regression",
              "statistic": "single" if len(samples) == 1 else "median",
              "first_pair_gates": first_pair_gates,
              **gate_results(samples, merges)}
    (args.destination / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report), flush=True)
    require(all(gate["pass"] for group in ("sql_gates", "merge_gates")
                for gate in report.get(group, {}).values()), "non-regression threshold exceeded")


if __name__ == "__main__":
    main()
