#!/usr/bin/env python3
"""真实 SQLite/file/HTTP 验证 LOAD CSV 的执行前外联禁令。"""

import argparse
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
from pathlib import Path
import sqlite3
import tempfile
from threading import Lock, Thread


DENIED = {"readAccess": {"allowExternalIO": False}}
ALLOWED = {"readAccess": {"allowExternalIO": True}}


def require(condition, message):
    if not condition:
        raise AssertionError(message)


@contextmanager
def csv_server():
    lock = Lock()
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            with lock:
                requests.append(self.path)
            self.send_response(200)
            self.send_header("Content-Type", "text/csv")
            self.end_headers()
            self.wfile.write(b"id,name\n1,Alice\n")

        def log_message(self, _format, *_args):
            pass

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()

    def request_count():
        with lock:
            return len(requests)

    try:
        yield f"http://127.0.0.1:{server.server_port}/synthetic.csv", request_count
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
        require(not thread.is_alive(), "CSV fixture server did not stop")


def execute(db, surface, query, params, options):
    arguments = (query, json.dumps(params), json.dumps(options))
    if surface == "scalar":
        cursor = db.execute("SELECT lithograph(?,?,?)", arguments)
        try:
            return json.loads(cursor.fetchone()[0])["rows"]
        finally:
            cursor.close()
    cursor = db.execute("SELECT event,data FROM lithograph_rows(?,?,?)", arguments)
    rows = []
    summaries = 0
    try:
        for event, data in cursor:
            value = json.loads(data)
            if event == "row":
                rows.append(value)
            elif event == "summary":
                summaries += 1
        require(summaries == 1, "rows execution must produce one final summary")
        return rows
    finally:
        cursor.close()


def head(db):
    response = db.execute(
        "SELECT lithograph(?)", ("CALL lithograph.commit.get('branch/main') "
                                 "YIELD commit RETURN commit",)).fetchone()[0]
    return json.loads(response)["rows"][0][0]


def queries(source):
    literal = "'" + source.replace("'", "''") + "'"
    load = "LOAD CSV WITH HEADERS FROM $source AS row "
    return [
        ("literal", "LOAD CSV WITH HEADERS FROM " + literal +
         " AS row RETURN row.id,row.name", [["1", "Alice"]], 1),
        ("parameter", load + "RETURN row.id,row.name", [["1", "Alice"]], 1),
        ("nested", "CALL { " + load + "RETURN row } "
         "RETURN row.id,row.name", [["1", "Alice"]], 1),
        ("expression-subquery", "RETURN COUNT { " + load + "RETURN row }", [[1]], 1),
        ("interpolation", 'RETURN s"{COUNT { ' + load + 'RETURN row }}"', [["1"]], 1),
        ("zero-row", "UNWIND [] AS unused " + load + "RETURN row", [], 0),
        ("dead-interpolation", 'RETURN CASE WHEN false THEN s"{COUNT { ' +
         load + 'RETURN row }}" ELSE "safe" END', [["safe"]], 0),
    ]


def check_case(db, surface, source_kind, source, count, case):
    name, query, expected, external_reads = case
    context = f"{surface}/{source_kind}/{name}"
    params = {"source": source}
    before = count()
    actual = execute(db, surface, query, params, ALLOWED)
    require(actual == expected, f"{context}: allowed result {actual!r} != {expected!r}")
    wanted = external_reads if source_kind == "http" else 0
    require(count() == before + wanted, f"{context}: allowed HTTP request count")
    before = count()
    before_head = head(db)
    try:
        execute(db, surface, query, params, DENIED)
    except sqlite3.Error as error:
        require(error.sqlite_errorcode == sqlite3.SQLITE_AUTH,
                f"{context}: expected SQLITE_AUTH: {error}")
        require("LITHOGRAPH_ACCESS_DENIED:" in str(error),
                f"{context}: expected ACCESS_DENIED: {error}")
    else:
        raise AssertionError(f"{context}: forbidden LOAD CSV succeeded")
    require(count() == before, f"{context}: denied LOAD CSV contacted HTTP server")
    require(head(db) == before_head, f"{context}: denied LOAD CSV changed Branch head")
    return context


def verify(extension):
    results = []
    with tempfile.TemporaryDirectory(prefix="lithograph-phase18-load-csv-") as directory:
        source = Path(directory) / "synthetic.csv"
        source.write_text("id,name\n1,Alice\n", encoding="utf-8")
        db = sqlite3.connect(":memory:", isolation_level=None)
        try:
            db.enable_load_extension(True)
            db.load_extension(str(extension))
            db.enable_load_extension(False)
            db.execute("SELECT lithograph_init()")
            with csv_server() as (url, count):
                for surface in ("scalar", "rows"):
                    for kind, uri in (("file", source.as_uri()), ("http", url)):
                        for case in queries(uri):
                            results.append(check_case(db, surface, kind, uri, count, case))
            integrity = json.loads(db.execute("SELECT lithograph_integrity_check()").fetchone()[0])
            require(integrity["ok"], "LOAD CSV checks must preserve graph integrity")
        finally:
            db.close()
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extension", type=Path)
    arguments = parser.parse_args()
    cases = verify(arguments.extension.resolve(strict=True))
    print(json.dumps({"passed": len(cases), "cases": cases}, ensure_ascii=False))


if __name__ == "__main__":
    main()
