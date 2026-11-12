use super::*;
use crate::query::plan::NodeSpec;
use key::HashKeyValue;

fn row(name: &str, id: i64, control: &ExecutionControl<'_>) -> (BindingRow, MemoryReservation) {
    let mut row = BindingRow::default();
    row.insert(name.to_owned(), BindingValue::Node(id));
    let memory = control
        .reserve(expression::binding_row_bytes(&row).unwrap())
        .unwrap();
    (row, memory)
}

fn key(value: i64, digest: [u8; 32], control: &ExecutionControl<'_>) -> EvaluatedHashKey {
    EvaluatedHashKey {
        digest,
        value: HashKeyValue::Scalar(Value::Integer(value)),
        memory: control
            .reserve(std::mem::size_of::<Value>() as u64)
            .unwrap(),
    }
}

fn part() -> PatternPart {
    PatternPart {
        path_variable: None,
        start: NodeSpec {
            variable: Some("n".to_owned()),
            labels: Vec::new(),
            label_names: Vec::new(),
            scan_label: None,
            scan_label_name: None,
            index_seek: None,
            id_seek: None,
            impossible: false,
        },
        relationship: None,
        end: None,
    }
}

#[test]
fn only_same_full_digest_and_false_key_equality_count_as_collisions() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut table = HashTable::new(&control).unwrap();
    let same = [0; 32];
    let mut other = same;
    other[31] = 1; // Same bucket bits, distinct full digest.
    for (id, value, digest) in [(1, 7, same), (2, 7, same), (3, 8, same), (4, 9, other)] {
        let input = row("n", id, &control);
        table
            .insert_reserved(input.0, input.1, key(value, digest, &control), &control)
            .unwrap();
    }
    let input = row("m", 9, &control);
    let mut probe = MemoryProbe {
        row: input.0,
        memory: input.1,
        key: key(7, same, &control),
        next: table.head(&same),
    };
    let mut stats = HashJoinRuntimeMetrics::default();
    let mut ids = Vec::new();
    while let Some(output) = next_match(&mut probe, &table, &part(), &mut stats, &control).unwrap()
    {
        let Some(BindingValue::Node(id)) = output.0.values.get("n") else {
            panic!("merged node")
        };
        ids.push(*id);
    }
    ids.sort();
    assert_eq!(
        ids,
        [1, 2],
        "duplicates are retained; different keys never match"
    );
    assert_eq!(
        stats.hash_collisions, 1,
        "a bucket-only mismatch and true duplicate are not collisions"
    );
    drop((probe, table));
    assert_eq!(resources.used(), 0);
}
