//! Public CREATE IDs and explicit key recipes define the independent bag.

use std::collections::BTreeSet;

use rusqlite::Connection;

use super::*;

pub(super) struct HashFixture {
    pub(super) db: Connection,
    pub(super) expected: Vec<Vec<Value>>,
}

impl HashFixture {
    pub(super) fn new() -> Self {
        let db = support::empty_db();
        let created = support::write(
            &db,
            "UNWIND range(0,255) AS seq \
             CREATE(a:G01Build {k:seq%32}) \
             CREATE(b:G01Probe {k:CASE WHEN seq<2 THEN 5 WHEN seq=2 THEN 13 \
                 WHEN seq=3 THEN null WHEN seq=4 THEN 99999 ELSE 100000+seq END}) \
             RETURN seq,elementId(a),a.k,elementId(b),b.k",
        );
        assert_eq!(created.rows.len(), 256);
        assert_eq!(created.counters.nodes_created, 512);
        let mut build = vec![(Value::Null, Value::Null); 256];
        let mut probe = vec![Value::Null; 256];
        let mut sequences = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for row in created.rows {
            assert_eq!(row.len(), 5);
            let Value::Integer(seq) = row[0] else {
                panic!("typed public CREATE sequence")
            };
            let seq = usize::try_from(seq).unwrap();
            assert!(seq < 256 && sequences.insert(seq));
            assert!(ids.insert(node_id(&row[1]).to_owned()));
            assert!(ids.insert(node_id(&row[3]).to_owned()));
            assert_eq!(row[2], Value::Integer((seq % 32) as i64));
            let expected = match seq {
                0 | 1 => Value::Integer(5),
                2 => Value::Integer(13),
                3 => Value::Null,
                4 => Value::Integer(99_999),
                _ => Value::Integer(100_000 + seq as i64),
            };
            assert_eq!(row[4], expected);
            build[seq] = (row[1].clone(), row[2].clone());
            probe[seq] = row[4].clone();
        }
        assert_eq!(ids.len(), 512);
        assert_eq!(sequences.len(), 256);
        for label in ["G01Build", "G01Probe"] {
            support::write(
                &db,
                &format!(
                    "CREATE CONSTRAINT {label}_integer FOR(n:{label}) REQUIRE n.k IS :: INTEGER"
                ),
            );
        }
        support::checkpoint(&db);
        // Eight Build owners for key5 each match two Probe owners; eight for
        // key13 match once. DISTINCT retains16 Build IDs; each then supplies
        // its own complete singleton list to the same SIP profile site.
        let mut pairs = 0;
        let mut expected = Vec::new();
        for (id, key) in build {
            let matches = probe
                .iter()
                .filter(|other| **other != Value::Null && *other == &key)
                .count();
            pairs += matches;
            if matches > 0 {
                expected.push(vec![id, key]);
            }
        }
        assert_eq!(pairs, 24);
        assert_eq!(expected.len(), 16);
        Self { db, expected }
    }
}
