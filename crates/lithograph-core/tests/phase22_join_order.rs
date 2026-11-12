//! Real cursor ordering and bounded Greedy, with the unreordered total OR as an
//! independent execution oracle. Exhaustive topology tests live beside DPhyp.
use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, QuerySummary, prepare};
use lithograph_core::storage::{self, CommitMetadata, LayerBuilder, OwnerKind, PropertyValue};
use rusqlite::Connection;
use std::collections::BTreeMap;

fn fixture() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
    storage::create_storage_schema(&db).unwrap();
    let root = storage::initialize_root(&db).unwrap().root;
    let common = storage::intern_label(&db, "Common").unwrap();
    let rare = storage::intern_label(&db, "Rare").unwrap();
    let k = storage::intern_property_key(&db, "k").unwrap();
    let mut layer = LayerBuilder::default();
    for number in 0..256 {
        let id = storage::allocate_node_id(&db).unwrap();
        layer.add_node(id).unwrap();
        layer.add_label(id, common).unwrap();
        if number == 17 {
            layer.add_label(id, rare).unwrap();
        }
        layer
            .set_property(OwnerKind::Node, id, k, PropertyValue::Integer(number))
            .unwrap();
    }
    storage::commit_layer(
        &db,
        "main",
        root,
        None,
        &layer,
        &CommitMetadata {
            author: None,
            message: None,
            committed_at: 1,
        },
    )
    .unwrap();
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    db
}

fn run(db: &Connection, query: &str) -> (Vec<Vec<Value>>, QuerySummary) {
    db.execute_batch("SAVEPOINT join_order_read").unwrap();
    let prepared = prepare(db, query, BTreeMap::new(), ExecutionOptions::default()).expect(query);
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(db, 7).expect(query);
        rows.extend(batch.rows);
        if batch.done {
            let summary = cursor.complete(db).unwrap();
            db.execute_batch("RELEASE join_order_read").unwrap();
            return (rows, summary);
        }
    }
}

fn text(parts: usize, rare: usize, oracle: bool) -> String {
    let patterns = (0..parts)
        .map(|part| {
            if part == rare {
                "(n:Rare)"
            } else {
                "(n:Common)"
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "MATCH {patterns} {} RETURN n.k",
        if oracle { "WHERE true OR false" } else { "" }
    )
}

fn hits(summary: &QuerySummary) -> u64 {
    summary
        .metrics
        .operator_profile()
        .unwrap()
        .iter()
        .map(|op| op.db_hits)
        .sum()
}

#[test]
fn three_through_eight_parts_execute_the_rare_start_and_preserve_the_bag() {
    let db = fixture();
    for parts in 3..=8 {
        let query = text(parts, parts - 1, false);
        #[cfg(feature = "test-support")]
        {
            lithograph_core::performance::set_enabled(true);
            lithograph_core::performance::reset();
        }
        let (actual, profile) = run(&db, &format!("PROFILE {query}"));
        #[cfg(feature = "test-support")]
        {
            let counters = lithograph_core::performance::snapshot();
            lithograph_core::performance::set_enabled(false);
            eprintln!("S4 parts={parts} counters={counters:?}");
            if parts == 3 {
                assert!(
                    counters.join_dphyp_pairs > 0 && counters.join_exact_completed > 0,
                    "real exact physical search: {counters:?}"
                );
            }
            if parts == 8 {
                assert!(
                    counters.join_greedy_fallbacks > 0,
                    "dense actual Greedy: {counters:?}"
                );
            }
        }
        let (expected, old) = run(&db, &format!("PROFILE {}", text(parts, parts - 1, true)));
        eprintln!(
            "S4 parts={parts} actual_db_hits={} oracle_db_hits={}",
            hits(&profile),
            hits(&old)
        );
        assert_eq!(actual, vec![vec![Value::Integer(17)]]);
        assert_eq!(actual, expected);
        assert!(
            hits(&profile) * 4 < hits(&old),
            "parts={parts}: optimized={} old={}",
            hits(&profile),
            hits(&old)
        );
        let (first, _) = run(&db, &format!("EXPLAIN {query}"));
        let (second, _) = run(&db, &format!("EXPLAIN {query}"));
        assert_eq!(first, second, "deterministic physical order");
    }
}

#[test]
fn disconnected_components_keep_the_cartesian_product() {
    let db = fixture();
    let (actual, _) = run(
        &db,
        "MATCH(a:Rare),(b:Rare),(c:Common) RETURN a.k,b.k,c.k ORDER BY c.k",
    );
    let (oracle, _) = run(
        &db,
        "MATCH(a:Rare),(b:Rare),(c:Common) WHERE true OR false RETURN a.k,b.k,c.k ORDER BY c.k",
    );
    assert_eq!(actual, oracle);
    assert_eq!(actual.len(), 256);
}

#[cfg(feature = "test-support")]
#[test]
fn twenty_four_parts_retain_a_complete_greedy_when_exact_is_ineligible() {
    let db = fixture();
    lithograph_core::performance::set_enabled(true);
    lithograph_core::performance::reset();
    let query = text(24, 23, false);
    let (actual, profile) = run(&db, &format!("PROFILE {query}"));
    let counters = lithograph_core::performance::snapshot();
    lithograph_core::performance::set_enabled(false);
    eprintln!(
        "S4 parts=24 actual_db_hits={} counters={counters:?}",
        hits(&profile)
    );
    assert_eq!(actual, vec![vec![Value::Integer(17)]]);
    assert!(
        counters.join_greedy_fallbacks > 0,
        "complete Greedy retained: {counters:?}"
    );
    assert_eq!(counters.join_exact_completed, 0);
    assert!(
        counters.planner_states <= 4096
            && counters.planner_candidates <= 1024
            && counters.planner_bytes <= 256 * 1024,
        "{counters:?}"
    );
    let (_, old) = run(&db, &format!("PROFILE {}", text(24, 23, true)));
    assert!(
        hits(&profile) * 4 < hits(&old),
        "Greedy actual access gain: {} vs {}",
        hits(&profile),
        hits(&old)
    );
}

#[test]
fn absent_derived_statistics_preserve_results_and_original_error_timing() {
    let db = fixture();
    db.execute("DELETE FROM _lithograph_checkpoints", [])
        .unwrap();
    let (actual, _) = run(&db, &text(4, 3, false));
    assert_eq!(actual, vec![vec![Value::Integer(17)]]);
    let (empty, _) = run(
        &db,
        "MATCH (n:Missing),(m:Common),(p:Common) WHERE m.k+true=p.k RETURN m.k",
    );
    assert!(empty.is_empty());
}
