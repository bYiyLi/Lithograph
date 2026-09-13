use super::*;

#[test]
fn load_csv_preserves_clause_barriers_and_global_projection_semantics() {
    let connection = fresh_storage();
    let (path, uri) = csv_fixture("clause-barrier", "value\n1\n2\n");
    let mutation = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS row CREATE (:CsvBarrier {{value:row.value}}) WITH row MATCH (n:CsvBarrier) RETURN row.value AS value, count(n) AS seen ORDER BY value"
    );
    let (rows, _) = execute(&connection, &mutation, ExecutionOptions::default())
        .expect("LOAD CSV mutation clause barrier");
    assert_eq!(
        rows,
        vec![
            vec![Value::String("1".to_owned()), Value::Integer(2)],
            vec![Value::String("2".to_owned()), Value::Integer(2)],
        ]
    );
    fs::remove_file(path).expect("remove clause-barrier CSV fixture");

    let (read_path, read_uri) = csv_fixture("read-global", "2\n1\n");
    let order_query =
        format!("LOAD CSV FROM '{read_uri}' AS row RETURN row[0] AS value ORDER BY value");
    let (ordered, _) = execute(&connection, &order_query, ExecutionOptions::default())
        .expect("LOAD CSV global ORDER BY");
    assert_eq!(
        ordered,
        vec![
            vec![Value::String("1".to_owned())],
            vec![Value::String("2".to_owned())],
        ]
    );
    let count_query = format!("LOAD CSV FROM '{read_uri}' AS row RETURN count(row)");
    let (counted, _) = execute(&connection, &count_query, ExecutionOptions::default())
        .expect("LOAD CSV global aggregation");
    assert_eq!(counted, vec![vec![Value::Integer(2)]]);
    fs::remove_file(read_path).expect("remove read-global CSV fixture");

    let mut batched = String::from("value\n");
    for value in 0..300 {
        batched.push_str(&format!("{value}\n"));
    }
    let (batched_path, batched_uri) = csv_fixture("batched-barrier", &batched);
    let batched_query = format!(
        "LOAD CSV WITH HEADERS FROM '{batched_uri}' AS row CREATE (:CsvBatchedBarrier {{value:row.value}}) WITH count(row) AS imported MATCH (n:CsvBatchedBarrier) RETURN imported, count(n)"
    );
    let (batched_rows, _) = execute(&connection, &batched_query, ExecutionOptions::default())
        .expect("LOAD CSV clause barrier across spill batches");
    assert_eq!(
        batched_rows,
        vec![vec![Value::Integer(300), Value::Integer(300)]]
    );
    fs::remove_file(batched_path).expect("remove batched-barrier CSV fixture");
}

#[test]
fn load_csv_spill_preserves_bound_graph_elements_across_write_clauses() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:CsvTarget {id:'1', value:0, temp:true}), (:CsvTarget {id:'2', value:0, temp:true})",
        ExecutionOptions::default(),
    )
    .expect("seed CSV mutation targets");
    let (path, uri) = csv_fixture("direct-writes", "id,value\n1,10\n2,20\n");
    let update = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS row MATCH (n:CsvTarget {{id:row.id}}) SET n.value = toInteger(row.value) REMOVE n.temp RETURN n.id, n.value ORDER BY n.id"
    );
    let (updated, _) = execute(&connection, &update, ExecutionOptions::default())
        .expect("LOAD CSV MATCH SET REMOVE");
    assert_eq!(
        updated,
        vec![
            vec![Value::String("1".to_owned()), Value::Integer(10)],
            vec![Value::String("2".to_owned()), Value::Integer(20)],
        ]
    );

    let delete = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS row MATCH (n:CsvTarget {{id:row.id}}) DELETE n FINISH"
    );
    execute(&connection, &delete, ExecutionOptions::default()).expect("LOAD CSV MATCH DELETE");
    let (remaining, _) = execute(
        &connection,
        "MATCH (n:CsvTarget) RETURN count(n)",
        ExecutionOptions::default(),
    )
    .expect("count deleted CSV targets");
    assert_eq!(remaining, vec![vec![Value::Integer(0)]]);

    let merge = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS row MERGE (n:CsvMerged {{id:row.id}}) ON CREATE SET n.value = toInteger(row.value) FINISH"
    );
    execute(&connection, &merge, ExecutionOptions::default()).expect("LOAD CSV MERGE");
    let (merged, _) = execute(
        &connection,
        "MATCH (n:CsvMerged) RETURN n.id, n.value ORDER BY n.id",
        ExecutionOptions::default(),
    )
    .expect("query merged CSV nodes");
    assert_eq!(
        merged,
        vec![
            vec![Value::String("1".to_owned()), Value::Integer(10)],
            vec![Value::String("2".to_owned()), Value::Integer(20)],
        ]
    );
    fs::remove_file(path).expect("remove direct-writes CSV fixture");
}

#[test]
fn load_csv_streams_headers_quotes_delimiters_and_escapes() {
    let connection = fresh_storage();
    let (path, uri) = csv_fixture(
        "read",
        "name,city\nAlice,\"New\nYork\"\nBob,San Francisco\n",
    );
    let absolute = fs::canonicalize(&path).expect("canonical CSV fixture path");
    let query = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS row RETURN row.name AS name, row.city AS city, file() AS source, linenumber() AS line"
    );
    let (rows, _) = execute(&connection, &query, ExecutionOptions::default()).expect("load csv");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::String("Alice".to_owned()));
    assert_eq!(rows[0][1], Value::String("New\nYork".to_owned()));
    assert_eq!(
        rows[0][2],
        Value::String(absolute.to_string_lossy().into_owned())
    );
    assert_eq!(rows[0][3], Value::Integer(2));
    assert_eq!(rows[1][3], Value::Integer(3));
    fs::remove_file(path).expect("remove csv fixture");

    let (delimiter_path, delimiter_uri) = csv_fixture("delimiter", "Alice;London\nBob;Paris\n");
    let delimiter_query = format!(
        "LOAD CSV FROM '{delimiter_uri}' AS row FIELDTERMINATOR ';' RETURN row[0], row[1], linenumber() ORDER BY row[0]"
    );
    let (delimiter_rows, _) = execute(&connection, &delimiter_query, ExecutionOptions::default())
        .expect("LOAD CSV without headers and with custom delimiter");
    assert_eq!(
        delimiter_rows,
        vec![
            vec![
                Value::String("Alice".to_owned()),
                Value::String("London".to_owned()),
                Value::Integer(1)
            ],
            vec![
                Value::String("Bob".to_owned()),
                Value::String("Paris".to_owned()),
                Value::Integer(2)
            ],
        ]
    );
    let unicode_delimiter_query = format!(
        "LOAD CSV FROM '{delimiter_uri}' AS row FIELDTERMINATOR '\\u003B' RETURN row[0], row[1] ORDER BY row[0]"
    );
    let (unicode_delimiter_rows, _) = execute(
        &connection,
        &unicode_delimiter_query,
        ExecutionOptions::default(),
    )
    .expect("LOAD CSV accepts four-digit Unicode FIELDTERMINATOR escape");
    assert_eq!(
        unicode_delimiter_rows,
        vec![
            vec![
                Value::String("Alice".to_owned()),
                Value::String("London".to_owned())
            ],
            vec![
                Value::String("Bob".to_owned()),
                Value::String("Paris".to_owned())
            ],
        ]
    );
    fs::remove_file(delimiter_path).expect("remove delimiter CSV fixture");

    let (escape_path, escape_uri) = csv_fixture(
        "escaped-quotes",
        "\"1\",\"The \"\"Symbol\"\"\"\n\"2\",\"The \\\"Symbol\\\"\"\n",
    );
    let escape_query =
        format!("LOAD CSV FROM '{escape_uri}' AS row RETURN row[0], row[1] ORDER BY row[0]");
    let (escaped, _) = execute(&connection, &escape_query, ExecutionOptions::default())
        .expect("LOAD CSV accepts doubled and backslash-escaped quotes");
    assert_eq!(
        escaped,
        vec![
            vec![
                Value::String("1".to_owned()),
                Value::String("The \"Symbol\"".to_owned())
            ],
            vec![
                Value::String("2".to_owned()),
                Value::String("The \"Symbol\"".to_owned())
            ],
        ]
    );
    fs::remove_file(escape_path).expect("remove escaped-quote CSV fixture");
}

#[test]
fn load_csv_context_survives_with_and_is_null_outside_load_csv() {
    let connection = fresh_storage();
    let (path, uri) = csv_fixture("context", "name\nAlice\nBob\n");
    let absolute = fs::canonicalize(&path).expect("canonical CSV fixture path");
    let context_query = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS row WITH row RETURN row.name, file(), linenumber() ORDER BY row.name"
    );
    let (rows, _) = execute(&connection, &context_query, ExecutionOptions::default())
        .expect("LOAD CSV context survives WITH");
    assert_eq!(
        rows,
        vec![
            vec![
                Value::String("Alice".to_owned()),
                Value::String(absolute.to_string_lossy().into_owned()),
                Value::Integer(2)
            ],
            vec![
                Value::String("Bob".to_owned()),
                Value::String(absolute.to_string_lossy().into_owned()),
                Value::Integer(3)
            ],
        ]
    );

    let star_query =
        format!("LOAD CSV WITH HEADERS FROM '{uri}' AS row RETURN * ORDER BY row.name");
    let (star_rows, _) = execute(&connection, &star_query, ExecutionOptions::default())
        .expect("LOAD CSV internal context is not exposed by RETURN star");
    assert!(star_rows.iter().all(|row| row.len() == 1));

    let subquery = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS row CALL (row) {{ RETURN file() AS source, linenumber() AS line }} RETURN row.name, source, line ORDER BY row.name"
    );
    let (subquery_rows, _) = execute(&connection, &subquery, ExecutionOptions::default())
        .expect("LOAD CSV context survives explicit CALL scope");
    assert_eq!(subquery_rows, rows);

    let collision = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS row WITH row, 'user-value' AS __lithograph_load_csv_file RETURN __lithograph_load_csv_file, file() ORDER BY row.name"
    );
    let (collision_rows, _) = execute(&connection, &collision, ExecutionOptions::default())
        .expect("LOAD CSV context does not collide with user variables");
    assert_eq!(
        collision_rows,
        vec![
            vec![
                Value::String("user-value".to_owned()),
                Value::String(absolute.to_string_lossy().into_owned())
            ],
            vec![
                Value::String("user-value".to_owned()),
                Value::String(absolute.to_string_lossy().into_owned())
            ],
        ]
    );

    let (second_path, second_uri) = csv_fixture("context-second", "code\nX\n");
    let second_absolute =
        fs::canonicalize(&second_path).expect("canonical second CSV fixture path");
    let multiple = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS first LOAD CSV WITH HEADERS FROM '{second_uri}' AS second RETURN first.name, second.code, file(), linenumber() ORDER BY first.name"
    );
    let (multiple_rows, _) = execute(&connection, &multiple, ExecutionOptions::default())
        .expect("multiple LOAD CSV clauses use the most recently executed context");
    assert_eq!(
        multiple_rows,
        vec![
            vec![
                Value::String("Alice".to_owned()),
                Value::String("X".to_owned()),
                Value::String(second_absolute.to_string_lossy().into_owned()),
                Value::Integer(2)
            ],
            vec![
                Value::String("Bob".to_owned()),
                Value::String("X".to_owned()),
                Value::String(second_absolute.to_string_lossy().into_owned()),
                Value::Integer(2)
            ],
        ]
    );

    let nested = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS first CALL (first) {{ LOAD CSV WITH HEADERS FROM '{second_uri}' AS second RETURN second.code AS code }} RETURN first.name, code, file(), linenumber() ORDER BY first.name"
    );
    let (nested_rows, _) = execute(&connection, &nested, ExecutionOptions::default())
        .expect("subquery LOAD CSV context becomes the most recently executed context");
    assert_eq!(nested_rows, multiple_rows);

    let nested_mutation = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS first CALL (first) {{ LOAD CSV WITH HEADERS FROM '{second_uri}' AS second CREATE (:CsvNestedContext {{code:second.code}}) RETURN second.code AS code }} RETURN first.name, code, file(), linenumber() ORDER BY first.name"
    );
    let (nested_mutation_rows, _) =
        execute(&connection, &nested_mutation, ExecutionOptions::default()).expect(
            "mutating subquery LOAD CSV context becomes the most recently executed context",
        );
    assert_eq!(nested_mutation_rows, multiple_rows);
    fs::remove_file(second_path).expect("remove second context CSV fixture");
    fs::remove_file(path).expect("remove context CSV fixture");

    let (outside, _) = execute(
        &connection,
        "RETURN file() AS source, linenumber() AS line",
        ExecutionOptions::default(),
    )
    .expect("LOAD CSV context functions outside LOAD CSV");
    assert_eq!(outside, vec![vec![Value::Null, Value::Null]]);
}
