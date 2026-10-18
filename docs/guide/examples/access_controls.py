#!/usr/bin/env python3
"""Exercise Lithograph v0.3.2 access options against a disposable SQL graph."""

import argparse
import json
import math
from pathlib import Path
import sqlite3
import tempfile

from python_quickstart import open_graph, query, require
from version_workflow import head, one


def stream(db, text, params=None, options=None):
    payload = (text, json.dumps(params or {}, ensure_ascii=False),
               json.dumps(options or {}, ensure_ascii=False))
    cursor = db.execute("SELECT ordinal,event,data FROM lithograph_rows(?,?,?)", payload)
    events = []
    try:
        for ordinal, event, data in cursor:
            require(ordinal == len(events), "Non-contiguous stream ordinal")
            events.append((event, json.loads(data)))
    finally:
        cursor.close()
    require(bool(events) and events[0][0] == "columns" and events[-1][0] == "summary",
            "Incomplete execution event stream")
    require(all(event == "row" for event, _ in events[1:-1]), "Unexpected stream event")
    return {"columns": events[0][1], "rows": [row for _, row in events[1:-1]],
            "summary": events[-1][1]}


def denied(action):
    try:
        action()
    except sqlite3.Error as error:
        require(str(error).startswith("LITHOGRAPH_ACCESS_DENIED:"), str(error))
        require(error.sqlite_errorcode == sqlite3.SQLITE_AUTH,
                "ACCESS_DENIED did not preserve SQLITE_AUTH")
    else:
        raise RuntimeError("Expected ACCESS_DENIED")


def fixture(db):
    query(db, "CREATE (:人物 {编号:'p001',姓名:'张三',年龄:20,"
          "`@loreweft:key`:'person-p001',私密备注:'内部备注',创建时间:'2026-10-18',"
          "audit_1:1,audit_1_extra:2}) FINISH")


def read_and_result(db):
    hidden = {"readAccess": {"query": {"denyProperties": ["私密备注"]},
                             "result": {"denyPropertyPrefixes": ["@loreweft:"]}}}
    for execute in (query, stream):
        result = execute(db, "MATCH (p:人物) WHERE p.`@loreweft:key`=$key "
                         "RETURN p AS 人物,p.私密备注 AS 备注,p[$field] AS 动态备注,"
                         "{人物:p,列表:[p]} AS 包装",
                         {"key": "person-p001", "field": "私密备注"}, hidden)
        person, note, dynamic, wrapped = result["rows"][0]
        require(note is None and dynamic is None, "Query masking did not produce null")
        require("@loreweft:key" not in person["properties"] and
                "私密备注" not in person["properties"], "Graph property escaped masking")
        require(wrapped == {"人物": person, "列表": [person]},
                "Nested graph-object filtering differs")
        require(person["elementId"] and person["labels"] == ["人物"] and
                result["summary"]["commit"], "Structural output was removed")
        explicit = execute(db, "MATCH (p:人物) RETURN p.`@loreweft:key` AS 值,"
                           "properties(p) AS 属性,p{.*} AS 投影,"
                           "{`@loreweft:key`:'ordinary'} AS 普通对象", options={
                               "readAccess": {"result": {
                                   "denyPropertyPrefixes": ["@loreweft:"]}}})
        value, properties, projection, ordinary = explicit["rows"][0]
        require(value == "person-p001" and properties == projection and
                properties["@loreweft:key"] == value and
                ordinary == {"@loreweft:key": "ordinary"},
                "Result-only rules altered explicit extraction or ordinary Maps")
        regex = execute(db, "MATCH (p:人物) RETURN p.audit_1,p.audit_1_extra", options={
            "readAccess": {"query": {"denyPropertyPatterns": ["audit_[0-9]+"]}}})
        require(regex["rows"] == [[None, 2]], "Property regex was not a full-name match")
        denied(lambda: execute(db, "MATCH (p:人物) RETURN elementId(p)", options={
            "readAccess": {"denyFunctions": ["elementId"]}}))
    print("PASS: scalar/rows query/result, explicit Maps, nested graph objects, full regex")


def controlled_write(db):
    policy = {"readAccess": {"result": {"denyPropertyPrefixes": ["@loreweft:"]},
                              "allowExternalIO": False},
              "writeAccess": {"denyProperties": ["创建时间", "私密备注"],
                              "denyPropertyPrefixes": ["@loreweft:"],
                              "allowDelete": False}}
    changed = query(db, "MATCH (p:人物) WHERE p.`@loreweft:key`=$key "
                    "SET p.姓名=$name RETURN p.姓名", {"key": "person-p001", "name": "李四"}, policy)
    require(changed["rows"] == [["李四"]], "Allowed business-field write failed")
    before = head(db)
    for execute in (query, stream):
        for text in ("MATCH (p:人物) SET p.`@loreweft:key`='person-p001' FINISH",
                     "MATCH (p:人物) SET p[$key]=null FINISH",
                     "MATCH (p:人物) SET p={姓名:'unexpected'} FINISH",
                     "MATCH (p:人物) DETACH DELETE p"):
            denied(lambda: execute(db, text, {"key": "@loreweft:key"}, policy))
            require(head(db) == before, "Rejected execution moved Branch head")
    require(query(db, "MATCH (p:人物) RETURN p.姓名,p.`@loreweft:key`")["rows"] ==
            [["李四", "person-p001"]], "Rejected write changed properties")
    print("PASS: allowed update; protected no-op/dynamic/replacement/delete rollback")


def transaction_failure(db):
    before = head(db)
    db.execute("SELECT lithograph_tx_begin('{}')").fetchone()
    query(db, "CREATE (:待处理 {内容:'临时数据'}) FINISH")
    denied(lambda: stream(db, "MATCH (p:人物) SET p.私密备注='changed' FINISH", options={
        "writeAccess": {"denyProperties": ["私密备注"]}}))
    require(db.in_transaction is False and head(db) == before,
            "Rejected staged write did not abort the explicit transaction")
    require(query(db, "MATCH (p:待处理) RETURN count(p)")["rows"] == [[0]],
            "Earlier staged write survived explicit transaction abort")
    print("PASS: per-execution staged policy and automatic whole-transaction abort")


def version_effects(db):
    baseline = head(db)
    query(db, "MATCH (p:人物) SET p.`@loreweft:key`='person-p001-v2' FINISH")
    query(db, "MATCH (p:人物) SET p.年龄=21 FINISH")
    protected = {"writeAccess": {"denyPropertyPrefixes": ["@loreweft:"]}}
    before = query(db, "MATCH (p:人物) RETURN properties(p)")["rows"]
    previous = head(db)
    squashed = one(db, "CALL lithograph.squash($since)", {"since": baseline}, protected)
    require(squashed["commit"] != previous and
            query(db, "MATCH (p:人物) RETURN properties(p)")["rows"] == before,
            "Squash failed to preserve the effective graph")
    current = head(db)
    denied(lambda: query(db, "CALL lithograph.reset($target)",
                         {"target": baseline}, protected))
    require(head(db) == current and
            query(db, "MATCH (p:人物) RETURN properties(p)")["rows"] == before,
            "Denied Reset changed the protected effective graph")
    denied(lambda: query(db, "CALL lithograph.diff($a,$b)",
                         {"a": baseline, "b": current},
                         {"readAccess": {"query": {
                             "denyPropertyPrefixes": ["@loreweft:"]}}}))
    require(json.loads(db.execute("SELECT lithograph_integrity_check()").fetchone()[0])["ok"],
            "Integrity failed after access-controlled version operations")
    print("PASS: Squash unchanged effective state, denied Reset/Diff, integrity")


def canonical_version_values(db):
    protected = {"writeAccess": {"denyProperties": ["protected"]}}
    query(db, "CREATE (:CanonicalDoc {id:'zero',protected:$value}) FINISH", {"value": 0.0})
    since = head(db)
    query(db, "MATCH (n:CanonicalDoc {id:'zero'}) SET n.protected=$value FINISH",
          {"value": -0.0})
    one(db, "CALL lithograph.squash($since)", {"since": since}, protected)
    zero = query(db, "MATCH (n:CanonicalDoc {id:'zero'}) RETURN n.protected")["rows"][0][0]
    require(zero == 0 and math.copysign(1.0, zero) < 0,
            "Squash changed the protected canonical negative zero")

    nan = {"$type": "Float", "value": "NaN"}
    query(db, "CREATE (:CanonicalDoc {id:'nan',protected:$value,public:0}) FINISH", {"value": nan})
    changed = query(db, "MATCH (n:CanonicalDoc {id:'nan'}) SET n.public=1 FINISH")["summary"]["commit"]
    one(db, "CALL lithograph.revert($commit)", {"commit": changed}, protected)
    require(query(db, "MATCH (n:CanonicalDoc {id:'nan'}) RETURN n.protected,n.public")["rows"] ==
            [[nan, 0]], "Unrelated Revert failed to preserve the protected canonical NaN")
    require(json.loads(db.execute("SELECT lithograph_integrity_check()").fetchone()[0])["ok"],
            "Integrity failed after canonical property reconstruction")
    print("PASS: protected signed-zero Squash and NaN-preserving unrelated Revert")


def verify_access_controls(extension):
    with tempfile.TemporaryDirectory(prefix="lithograph-access-docs-") as directory:
        db = open_graph(extension, str(Path(directory) / "graph.sqlite"))
        try:
            fixture(db)
            read_and_result(db)
            controlled_write(db)
            transaction_failure(db)
            version_effects(db)
            canonical_version_values(db)
        finally:
            db.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extension", type=Path, help="Trusted Lithograph v0.3.2 extension")
    args = parser.parse_args()
    verify_access_controls(args.extension)


if __name__ == "__main__":
    main()
