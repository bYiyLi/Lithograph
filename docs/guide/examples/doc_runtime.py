"""Small SQLite host shared by the documentation checks (standard library only)."""

import json
import os
from pathlib import Path
import selectors
import sqlite3
import subprocess
import time
import tomllib
import uuid

from python_quickstart import open_graph, query


ROOT = Path(__file__).resolve().parents[3]
PROFILE = "CY25-2026.08"


def workspace_version():
    with (ROOT / "Cargo.toml").open("rb") as source:
        return tomllib.load(source)["workspace"]["package"]["version"]


def python_sqlite_supported():
    return sqlite3.sqlite_version_info >= (3, 45, 0) and hasattr(
        sqlite3.Connection, "enable_load_extension"
    )


def sql_quote(value):
    return "'" + str(value).replace("'", "''") + "'"


class DocGraph:
    """Use Python SQLite or the selected CLI against an isolated fixture file."""

    def __init__(self, extension, directory, sqlite_cli=None, provider=None, surface="scalar"):
        self.extension = Path(extension).expanduser().resolve(strict=True)
        self.database = Path(directory) / "graph.sqlite"
        self.sqlite_cli = Path(sqlite_cli).resolve(strict=True) if sqlite_cli else None
        self.provider = Path(provider).resolve(strict=True) if provider else None
        self.surface = surface
        self.db = None
        self.process = None
        if python_sqlite_supported():
            self.db = open_graph(self.extension, str(self.database))
            if self.provider:
                self.db.enable_load_extension(True)
                try:
                    self.db.execute("SELECT load_extension(?, ?)",
                                    (str(self.provider), "sqlite3_syntheticembedding_init"))
                finally:
                    self.db.enable_load_extension(False)
        elif self.sqlite_cli:
            # Keep one host connection for the complete fixture. Reopening the
            # CLI for each query would silently discard connection checkout.
            self.process = subprocess.Popen([str(self.sqlite_cli), "-batch", str(self.database)],
                                            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                            stderr=subprocess.PIPE)
            os.set_blocking(self.process.stdout.fileno(), False)
            os.set_blocking(self.process.stderr.fileno(), False)
            statements = [".bail off", ".mode list", ".headers off", ".nullvalue null",
                          "SELECT load_extension(" + sql_quote(self.extension) +
                          ", 'sqlite3_lithograph_init');"]
            if self.provider:
                statements.append("SELECT load_extension(" + sql_quote(self.provider) +
                                  ", 'sqlite3_syntheticembedding_init');")
            statements.append("SELECT lithograph_init();")
            try:
                self.cli("\n".join(statements))
            except Exception:
                self.close()
                raise
        else:
            raise RuntimeError("Python needs SQLite 3.45+ with extension loading; "
                               "otherwise pass --sqlite3 /path/to/sqlite3")

    def cli(self, sql):
        marker = ("lithograph-doc-end-" + uuid.uuid4().hex + "\n").encode()
        payload = sql.rstrip().rstrip(";") + ";\n.print " + marker.decode().rstrip() + "\n"
        self.process.stdin.write(payload.encode())
        self.process.stdin.flush()
        output, errors = b"", b""
        deadline = time.monotonic() + 120
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdout, selectors.EVENT_READ, "stdout")
            selector.register(self.process.stderr, selectors.EVENT_READ, "stderr")
            while marker not in output:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise RuntimeError("SQLite CLI documentation query exceeded 120 seconds")
                ready = selector.select(remaining)
                if not ready:
                    raise RuntimeError("SQLite CLI documentation query exceeded 120 seconds")
                for key, _ in ready:
                    data = os.read(key.fileobj.fileno(), 65536)
                    if not data:
                        raise RuntimeError("SQLite CLI exited before query completion: " + errors.decode())
                    if key.data == "stdout":
                        output += data
                    else:
                        errors += data
            # stderr writes precede the stdout completion marker in the same
            # process; drain any bytes not selected in the last readiness batch.
            while True:
                try:
                    data = os.read(self.process.stderr.fileno(), 65536)
                except BlockingIOError:
                    break
                if not data:
                    break
                errors += data
        if errors:
            raise RuntimeError(errors.decode().strip())
        return output.split(marker, 1)[0].decode().strip()

    def scalar(self, sql):
        if self.db:
            cursor = self.db.execute(sql)
            try:
                row = cursor.fetchone()
                return row[0] if row else None
            finally:
                cursor.close()
        output = self.cli(sql)
        return output.splitlines()[-1] if output else None

    def version(self):
        return json.loads(self.scalar("SELECT lithograph_version()"))

    def check_version(self):
        version = self.version()
        if version.get("extension") != workspace_version():
            raise RuntimeError("Extension version " + str(version.get("extension")) +
                               " differs from workspace " + workspace_version())
        if version.get("cypherProfile") != PROFILE:
            raise RuntimeError("Unexpected Cypher profile: " + str(version))
        if version.get("storageFormat", {}).get("current") != 3:
            raise RuntimeError("Unexpected storage format: " + str(version))
        return version

    def query(self, cypher, params=None, options=None):
        if self.surface == "rows":
            return self.stream(cypher, params, options)
        if self.db:
            return query(self.db, cypher, params, options)
        args = [cypher, json.dumps(params or {}, allow_nan=False),
                json.dumps(options or {}, allow_nan=False)]
        return json.loads(self.scalar("SELECT lithograph(" +
                          ",".join(sql_quote(arg) for arg in args) + ")"))

    def stream(self, cypher, params=None, options=None):
        args = [cypher, json.dumps(params or {}, allow_nan=False),
                json.dumps(options or {}, allow_nan=False)]
        if self.db:
            cursor = self.db.execute("SELECT ordinal,event,data FROM lithograph_rows(?,?,?)", args)
            try:
                events = [{"ordinal": ordinal, "event": event, "data": json.loads(data)}
                          for ordinal, event, data in cursor]
            finally:
                cursor.close()
        else:
            sql = ("SELECT json_group_array(json_object('ordinal',ordinal,'event',event," +
                   "'data',json(data))) FROM lithograph_rows(" +
                   ",".join(sql_quote(arg) for arg in args) + ")")
            events = json.loads(self.scalar(sql))
        if len(events) < 2 or events[0]["event"] != "columns" or events[-1]["event"] != "summary":
            raise RuntimeError("Invalid rows stream: expected columns, rows, terminal summary")
        columns = events[0]["data"]
        if not isinstance(columns, list) or not all(isinstance(item, str) for item in columns):
            raise RuntimeError("Invalid columns event")
        rows = []
        for ordinal, item in enumerate(events):
            if item["ordinal"] != ordinal:
                raise RuntimeError("Rows stream ordinal is not contiguous from zero")
            if 0 < ordinal < len(events) - 1:
                if item["event"] != "row" or not isinstance(item["data"], list):
                    raise RuntimeError("Unexpected non-row event inside stream")
                if len(item["data"]) != len(columns):
                    raise RuntimeError("Rows event width differs from columns")
                rows.append(item["data"])
        summary = events[-1]["data"]
        if not isinstance(summary, dict) or not {"commit", "queryType", "counters", "metrics"} <= summary.keys():
            raise RuntimeError("Invalid terminal summary")
        return {"columns": columns, "rows": rows, "summary": summary}

    def close(self):
        if self.db:
            self.db.close()
        if self.process:
            try:
                self.process.stdin.write(b".quit\n")
                self.process.stdin.flush()
                self.process.wait(timeout=5)
            except (BrokenPipeError, subprocess.TimeoutExpired):
                self.process.kill()
                self.process.wait(timeout=5)
            finally:
                self.process.stdin.close()
                self.process.stdout.close()
                self.process.stderr.close()
                self.process = None

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()
