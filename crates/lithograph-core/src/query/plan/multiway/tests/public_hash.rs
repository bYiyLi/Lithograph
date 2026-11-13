//! Prepare the public 66-row Hash/Grace fixture with its complete projection.

use super::*;
use crate::query::plan::budget::trace;

fn fixture() -> Connection {
    let db = crate::query::plan::test_support::database("00000000-0000-4000-8000-000000000027");
    for domain in 0..3 {
        assert_eq!(
            run(
                &db,
                &format!(
                    "UNWIND range(0,15) AS seq CREATE(n:S8D{domain} {{seq:seq}}) RETURN seq,elementId(n)"
                )
            ),
            16
        );
    }
    for (from, to, kind) in [(0, 1, "S8A"), (1, 2, "S8B"), (2, 0, "S8C")] {
        assert_eq!(
            run(
                &db,
                &format!(
                    "MATCH(a:S8D{from}),(b:S8D{to}) WHERE ((b.seq-a.seq+16)%16)<8 \
             UNWIND range(1,1) AS copy CREATE(a)-[r:{kind}]->(b) \
             RETURN a.seq,b.seq,elementId(a),elementId(b),elementId(r),\
             elementId(startNode(r)),elementId(endNode(r)),copy"
                )
            ),
            128
        );
    }
    checkpoint(&db);
    assert_eq!(
        run(
            &db,
            "MATCH(source:S8D2)-[r:S8C]->(:S8D0) SET r.k=source.seq RETURN elementId(r),r.k"
        ),
        128
    );
    assert_eq!(
        run(
            &db,
            "UNWIND range(0,1023) AS seq CREATE(n:S8ComponentBuild {seq:seq,\
         k:CASE WHEN seq<2 THEN 0 WHEN seq=2 THEN 13 WHEN seq=3 THEN null \
         ELSE 100000+seq END}) RETURN seq,elementId(n),n.k"
        ),
        1024
    );
    for text in [
        "CREATE CONSTRAINT s8_component_rel_key FOR ()-[r:S8C]-() REQUIRE r.k IS :: INTEGER",
        "CREATE CONSTRAINT s8_component_build_key FOR(n:S8ComponentBuild) REQUIRE n.k IS :: INTEGER",
    ] {
        run(&db, text);
    }
    checkpoint(&db);
    db
}

#[test]
fn complete_public_hash_tail_projection_keeps_hash_with_default_and_512k() {
    let db = fixture();
    let text = "PROFILE MATCH (n0:S8D0)-[r0:S8A]->(n1:S8D1),\
        (n1:S8D1)-[r1:S8B]->(n2:S8D2),(n2:S8D2)-[r2:S8C]->(n0:S8D0),\
        (tail:S8ComponentBuild) WHERE r2.k=tail.k \
        RETURN elementId(n0),elementId(n1),elementId(n2),elementId(r0),\
        elementId(r1),elementId(r2),r2.k,elementId(tail),tail.k";
    let mut failures = Vec::new();
    for limit in [None, Some(512 * 1024)] {
        let options = ExecutionOptions {
            memory_limit_bytes: limit,
            ..ExecutionOptions::default()
        };
        db.execute_batch("SAVEPOINT phase22_factorized_read")
            .unwrap();
        ADVICE.set(Some(Advice::default()));
        trace::start();
        let prepared = prepare(&db, text, BTreeMap::new(), options).unwrap();
        let trace = trace::take();
        let advice = ADVICE.take().unwrap();
        let plan = prepared.physical.explain();
        eprintln!(
            "public-hash-prepare limit={limit:?} advice={advice:?} trace={trace:#?} plan={plan}"
        );
        if !plan.contains("MultiwayIntersect") || !plan.contains("HashJoin") {
            failures.push(format!("{limit:?}: {advice:?}: {trace:#?}: {plan}"));
        }
        drop(prepared);
        db.execute_batch("RELEASE phase22_factorized_read").unwrap();
    }
    assert!(
        failures.is_empty(),
        "public fixture must choose its real operators: {failures:#?}"
    );
}
