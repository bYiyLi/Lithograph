#!/usr/bin/env python3
"""Verify SQL execution lifecycles using disposable Lithograph databases."""

import argparse
import json
from pathlib import Path
import sqlite3
import tempfile

from python_quickstart import open_graph, query, require
from version_workflow import expect_error, head


def scalar(db, sql, parameters=()):
    cursor = db.execute(sql, parameters)
    try:
        return json.loads(cursor.fetchone()[0])
    finally:
        cursor.close()


def stream(db, text, params=None, options=None):
    return db.execute(
        "SELECT ordinal,event,data FROM lithograph_rows(?,?,?)",
        (text, json.dumps(params or {}, allow_nan=False),
         json.dumps(options or {}, allow_nan=False)),
    )


def drain(cursor):
    columns, rows, summary = None, [], None
    try:
        for ordinal, event, payload in cursor:
            data = json.loads(payload)
            if event == "columns":
                require(ordinal == 0 and columns is None, "Invalid columns event")
                columns = data
            elif event == "row":
                require(columns is not None and summary is None, "Invalid row lifecycle")
                rows.append(data)
            elif event == "summary":
                require(summary is None, "Duplicate summary")
                summary = data
            else:
                raise RuntimeError("Unknown event: " + event)
    finally:
        cursor.close()
    require(columns is not None and summary is not None, "Missing success boundary")
    return columns, rows, summary


def verify_streams(db):
    before = head(db)
    columns, rows, summary = drain(stream(db,
        "UNWIND [1,2,3] AS value CREATE (:Streamed {value:value}) "
        "RETURN value ORDER BY value"))
    require(columns == ["value"] and rows == [[1], [2], [3]], "Rows mismatch")
    require(summary["commit"] != before, "Successful stream did not commit")
    require(query(db, "MATCH (n:Streamed) RETURN count(n)")["rows"] == [[3]],
            "Scalar read did not observe streamed writes")

    before = head(db)
    cursor = stream(db, "CREATE (:StoppedAtColumns) RETURN 1 AS value")
    cursor.close()  # execute() has positioned the statement at columns only.
    require(head(db) == before, "Closing at columns changed the head")
    require(query(db, "MATCH (n:StoppedAtColumns) RETURN count(n)")["rows"] == [[0]],
            "Closing at columns created a node")

    cursor = stream(db, "CREATE (:Provisional) RETURN 1 AS value")
    try:
        require(cursor.fetchone()[1] == "columns", "Expected initial columns")
    finally:
        cursor.close()  # DB-API has stepped into runtime work, without summary.
    require(head(db) == before, "Early close kept an unpublished Commit")
    require(query(db, "MATCH (n:Provisional) RETURN count(n)")["rows"] == [[0]],
            "Early close kept provisional writes")

    expect_error(lambda: drain(stream(db,
        "CREATE (:LateProjection) RETURN range(1,2,0) AS values")), "TYPE_ERROR")
    require(head(db) == before, "Late projection error changed the head")
    require(query(db, "MATCH (n:LateProjection) RETURN count(n)")["rows"] == [[0]],
            "Late projection failure kept writes")
    print("PASS: scalar/rows, full stream, columns/row early close, late-error rollback")


def verify_transactions(db):
    before = head(db)
    scalar(db, "SELECT lithograph_tx_begin('{}')")
    query(db, "CREATE (:StagedStream {value:1}) FINISH")
    columns, rows, summary = drain(stream(db,
        "MATCH (n:StagedStream) RETURN n.value AS value"))
    require(columns == ["value"] and rows == [[1]] and summary["commit"] is None,
            "Staged stream returned an incorrect result")
    cursor = stream(db, "MATCH (n:StagedStream) RETURN n.value AS value")
    cursor.close()
    require(not db.in_transaction, "Incomplete cursor kept explicit transaction")
    require(head(db) == before, "Incomplete cursor kept staged writes")
    expect_error(lambda: scalar(db, "SELECT lithograph_tx_commit()"), "INVALID_ARGUMENT")

    query(db, "CREATE (:HeadMoved) FINISH")
    expect_error(lambda: scalar(db, "SELECT lithograph_tx_begin(?)",
        (json.dumps({"expectedHead": before}),)), "BRANCH_HEAD_MOVED")
    require(not db.in_transaction, "Failed expectedHead started a transaction")
    scalar(db, "SELECT lithograph_tx_begin(?)",
           (json.dumps({"expectedHead": head(db)}),))
    query(db, "CREATE (:CommittedOnce) FINISH")
    committed = scalar(db, "SELECT lithograph_tx_commit()")
    require(committed["commit"] == head(db), "Final Commit mismatch")
    print("PASS: staged stream, incomplete-cursor abort, expectedHead and final Commit")


def verify_safe_execution(db):
    safe = {"readOnlyNoExternalIO": True}
    before = head(db)
    require(query(db, "MATCH (n:Streamed) RETURN count(n)", options=safe)["rows"] == [[3]],
            "Safe read mismatch")
    _, rows, _ = drain(stream(db, "RETURN 7 AS value", options=safe))
    require(rows == [[7]], "Safe rows read mismatch")
    for text in (
        "CREATE (:Blocked) FINISH",
        "EXPLAIN CREATE (:Blocked) FINISH",
        "PROFILE CREATE (:Blocked) FINISH",
        "UNWIND [] AS x CREATE (:Blocked) FINISH",
        "CALL { CREATE (:Blocked) }",
        "WHEN false THEN { CREATE (:Blocked) RETURN 1 AS x } ELSE { RETURN 1 AS x }",
        "LOAD CSV FROM $source AS row RETURN row",
        "CALL db.index.fulltext.queryNodes('missing','text') YIELD node RETURN node",
        "CALL db.index.semantic.queryNodes('missing','text') YIELD node RETURN node",
        "UNWIND [1] AS x CALL (x) { RETURN x AS y } IN TRANSACTIONS RETURN y",
    ):
        expect_error(lambda: query(db, text,
            {"source": "file:///__lithograph_safe_execution_never_opened__.csv"}, safe),
            "INVALID_ARGUMENT")
    for invalid in (None, 1, "true"):
        expect_error(lambda: query(db, "RETURN 1",
            options={"readOnlyNoExternalIO": invalid}), "INVALID_ARGUMENT")
    require(head(db) == before, "Safe rejection changed head")
    scalar(db, "SELECT lithograph_tx_begin('{}')")
    query(db, "CREATE (:SafeStaged) FINISH")
    require(query(db, "MATCH (n:SafeStaged) RETURN count(n)", options=safe)["rows"] == [[1]],
            "Safe read could not observe staged writes")
    expect_error(lambda: query(db, "CREATE (:Blocked) FINISH", options=safe),
                 "INVALID_ARGUMENT")
    require(not db.in_transaction and head(db) == before,
            "Safe rejection did not abort explicit transaction")
    print("PASS: safe scalar/rows, nested/zero-row/explain rejection, Boolean, tx abort")


def verify_csv_streams(db):
    with tempfile.TemporaryDirectory(prefix="lithograph-stream-csv-") as directory:
        source = Path(directory) / "bad.csv"
        source.write_text('name\nA\nB\n"unterminated\n', encoding="utf-8")
        params = {"source": source.as_uri()}
        before = head(db)
        try:
            drain(stream(db, "LOAD CSV WITH HEADERS FROM $source AS row "
                "CREATE (:OrdinaryCsv {name:row.name}) FINISH", params))
        except sqlite3.Error:
            pass
        else:
            raise RuntimeError("Malformed CSV unexpectedly succeeded")
        require(head(db) == before, "Ordinary CSV stream kept partial writes")
        require(query(db, "MATCH (n:OrdinaryCsv) RETURN count(n)")["rows"] == [[0]],
                "Ordinary CSV stream did not roll back")
        try:
            drain(stream(db, "LOAD CSV WITH HEADERS FROM $source AS row "
                "CALL (row) { CREATE (:PartialCsv {name:row.name}) } "
                "IN TRANSACTIONS OF 1 ROWS FINISH", params))
        except sqlite3.Error:
            pass
        else:
            raise RuntimeError("Malformed batched CSV unexpectedly succeeded")
        require(head(db) != before, "Batched CSV did not retain committed batches")
        require(query(db, "MATCH (n:PartialCsv) RETURN n.name ORDER BY n.name")["rows"]
                == [["A"], ["B"]], "Earlier CSV batch contents mismatch")
    print("PASS: malformed CSV ordinary rollback and earlier-batch durability")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extension", type=Path)
    args = parser.parse_args()
    db = open_graph(args.extension)
    try:
        verify_streams(db)
        verify_transactions(db)
        verify_safe_execution(db)
        verify_csv_streams(db)
        require(scalar(db, "SELECT lithograph_integrity_check()")["ok"],
                "Integrity failed after lifecycle checks")
    finally:
        db.close()
    print("PASS: all disposable SQL lifecycle examples and integrity")


if __name__ == "__main__":
    main()
