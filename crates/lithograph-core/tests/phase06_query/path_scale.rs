use super::*;

#[test]
fn variable_path_static_label_start_does_not_scan_unrelated_nodes() {
    let connection = fresh_storage();
    let _ = rows(
        &connection,
        "UNWIND range(1,1000) AS i CREATE (:Decoy {i:i}) FINISH",
    );
    let _ = rows(
        &connection,
        "CREATE (a:ScaleLowDegree), (b), (c), (d), (e) CREATE (a)-[:R]->(b), (b)-[:R]->(c), (c)-[:R]->(d), (d)-[:R]->(e) FINISH",
    );

    let query = "MATCH (:ScaleLowDegree)-[:R*1..4]->(m) RETURN 1";
    let prepared = prepare(
        &connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare labeled variable path");
    let mut cursor = QueryCursor::new(prepared);
    let mut row_count = 0_usize;
    loop {
        let batch = cursor
            .next_batch(&connection, 16)
            .expect("execute labeled variable path");
        row_count += batch.rows.len();
        if batch.done {
            let summary = cursor
                .complete(&connection)
                .expect("complete variable path");
            assert_eq!(row_count, 4);
            assert!(
                summary.metrics.db_hits < 100,
                "static label start must not scan 1000 unrelated Nodes: {:?}",
                summary.metrics
            );
            break;
        }
    }
}
