#!/usr/bin/env python3
"""S6 finite-ID SIP on published, immutable shared fixtures.

Run on the authorized native Linux host, never seed/rebuild an input. All paths,
artifacts and the actual stock-SQLite SourceID are explicit CLI inputs. Reuse
the retained C-API statements, complete typed bag consumer, exact stmt_status
accounting and uninstrumented 5x20+100 sampling from the existing S4/S5 tools.
One command measures one artifact; compare reads saved reports without queries.
"""

import argparse
import ctypes as c
import json
from pathlib import Path
import platform
import sys

from phase22_hash_work import actual_sql_runtime, short_sampling
from phase22_join_work import checkpoint_descriptor, closed, measured, new_path, sidecars
from phase22_relationship_work import file_hash, require, select_sqlite


QUERIES = {
    "node": "MATCH(b:S6Chosen) WITH collect(b) AS bs "
            "MATCH(a:S6Probe) WHERE a IN bs RETURN elementId(a)",
    "element-id": "MATCH(b:S6Chosen) WITH collect(elementId(b)) AS ids "
                  "MATCH(a:S6Probe) WHERE elementId(a) IN ids RETURN elementId(a)",
}
ORACLE = [["n:101"], ["n:501"], ["n:901"]]


def save(path, value):
    with Path(path).open("x") as output:
        json.dump(value, output, indent=2, sort_keys=True)
        output.write("\n")


def fixture(manifest_path, database):
    manifest = json.loads(manifest_path.read_text())
    nodes = manifest.get("probe_nodes", manifest.get("nodes", 0) - 1024)
    require(nodes in (1000, 10000, 100000, 1000000), "unsupported shared fixture scale")
    require(manifest.get("total_nodes", manifest.get("nodes")) == nodes + 1024,
            "shared fixture must contain N Probe plus 1024 Build Nodes")
    pin = manifest.get("pin") or manifest.get("options", {}).get("at")
    pin = checkpoint_descriptor(pin or "")
    ready = manifest.get("ready", manifest.get("ready_generations", {}))
    for name, count in (("s6_probe_k_key", nodes), ("s6_build_k_range", 1024)):
        require(ready.get(name, [])[-4:] == [pin.removeprefix("commit/"), 1, count, count],
                "published ready generation does not match the fixed pin/counts")
    # Both accepted publications use the identical public ID/value recipe.
    if "kind" in manifest:
        require(manifest["kind"] == "S6-shared-ready-recipe-v1"
                and manifest["oracles"]["sip"] == ORACLE, "shared recipe/oracle differs")
    else:
        require(str(manifest.get("status", "")).startswith("passed"), "bulk setup did not pass")
        check = manifest.get("data_check")
        if check is not None:
            require(check["complete_typed_identity_value_labels_oracle"]
                    and check["all_node_ids_seen_once"] and check["rows_checked"] == nodes + 1024
                    and check["labels"]["S6Chosen"] == 3, "bulk full checker did not pass")
    digest = file_hash(database)
    require(digest == manifest["fixture_sha256"], "database differs from its published fixture")
    return {"manifest": str(manifest_path.resolve()), "manifest_sha256": file_hash(manifest_path),
            "database": str(database.resolve()), "probe_nodes": nodes, "pin": pin,
            "fixture_sha256": digest, "sidecars": closed(database)}


def verify_profile(sample, role):
    require(sample["row_count"] == 3, "complete row-event count differs from the typed oracle")
    operators = sample["profile"]["operators"]
    require(sum(op["dbHits"] for op in operators) == sample["metrics"]["dbHits"],
            "PROFILE dbHits sum differs from summary")
    masks = [op for op in operators if "semiMask" in op]
    if role == "before":
        require(not masks, "Before artifact unexpectedly applied SIP")
    else:
        require(sample["metrics"]["rows"] == 3, "After summary must count public output once")
        require(len(masks) == 1, "After must actually apply one complete finite-ID mask")
        mask = masks[0]
        require(mask["semiMask"] == {"keys": 3, "seeks": 3} and mask["rows"] == 3,
                "actual finite candidates/point reads/rows differ")
        require(operators[-1]["operator"] == "Project" and operators[-1]["rows"] == 3,
                "final Project must produce three public rows once")


def smoke(args, sqlite, database, options, case, prefix):
    samples = {}
    for surface in ("scalar", "rows"):
        normal = sqlite.connect(args.extension, database, True)
        try:
            ordinary = measured(normal, case, options, surface)
            save(prefix.with_name(prefix.name + f".{surface}.ordinary.json"), ordinary)
        except BaseException as error:
            save(prefix.with_name(prefix.name + f".{surface}.ordinary.error.json"),
                 {"error": repr(error)})
            raise
        finally:
            normal.close()
        diagnostic = sqlite.connect(args.extension, database, True)
        try:
            profiled = measured(diagnostic, case, options, surface, traced=True, profile=True)
            save(prefix.with_name(prefix.name + f".{surface}.profile-vm.json"), profiled)
            plan = diagnostic.execute("EXPLAIN " + case["query"], options=options)["rows"][0][0]
        except BaseException as error:
            save(prefix.with_name(prefix.name + f".{surface}.diagnostic.error.json"),
                 {"error": repr(error)})
            raise
        finally:
            diagnostic.close()
        samples[surface] = {"ordinary": ordinary, "profile_vm": profiled, "plan": plan}
    return {"sampling": "one ordinary + one independent PROFILE/VM per surface; no P50/P95",
            "surfaces": samples}


def output_counts(samples, role):
    defects = []
    for surface, records in samples["surfaces"].items():
        if "ordinary" in records:
            values = [("ordinary", records["ordinary"]), ("profile_vm", records["profile_vm"])]
        else:
            values = [("first", value) for value in records["connection_first"]]
            values += [("warm", value) for group in records["warm_raw"] for value in group]
            values += [("fresh", value) for value in records["fresh_raw"]]
            values += [(name, records[name]) for name in ("fresh_vm", "warm_vm", "profile_sample")]
        for name, sample in values:
            require(sample["row_count"] == 3, "complete public row count differs")
            if sample["metrics"]["rows"] != 3:
                defects.append({"surface": surface, "role": name, "public_rows": 3,
                                "summary_rows": sample["metrics"]["rows"]})
    require(role == "before" or not defects, "After summary counted output more than once")
    return defects


def measure_fixture(args, sqlite, manifest_path, database):
    context = fixture(manifest_path, database)
    options = {"at": context["pin"]}
    if args.memory_limit_bytes is not None:
        options["memoryLimitBytes"] = args.memory_limit_bytes
    case = {"name": args.case, "query": QUERIES[args.case], "oracle": ORACLE}
    prefix = args.output / f"{context['probe_nodes']}-{args.case}"
    save(prefix.with_suffix(".context.json"), {"context": context, "case": case,
        "options": options, "role": args.role, "runtime": args.runtime})
    try:
        if args.command == "smoke":
            samples = smoke(args, sqlite, database, options, case, prefix)
        else:
            samples = short_sampling(args, sqlite, database, options, case,
                                     prefix.with_suffix(".raw.jsonl"))
        save(prefix.with_suffix(".observed.json"), samples)
        for surface in samples["surfaces"].values():
            verify_profile(surface.get("profile_sample", surface.get("profile_vm")), args.role)
        defects = output_counts(samples, args.role)
        report = {"status": "completed_before_with_known_metrics_defect" if defects else "passed",
                  "baseline_metrics_defects": defects, "role": args.role, "case": args.case,
                  "context": context, "query": case["query"], "params": {}, "options": options,
                  "runtime": args.runtime,
                  "complete_typed_bag_oracle": ORACLE, "samples": samples}
    except BaseException as error:
        save(prefix.with_suffix(".failure.json"), {"error": repr(error)})
        raise
    finally:
        unchanged = (file_hash(database) == context["fixture_sha256"]
                     and sidecars(database) == context["sidecars"]
                     and file_hash(manifest_path) == context["manifest_sha256"])
        save(prefix.with_suffix(".postflight.json"), {"unchanged": unchanged})
        require(unchanged, "immutable fixture/manifest/sidecars changed")
    save(prefix.with_suffix(".json"), report)
    return report


def physical_samples(report):
    for surface, samples in report["samples"]["surfaces"].items():
        roles = (("profile_vm", samples["profile_vm"]),) if "profile_vm" in samples else (
            ("fresh_vm", samples["fresh_vm"]), ("warm_vm", samples["warm_vm"]))
        for role, sample in roles:
            yield surface, role, sample


def growth_checks(reports):
    groups, checks = {}, []
    for report in reports:
        for surface, role, sample in physical_samples(report):
            groups.setdefault((surface, role), []).append(
                (report["context"]["probe_nodes"], sample["sqlite_work"]["vm_steps"]))
    for key, values in groups.items():
        values.sort()
        for (small, low), (large, high) in zip(values, values[1:]):
            if large == 10 * small:
                require(high <= 2 * low + 1000, f"fixed-result 10x VM growth failed: {key}: {values}")
                checks.append({"surface": key[0], "role": key[1], "scales": [small, large],
                               "work": [low, high], "limit": 2 * low + 1000})
    return checks


def completed_report(path):
    report = json.loads(path.read_text())
    postflight = json.loads(path.with_suffix(".postflight.json").read_text())
    parent = json.loads((path.parent / "summary.json").read_text())
    require(postflight["unchanged"] and parent["status"] in (
        "passed", "completed_before_with_known_metrics_defect"), "measurement did not finish its guards")
    require(report["role"] == parent["role"] and report["runtime"] == parent["sqlite"]
            and report["context"] in parent["reports"], "report is not from this completed run")
    return report


def compare(args):
    before, after = (completed_report(path) for path in (args.before, args.after))
    require(before["runtime"] == after["runtime"], "Before/After actual SQLite runtime differs")
    for key in ("case", "query", "params", "options", "complete_typed_bag_oracle"):
        require(before[key] == after[key], "Before/After query/options/oracle differs")
    for key in ("fixture_sha256", "pin", "probe_nodes", "manifest_sha256"):
        require(before["context"][key] == after["context"][key], "Before/After fixture differs")
    require(before["role"] == "before" and after["role"] == "after", "wrong pair roles")
    defects = output_counts(before["samples"], "before")
    output_counts(after["samples"], "after")
    old = {(surface, role): sample for surface, role, sample in physical_samples(before)}
    checks = []
    for surface, role, sample in physical_samples(after):
        base = old[(surface, role)]
        require(sample["row_count"] == base["row_count"] == 3, "paired complete rows differ")
        low, high = sample["sqlite_work"], base["sqlite_work"]
        require(low["vm_steps"] < high["vm_steps"], "SIP did not reduce actual host SQLite VM")
        checks.append({"surface": surface, "role": role, "before": high, "after": low})
    save(args.output, {"status": "passed", "same_fixed_complete_output": True,
        "before_metrics_contract": "failed_known_baseline_defect" if defects else "passed",
        "baseline_metrics_defects": defects, "physical_VM_reduction": checks,
        "latency_claim": "none; read normal raw separately"})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("smoke", "measure"):
        command = sub.add_parser(name)
        command.add_argument("extension", type=Path)
        command.add_argument("--fixture", nargs=2, action="append", required=True,
                             metavar=("MANIFEST", "DATABASE"))
        command.add_argument("--sqlite-library", required=True)
        command.add_argument("--sqlite-source-id", required=True)
        command.add_argument("--role", choices=("before", "after"), default="after")
        command.add_argument("--case", choices=QUERIES, default="node")
        command.add_argument("--memory-limit-bytes", type=int)
        command.add_argument("--output", type=Path, required=True)
    paired = sub.add_parser("compare")
    paired.add_argument("--before", type=Path, required=True)
    paired.add_argument("--after", type=Path, required=True)
    paired.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "compare":
        compare(args)
        return
    require(platform.system() == "Linux", "execution is restricted to the authorized native Linux host")
    require(args.memory_limit_bytes is None or args.memory_limit_bytes > 0, "quota must be positive")
    new_path(args.output)
    args.output.mkdir(parents=True)
    tool_sha, artifact_sha = file_hash(Path(__file__)), file_hash(args.extension)
    save(args.output / "invocation.json", {"argv": sys.argv, "tool_sha256": tool_sha,
        "artifact_sha256": artifact_sha, "role": args.role})
    sqlite = select_sqlite(args.sqlite_library)
    sqlite.lib.sqlite3_sourceid.argtypes, sqlite.lib.sqlite3_sourceid.restype = [], c.c_char_p
    require(sqlite.lib.sqlite3_sourceid().decode() == args.sqlite_source_id,
            "actual SQLite SourceID differs before extension load")
    runtime = actual_sql_runtime(sqlite, args.extension)
    require(runtime["source_id"] == args.sqlite_source_id, "loaded SQLite SourceID differs")
    args.runtime = runtime
    reports = []
    for manifest, database in args.fixture:
        reports.append(measure_fixture(args, sqlite, Path(manifest), Path(database)))
    require(file_hash(Path(__file__)) == tool_sha and file_hash(args.extension) == artifact_sha,
            "tool/artifact changed during measurement")
    defects = [item for report in reports for item in report["baseline_metrics_defects"]]
    save(args.output / "summary.json", {
        "status": "completed_before_with_known_metrics_defect" if defects else "passed",
        "baseline_metrics_defects": defects, "command": args.command,
        "role": args.role, "tool_sha256": tool_sha, "artifact_sha256": artifact_sha,
        "sqlite": runtime, "argv": sys.argv, "reports": [r["context"] for r in reports],
        "fixed_result_10x_VM_checks": growth_checks(reports) if args.role == "after" else [],
        "VM_scope": "host connection exact stmt_status; not private SQLite VM or a dbHits proxy",
        "source_membership": "external command guard required; not claimed by this tool",
        "os_cache": "fixture SHA read before/after; not OS cold"})


if __name__ == "__main__":
    main()
