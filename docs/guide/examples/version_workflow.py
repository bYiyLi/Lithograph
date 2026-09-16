#!/usr/bin/env python3
"""Exercise v0.1.0 version operations on temporary databases, never user data."""

import argparse
import json
from pathlib import Path
import sqlite3
import tempfile

from python_quickstart import open_graph, query, require


def one(db, text, params=None, options=None):
    result = query(db, text, params, options)
    require(len(result["rows"]) == 1, "Expected exactly one procedure row")
    require(len(set(result["columns"])) == len(result["columns"]), "Duplicate columns")
    return dict(zip(result["columns"], result["rows"][0]))


def head(db, version="branch/main"):
    return one(db, "CALL lithograph.commit.get($version)", {"version": version})["commit"]


def expect_error(action, category):
    try:
        action()
    except sqlite3.Error as error:
        require("LITHOGRAPH_" + category in str(error), str(error))
    else:
        raise RuntimeError("Expected " + category)


def merge_workflow(db, extension, filename):
    query(db, "CREATE (:Person {id:'alice', name:'Alice'}) FINISH")
    baseline = head(db)
    query(db, "CALL lithograph.tag.create('baseline', $version)", {"version": baseline})
    query(db, "CALL lithograph.branch.create('feature', 'tag/baseline')")
    query(db, "MATCH (p:Person) SET p.name='Alicia' FINISH")
    query(db, "MATCH (p:Person) SET p.name='Ally' FINISH", options={"branch": "feature"})
    ours = head(db)
    state = one(db, "CALL lithograph.merge.start('branch/feature', $expected)",
                {"expected": ours}, {"branch": "main"})
    require(state["status"] == "conflicted", "Expected a property conflict")
    require(head(db) == ours, "merge.start moved target Branch")
    session, revision = state["session"], state["revision"]
    # A durable merge session does not depend on the original connection.
    db.close()
    db = open_graph(extension, filename, initialize=False)
    try:
        restored = one(db, "CALL lithograph.merge.get($session)", {"session": session})
        require(restored["revision"] == revision, "Session was not restored")
        expect_error(lambda: query(db, "MATCH (p:Person) RETURN p.name",
            options={"mergeSession": {"id": session, "revision": revision}}),
            "MERGE_CONFLICT")
        conflict = one(db, "CALL lithograph.merge.conflicts($session, 10)",
                       {"session": session})
        resolution = {"conflictId": conflict["conflictId"], "choice": "theirs"}
        resolved = one(db, "CALL lithograph.merge.resolve($session, $revision, $choices)",
                       {"session": session, "revision": revision, "choices": [resolution]})
        require(resolved["unresolved"] == 0, "Conflict not resolved")
        expect_error(lambda: query(db, "CALL lithograph.merge.finalize($session,$revision)",
            {"session": session, "revision": revision}), "MERGE_SESSION_CHANGED")
        revision = resolved["revision"]
        candidate = query(db, "MATCH (p:Person) RETURN p.name AS name",
            options={"mergeSession": {"id": session, "revision": revision}})
        require(candidate["rows"] == [["Ally"]], "Candidate mismatch")
        require(candidate["summary"]["commit"] is None, "Candidate is not a Commit")
        merged = one(db, "CALL lithograph.merge.finalize($session, $revision)",
                     {"session": session, "revision": revision},
                     {"message": "Accept the reviewed candidate"})
        require(merged["status"] == "merged", "Expected a merge Commit")
        require(len(one(db, "CALL lithograph.commit.get('branch/main')")["parents"]) == 2,
                "Merge parent count mismatch")
        print("PASS: conflict, reopen, resolution, stale revision, candidate, finalize")
        return db, baseline
    except Exception:
        db.close()
        raise


def history_workflow(db, baseline):
    patch = one(db, "CALL lithograph.diff($before, 'branch/main')",
                {"before": baseline})["patch"]
    query(db, "CALL lithograph.branch.create('replay', 'tag/baseline')")
    query(db, "CALL lithograph.patch.apply($patch)", {"patch": patch}, {"branch": "replay"})
    require(query(db, "MATCH (p:Person) RETURN p.name", options={"branch": "replay"})["rows"]
            == [["Ally"]], "Patch result mismatch")
    before_data = head(db)
    query(db, "CALL lithograph.commit.data.set($commit,$data)",
          {"commit": before_data, "data": {"review": "accepted", "number": 1}})
    require(head(db) == before_data, "Annotation created a Commit")
    query(db, "CALL lithograph.commit.data.set($commit,$data)",
          {"commit": before_data, "data": None})
    require(one(db, "CALL lithograph.commit.get($commit)", {"commit": before_data})["hasData"],
            "Explicit null annotation was lost")
    query(db, "CALL lithograph.commit.data.clear($commit)", {"commit": before_data})
    require(not one(db, "CALL lithograph.commit.get($commit)",
                    {"commit": before_data})["hasData"], "Annotation not cleared")
    marker = one(db, "CALL lithograph.commit.create($data)", {"data": "review milestone"})
    require(one(db, "CALL lithograph.diff($a,$b)",
                {"a": before_data, "b": marker["commit"]})["patch"]["operations"] == [],
            "Explicit marker changed graph")
    start, cursor, seen = head(db), None, []
    while True:
        text = "CALL lithograph.log($version,2)" if cursor is None else (
            "CALL lithograph.log($version,2,$cursor)")
        page = query(db, text, {"version": start, "cursor": cursor})
        records = [dict(zip(page["columns"], row)) for row in page["rows"]]
        seen.extend(row["commit"] for row in records)
        if not records or records[-1]["cursor"] is None:
            break
        cursor = records[-1]["cursor"]
    require(len(seen) == len(set(seen)) and len(seen) >= 5, "History pagination mismatch")
    print("PASS: diff/patch, mutable Commit Data, explicit marker, paginated history")


def rewrite_workflow(db):
    query(db, "CALL lithograph.branch.create('rewrite', 'tag/baseline')")
    query(db, "MATCH (p:Person) SET p.note='local change' FINISH", options={"branch": "rewrite"})
    rebased = one(db, "CALL lithograph.rebase('branch/main')", options={"branch": "rewrite"})
    require(rebased["status"] == "rebased" and rebased["rewritten"], "Rebase failed")
    view = "MATCH (p:Person) RETURN p.name,p.note"
    before = query(db, view, options={"branch": "rewrite"})["rows"]
    one(db, "CALL lithograph.squash('tag/baseline')", options={"branch": "rewrite"})
    require(query(db, view, options={"branch": "rewrite"})["rows"] == before,
            "Squash changed Snapshot")
    changed = query(db, "MATCH (p:Person) SET p.note='temporary' FINISH",
                    options={"branch": "rewrite"})["summary"]["commit"]
    one(db, "CALL lithograph.revert($commit)", {"commit": changed}, {"branch": "rewrite"})
    require(query(db, view, options={"branch": "rewrite"})["rows"] == before, "Revert failed")
    one(db, "CALL lithograph.reset('tag/baseline')", options={"branch": "rewrite"})
    # v0.1.0 SQL Bridge checkout hits its internal transaction boundary.
    expect_error(lambda: query(db, "CALL lithograph.branch.checkout('rewrite')"),
                 "TRANSACTION_BOUNDARY_REQUIRED")
    require(query(db, "MATCH (p:Person) RETURN p.name", options={"branch": "rewrite"})["rows"]
            == [["Alice"]], "Explicit Branch selection failed")
    query(db, "CALL lithograph.branch.delete('rewrite')")
    query(db, "CALL lithograph.tag.create('movable', 'tag/baseline')")
    query(db, "CALL lithograph.tag.move('movable', 'branch/main')")
    query(db, "CALL lithograph.tag.delete('movable')")
    session = one(db, "CALL lithograph.merge.start('branch/main')")
    query(db, "CALL lithograph.merge.list(10)")
    query(db, "CALL lithograph.merge.abort($session,$revision)", session)
    query(db, "CALL lithograph.gc()")
    print("PASS: rebase, squash, revert, reset, checkout limitation, refs, session abort, GC")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extension", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="lithograph-docs-") as directory:
        filename = str(Path(directory) / "demo.sqlite")
        db = open_graph(args.extension, filename)
        try:
            db, baseline = merge_workflow(db, args.extension, filename)
            history_workflow(db, baseline)
            rewrite_workflow(db)
            destination = str(Path(directory) / "backup.sqlite")
            backup = sqlite3.connect(destination)
            try:
                db.backup(backup)
            finally:
                backup.close()
            restored = open_graph(args.extension, destination, initialize=False)
            try:
                require(head(restored) == head(db), "Backup head mismatch")
                require(json.loads(restored.execute(
                    "SELECT lithograph_integrity_check()").fetchone()[0])["ok"],
                    "Backup integrity failed")
            finally:
                restored.close()
            print("PASS: consistent backup, reopen, Commit preservation, integrity")
        finally:
            db.close()


if __name__ == "__main__":
    main()
