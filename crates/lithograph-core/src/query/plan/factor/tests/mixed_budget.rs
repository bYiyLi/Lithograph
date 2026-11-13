//! Real two-MATCH regression; no manually assembled PreparedQuery or hint.

use super::*;
use crate::query::plan::budget;
use crate::query::{ExecutionOptions, QueryCursor, prepare};

const STAR: &str = "MATCH(h:Hub)-[r:A]->(a:LeafA),(h)-[s:B]->(b:LeafB) WHERE h.k=$key";

fn write(db: &Connection, text: &str) {
    let prepared = prepare(db, text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
    let mut cursor = QueryCursor::new(prepared);
    while !cursor.next_batch(db, 128).unwrap().done {}
    cursor.complete(db).unwrap();
}

fn star_fixture() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    write(&db, "CREATE(h:Hub {k:7}) RETURN elementId(h),h.k");
    for ddl in [
        "CREATE CONSTRAINT s7_hub_k_type FOR(h:Hub) REQUIRE h.k IS :: INTEGER",
        "CREATE CONSTRAINT s7_hub_k_unique FOR(h:Hub) REQUIRE h.k IS UNIQUE",
    ] {
        write(&db, ddl);
    }
    for (kind, label, width) in [("A", "LeafA", 64), ("B", "LeafB", 128)] {
        write(
            &db,
            &format!(
                "MATCH(h:Hub) WHERE h.k=7 UNWIND range(1,{width}) AS seq \
             CREATE(n:{label} {{seq:seq}}) CREATE(h)-[r:{kind}]->(n),(h)-[s:{kind}]->(n) \
             RETURN elementId(n),elementId(r),elementId(s)"
            ),
        );
    }
    db
}

fn cycle(db: &Connection) -> String {
    for domain in 0..3 {
        write(
            db,
            &format!(
                "UNWIND range(0,15) AS seq CREATE(n:S8D{domain} {{seq:seq}}) RETURN elementId(n)"
            ),
        );
    }
    for (source, target, kind) in [(0, 1, "S8A"), (1, 2, "S8B"), (2, 0, "S8C")] {
        write(
            db,
            &format!(
                "MATCH(a:S8D{source}),(b:S8D{target}) WHERE (b.seq-a.seq+16)%16<8 \
             CREATE(a)-[r:{kind}]->(b) RETURN elementId(r)"
            ),
        );
    }
    format!(
        "PROFILE {STAR} WITH DISTINCT h \
        MATCH(n0:S8D0)-[e0:S8A]->(n1:S8D1),(n1)-[e1:S8B]->(n2:S8D2),\
        (n2)-[e2:S8C]->(n0) RETURN elementId(h),elementId(n0),elementId(e0)"
    )
}

fn sip(db: &Connection) -> String {
    write(db, "MATCH(a:LeafA) SET a:G01SipProbe RETURN elementId(a)");
    write(
        db,
        "UNWIND range(0,127) AS seq CREATE(n:G01SipProbe {noise:seq}) RETURN elementId(n)",
    );
    format!(
        "PROFILE {STAR} WITH DISTINCT elementId(a) AS aid WITH [aid] AS ids \
        MATCH(x:G01SipProbe) WHERE elementId(x) IN ids RETURN elementId(x)"
    )
}

#[test]
fn first_factorized_match_retains_its_two_clause_planning_opportunity() {
    let mut failures = Vec::new();
    for (name, seed) in [("cycle", cycle as fn(&Connection) -> String), ("sip", sip)] {
        let db = star_fixture();
        let text = seed(&db);
        storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
        db.execute_batch("SAVEPOINT factor_mixed_budget").unwrap();
        crate::query::plan::budget::trace::start();
        let prepared = prepare(
            &db,
            &text,
            BTreeMap::from([("key".into(), Value::Integer(7))]),
            ExecutionOptions::default(),
        )
        .unwrap();
        let events = crate::query::plan::budget::trace::take();
        let plan = prepared.physical.explain();
        eprintln!("factor-mixed name={name} events={events:?} plan={plan}");
        assert!(events.iter().all(|entry| entry.states <= budget::MAX_STATES && entry.bytes <= budget::MAX_BYTES));
        if !plan.contains("FactorizedAccumulate") {
            failures.push(format!("{name}: {events:?}: {plan}"));
        }
        drop(prepared);
        db.execute_batch("RELEASE factor_mixed_budget").unwrap();
    }
    assert!(
        failures.is_empty(),
        "the first accepted factor component must remain available: {failures:#?}"
    );
}
