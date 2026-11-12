#!/usr/bin/env python3
"""Public SQL resident Dynamic Filter measurements on published shared fixtures.

Native Linux only. No seed, rebuild, private-table access or algorithm hint.
Smoke keeps ordinary and PROFILE/VM on independent connections; measure uses
the existing untraced 5x20 warm +100 first-query/fresh sampling. Compare is
offline and requires successful immutable-input/tool/artifact final guards.
"""

import argparse
import ctypes as c
import json
from pathlib import Path
import platform
import sys

import phase22_sip_sql as sip
from phase22_hash_work import actual_sql_runtime, short_sampling
from phase22_join_work import check_rows, checkpoint_descriptor, closed, new_path, sidecars
from phase22_relationship_work import file_hash, require, select_sqlite


QUERY = ("MATCH(p:S6Probe),(b:S6ResidentBuild) WHERE p.k=b.k "
         "RETURN elementId(p),elementId(b),p.k,b.k")
COLUMNS = ["elementId(p)", "elementId(b)", "p.k", "b.k"]
PROBE_IDS = {0: "n:101", 1: "n:501", 2: "n:901"}


def oracle(manifest):
    source = manifest["oracles"]
    require(source["query"] == QUERY and source["columns"] == COLUMNS,
            "resident query/columns differ from the fixed public recipe")
    created = source["create_rows"]
    require(len(created) == 64, "expected the complete independent CREATE result")
    seen_ids, seen_seq, expected = set(), set(), []
    for identity, sequence, key, labels in created:
        require(isinstance(identity, str) and identity.startswith("n:")
                and identity[2:].isascii() and identity[2:].isdigit()
                and identity[2:3] != "0" and int(identity[2:]) <= 2**63 - 1,
                "noncanonical created NodeId")
        require(type(sequence) is int and 1 <= sequence <= 64
                and type(key) is int and key == sequence % 3, "created seq/key differs")
        require(sorted(labels) == ["S6ResidentBuild", "S6Visible"], "created labels differ")
        require(identity not in seen_ids and sequence not in seen_seq, "duplicate CREATE identity/seq")
        seen_ids.add(identity)
        seen_seq.add(sequence)
        expected.append([PROBE_IDS[key], identity, key, key])
    check_rows(source["rows"], expected)
    return {"name": "resident64", "query": QUERY, "oracle": expected}


def fixture(manifest_path, database):
    manifest = json.loads(manifest_path.read_text())
    require(manifest["kind"] == "s6-resident-df-v1", "unsupported resident fixture")
    nodes = manifest["probe_nodes"]
    require(nodes in (1000, 10000, 100000, 1000000)
            and manifest["resident_build_nodes"] == 64
            and manifest["base_total_nodes"] == nodes + 1024
            and manifest["total_nodes"] == nodes + 1088, "resident fixture dimensions differ")
    pin = checkpoint_descriptor(manifest["pin"])
    for name, count in (("s6_probe_k_key", nodes), ("s6_resident_build_k_range", 64)):
        require(manifest["ready"].get(name) == [pin.removeprefix("commit/"), 1, count, count],
                "resident ready generation differs from the final pin/count")
    case = oracle(manifest)
    digest = file_hash(database)
    require(digest == manifest["fixture_sha256"], "database differs from the published fixture")
    return ({"manifest": str(manifest_path.resolve()), "manifest_sha256": file_hash(manifest_path),
             "database": str(database.resolve()), "probe_nodes": nodes, "pin": pin,
             "fixture_sha256": digest, "sidecars": closed(database)}, case)


def output_counts(samples):
    for records in samples["surfaces"].values():
        if "ordinary" in records:
            values = [records["ordinary"], records["profile_vm"]]
        else:
            values = records["connection_first"] + records["fresh_raw"]
            values += [value for group in records["warm_raw"] for value in group]
            values += [records[name] for name in ("fresh_vm", "warm_vm", "profile_sample")]
        require(all(value["row_count"] == value["metrics"]["rows"] == 64 for value in values),
                "public/summary rows differ from the complete 64-row typed bag")


def verify_profile(sample, role):
    if sample["surface"] == "rows":
        require(sample["columns"] == COLUMNS, "public rows columns differ from the fixed projection")
    operators = sample["profile"]["operators"]
    require(sum(op["dbHits"] for op in operators) == sample["metrics"]["dbHits"], "PROFILE sum differs")
    if role == "before":
        require(any(op["operator"] == "IndexNestedLoop" and op["rows"] > 0 for op in operators),
                "R5 Before did not execute its ordinary Index Nested Loop")
        require(all("dynamicFilter" not in op.get("hash", {}) for op in operators), "Before applied DF")
        return
    hashes = [op for op in operators if op["operator"] == "HashJoin"]
    require(len(hashes) == 1 and hashes[0]["rows"] == 64, "After did not execute one actual Hash Join")
    hashed = hashes[0]["hash"]
    require(hashed["buildRows"] == 64 and hashed["probeRows"] == 4, "actual build/probe rows differ")
    require(all(hashed[name] == 0 for name in (
        "spillPartitions", "spillBatches", "spillVmSteps", "spillEncodedBytes", "tempAllocatedBytes")),
        "resident success must not silently become Grace spill")
    dynamic = hashed["dynamicFilter"]
    require(dynamic["applied"] is True, "no actual completed Dynamic Filter was applied")
    require(all(type(dynamic[name]) is int and dynamic[name] == value for name, value in {
        "buildRows": 64, "distinctKeys": 3, "prefixRowsRead": 1,
        "equalitySeeks": 3, "tailOwners": 3}.items()), "actual DF/keyed-tail counters differ")
    require(type(dynamic["keysetPeakBytes"]) is int and dynamic["keysetPeakBytes"] > 0,
            "missing actual keyset reservation peak")
    require(type(dynamic["collectorProbes"]) is int and dynamic["collectorProbes"] >= 0,
            "invalid collector work counter")


def preflight(args, sqlite, database, options, case, prefix):
    """Reject a wrong CBO producer before scheduling 205 ordinary invocations."""
    db = sqlite.connect(args.extension, database, True)
    try:
        sample = sip.measured(db, case, options, "rows", traced=True, profile=True)
        sip.save(prefix.with_suffix(".preflight-profile.json"), sample)
        verify_profile(sample, args.role)
    except BaseException as error:
        sip.save(prefix.with_suffix(".preflight-error.json"), {"error": repr(error)})
        raise
    finally:
        db.close()


def measure_fixture(args, sqlite, manifest_path, database):
    context, case = fixture(manifest_path, database)
    options = {"at": context["pin"]}
    if args.memory_limit_bytes is not None:
        options["memoryLimitBytes"] = args.memory_limit_bytes
    prefix = args.output / f"{context['probe_nodes']}-resident64"
    sip.save(prefix.with_suffix(".context.json"), {"context": context, "case": case,
        "options": options, "role": args.role, "runtime": args.runtime})
    try:
        if args.command == "smoke":
            samples = sip.smoke(args, sqlite, database, options, case, prefix)
        else:
            preflight(args, sqlite, database, options, case, prefix)
            samples = short_sampling(args, sqlite, database, options, case,
                                     prefix.with_suffix(".raw.jsonl"))
        sip.save(prefix.with_suffix(".observed.json"), samples)
        output_counts(samples)
        for records in samples["surfaces"].values():
            verify_profile(records.get("profile_sample", records.get("profile_vm")), args.role)
        report = {"status": "passed", "role": args.role, "case": "resident64", "context": context,
                  "runtime": args.runtime, "query": QUERY, "params": {}, "options": options,
                  "complete_typed_bag_oracle": case["oracle"], "samples": samples}
    except BaseException as error:
        sip.save(prefix.with_suffix(".failure.json"), {"error": repr(error)})
        raise
    finally:
        unchanged = (file_hash(database) == context["fixture_sha256"]
                     and sidecars(database) == context["sidecars"]
                     and file_hash(manifest_path) == context["manifest_sha256"])
        sip.save(prefix.with_suffix(".postflight.json"), {"unchanged": unchanged})
        require(unchanged, "immutable resident fixture/manifest/sidecars changed")
    sip.save(prefix.with_suffix(".json"), report)
    return report


def compare(args):
    before, after = (sip.completed_report(path) for path in (args.before, args.after))
    require(before["role"] == "before" and after["role"] == "after", "wrong pair roles")
    for key in ("case", "query", "params", "options", "runtime", "complete_typed_bag_oracle"):
        require(before[key] == after[key], "paired query/runtime/oracle differs")
    for key in ("fixture_sha256", "pin", "probe_nodes", "manifest_sha256"):
        require(before["context"][key] == after["context"][key], "paired resident input differs")
    output_counts(before["samples"])
    output_counts(after["samples"])
    old = {(surface, role): value for surface, role, value in sip.physical_samples(before)}
    checks = []
    for surface, role, value in sip.physical_samples(after):
        baseline = old[(surface, role)]["sqlite_work"]
        work = value["sqlite_work"]
        require(work["vm_steps"] < baseline["vm_steps"], "DF did not reduce actual host SQLite VM")
        checks.append({"surface": surface, "role": role, "before": baseline, "after": work})
    sip.save(args.output, {"status": "passed", "same_fixed_complete_output": True,
        "physical_VM_reduction": checks, "latency_claim": "none; normal samples are separate"})


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
    require(platform.system() == "Linux", "execution requires the authorized native Linux host")
    require(args.memory_limit_bytes is None or args.memory_limit_bytes > 0, "quota must be positive")
    new_path(args.output)
    args.output.mkdir(parents=True)
    sources = [Path(__file__), Path(sip.__file__), Path(short_sampling.__code__.co_filename),
               Path(select_sqlite.__code__.co_filename), Path(check_rows.__code__.co_filename)]
    pins = {str(path.resolve()): file_hash(path) for path in sources}
    artifact = file_hash(args.extension)
    sip.save(args.output / "invocation.json", {"argv": sys.argv, "tool_pins": pins,
        "artifact_sha256": artifact, "role": args.role})
    sqlite = select_sqlite(args.sqlite_library)
    sqlite.lib.sqlite3_sourceid.argtypes, sqlite.lib.sqlite3_sourceid.restype = [], c.c_char_p
    require(sqlite.lib.sqlite3_sourceid().decode() == args.sqlite_source_id,
            "actual SQLite SourceID differs before extension load")
    args.runtime = actual_sql_runtime(sqlite, args.extension)
    require(args.runtime["source_id"] == args.sqlite_source_id, "loaded SQLite SourceID differs")
    reports = [measure_fixture(args, sqlite, Path(manifest), Path(database))
               for manifest, database in args.fixture]
    require(all(file_hash(path) == digest for path, digest in pins.items())
            and file_hash(args.extension) == artifact, "tool/dependency/artifact drift")
    sip.save(args.output / "summary.json", {"status": "passed", "role": args.role,
        "command": args.command, "tool_pins": pins, "artifact_sha256": artifact, "argv": sys.argv,
        "sqlite": args.runtime, "reports": [report["context"] for report in reports],
        "fixed_result_10x_VM_checks": sip.growth_checks(reports) if args.role == "after" else [],
        "VM_scope": "host connection exact stmt_status, distinct from private Hash spill VM",
        "source_membership": "external command guard required; not claimed by this tool",
        "os_cache": "fixture SHA pre-read; measure has independent PROFILE admission; not OS cold"})


if __name__ == "__main__":
    main()
