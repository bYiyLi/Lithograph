//! A real Grace tail drains, then resumes, the same MATCH's cyclic producer.

use lithograph_core::query::{QueryCursor, QueryErrorKind, prepare};

use super::*;

struct HashTail {
    cycle: CycleFixture,
    expected: Vec<Vec<Value>>,
}

impl HashTail {
    fn new() -> Self {
        let cycle = CycleFixture::new(Shape::Triangle, 16, 1);
        let source_keys = cycle.relationships[2]
            .iter()
            .flat_map(|(&(from, _), ids)| {
                ids.iter()
                    .map(move |id| (format!("{id:?}"), Value::Integer(from as i64)))
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(source_keys.len(), 128);
        let updated = support::write(
            &cycle.db,
            "MATCH(source:S8D2)-[r:S8C]->(:S8D0) SET r.k=source.seq RETURN elementId(r),r.k",
        );
        assert_eq!(updated.rows.len(), source_keys.len());
        let mut seen = std::collections::BTreeSet::new();
        for row in updated.rows {
            let id = format!("{:?}", row[0]);
            assert!(seen.insert(id.clone()));
            assert_eq!(source_keys[&id], row[1]);
        }
        let created = support::write(
            &cycle.db,
            "UNWIND range(0,1023) AS seq CREATE(n:S8ComponentBuild {seq:seq,\
             k:CASE WHEN seq<2 THEN 0 WHEN seq=2 THEN 13 WHEN seq=3 THEN null \
             ELSE 100000+seq END}) RETURN seq,elementId(n),n.k",
        );
        assert_eq!(created.rows.len(), 1024);
        let mut build = BTreeMap::new();
        let mut sequences = std::collections::BTreeSet::new();
        for row in created.rows {
            let Value::Integer(seq) = row[0] else {
                panic!("typed public CREATE sequence")
            };
            assert!((0..1024).contains(&seq) && sequences.insert(seq));
            let key = match seq {
                0 | 1 => Value::Integer(0),
                2 => Value::Integer(13),
                3 => Value::Null,
                _ => Value::Integer(100000 + seq),
            };
            assert_eq!(row[2], key);
            assert!(matches!(&row[1], Value::String(id) if id.starts_with("n:")));
            assert!(
                build
                    .insert(format!("{:?}", row[1]), (row[1].clone(), key))
                    .is_none()
            );
        }
        for ddl in [
            "CREATE CONSTRAINT s8_component_rel_key FOR ()-[r:S8C]-() REQUIRE r.k IS :: INTEGER",
            "CREATE CONSTRAINT s8_component_build_key FOR(n:S8ComponentBuild) REQUIRE n.k IS :: INTEGER",
        ] {
            support::write(&cycle.db, ddl);
        }
        support::checkpoint(&cycle.db);
        let mut expected = Vec::new();
        for prefix in cycle.expected() {
            let key = &source_keys[&format!("{:?}", prefix[5])];
            for (node, build_key) in build.values() {
                if build_key != &Value::Null && build_key == key {
                    let mut row = prefix.clone();
                    row.extend([key.clone(), node.clone(), build_key.clone()]);
                    expected.push(row);
                }
            }
        }
        assert_eq!(expected.len(), 66);
        Self { cycle, expected }
    }

    fn query(&self) -> String {
        let cycle = self.cycle.query();
        let (pattern, projection) = cycle.split_once(" RETURN ").unwrap();
        format!(
            "PROFILE {pattern},(tail:S8ComponentBuild) WHERE r2.k=tail.k \
             RETURN {projection},r2.k,elementId(tail),tail.k"
        )
    }
}

fn assert_tail(actual: &support::Observed, fixture: &HashTail, spill: bool) {
    assert_bag(&actual.rows, &fixture.expected);
    assert_multiway(actual, 352);
    assert!(
        actual.plan.contains("HashJoin"),
        "real Hash tail required; hard512={spill}; plan: {}",
        actual.plan
    );
    let joined = actual_operator(actual, "HashJoin");
    assert_eq!(joined.rows, 66);
    let work = joined.hash.as_ref().expect("real tail Hash build/probe");
    assert_eq!(work.build_rows, 1024);
    assert_eq!(work.probe_rows, 352);
    if spill {
        assert!(work.spill_partitions > 1, "{work:?}");
        assert!(
            work.spill_batches > 0 && work.spill_vm_steps > 0,
            "{work:?}"
        );
        assert!(
            work.spill_encoded_bytes > 0 && work.temp_allocated_bytes > 0,
            "{work:?}"
        );
        assert!(actual.memory_peak_bytes <= 512 * 1024);
    }
}

#[test]
fn closing_relationship_key_feeds_a_real_hash_and_grace_tail_in_the_same_match() {
    let fixture = HashTail::new();
    let before = storage::branch_head(&fixture.cycle.db, "main").unwrap();
    for limit in [None, Some(512 * 1024)] {
        eprintln!("public-component-hash budget={limit:?}");
        let mut options = ExecutionOptions::default();
        options.memory_limit_bytes = limit;
        let actual = read(
            &fixture.cycle.db,
            &fixture.query(),
            BTreeMap::new(),
            options,
            3,
        );
        assert_tail(&actual, &fixture, limit.is_some());
        assert_eq!(
            storage::branch_head(&fixture.cycle.db, "main").unwrap(),
            before
        );
        assert!(fixture.cycle.db.is_autocommit());
    }
}

#[test]
fn grace_tail_cancel_and_close_recover_with_the_same_512k_budget() {
    let fixture = HashTail::new();
    let before = storage::branch_head(&fixture.cycle.db, "main").unwrap();
    let mut options = ExecutionOptions::default();
    options.memory_limit_bytes = Some(512 * 1024);
    for interrupt in [false, true] {
        eprintln!("public-component-hash budget=524288 interrupt={interrupt}");
        let scope = support::ReadScope::new(&fixture.cycle.db);
        let prepared = prepare(
            &fixture.cycle.db,
            &fixture.query(),
            BTreeMap::new(),
            options.clone(),
        )
        .unwrap();
        let plan = prepared.physical.explain();
        assert!(
            plan.contains("MultiwayIntersect") && plan.contains("HashJoin"),
            "{plan}"
        );
        let resources = prepared.resources.clone();
        let mut cursor = QueryCursor::new(prepared);
        {
            let batch = cursor.next_batch(&fixture.cycle.db, 1).unwrap();
            assert_eq!(batch.rows.len(), 1);
            assert!(!batch.done);
            assert!(fixture.expected.contains(&batch.rows[0]));
        }
        if interrupt {
            let error = cursor
                .next_batch_with_interrupt(&fixture.cycle.db, 1, &|| true)
                .unwrap_err();
            assert_eq!(error.kind, QueryErrorKind::Interrupted);
            assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
        } else {
            cursor.cancel(&fixture.cycle.db).unwrap();
        }
        drop(cursor);
        assert_eq!(resources.used(), 0);
        assert!(resources.peak() <= 512 * 1024);
        drop(scope);
        assert!(fixture.cycle.db.is_autocommit());
        assert_eq!(
            storage::branch_head(&fixture.cycle.db, "main").unwrap(),
            before
        );
        let recovered = read(
            &fixture.cycle.db,
            &fixture.query(),
            BTreeMap::new(),
            options.clone(),
            3,
        );
        assert_tail(&recovered, &fixture, true);
    }
}
