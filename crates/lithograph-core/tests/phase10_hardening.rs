use std::collections::BTreeMap;

use lithograph_core::cypher::{Value, decode_json, encode_json, parse};
use lithograph_core::query::{ExecutionOptions, QueryCursor, prepare};
use lithograph_core::storage::{
    CommitMetadata, HashId, LayerBuilder, OwnerKind, PropertyValue, SnapshotState,
    allocate_node_id, branch_head, commit_layer, create_branch, create_checkpoint,
    create_storage_schema, delete_checkpoint, initialize_connection_state, initialize_root,
    intern_label, intern_property_key, layer_between, load_snapshot_state,
};
use rusqlite::Connection;

#[derive(Clone, Copy)]
struct DeterministicRng(u64);

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn usize(&mut self, upper: usize) -> usize {
        (self.next_u64() % upper as u64) as usize
    }
}

fn fresh_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
                 id INTEGER PRIMARY KEY CHECK(id=1),\
                 magic TEXT NOT NULL,\
                 database_id TEXT NOT NULL,\
                 storage_format INTEGER NOT NULL\
             );\
             INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format)\
             VALUES(1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000000010', 2);",
        )
        .expect("metadata");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    initialize_connection_state(&connection).expect("connection state");
    connection
}

fn metadata(seed: u64) -> CommitMetadata {
    CommitMetadata {
        author: Some("phase10-property".to_owned()),
        message: Some(format!("seed-{seed}")),
        committed_at: i64::try_from(seed).expect("small deterministic seed") + 1,
    }
}

fn descriptor(commit: HashId) -> String {
    format!("commit/{}", commit.to_hex())
}

fn execute_query(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Vec<Vec<Value>> {
    let prepared = prepare(connection, query, params, options).expect("prepare generated query");
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor
            .next_batch(connection, 64)
            .expect("execute generated query");
        rows.extend(batch.rows);
        if batch.done {
            cursor
                .complete(connection)
                .expect("complete generated query");
            return rows;
        }
    }
}

#[test]
fn generated_parser_inputs_fail_closed_without_panics() {
    let alphabet = [
        ' ', '\n', '\t', '(', ')', '[', ']', '{', '}', ':', ',', '.', '+', '-', '*', '/', '%', '=',
        '<', '>', '!', '$', '\'', '"', '`', '\\', 'a', 'Z', '0', '_', 'λ', '中', '🙂',
    ];
    for seed in 1..=4_096_u64 {
        let mut rng = DeterministicRng::new(seed);
        let length = rng.usize(160);
        let input = (0..length)
            .map(|_| alphabet[rng.usize(alphabet.len())])
            .collect::<String>();
        let _ = parse(&input);
    }
}

#[test]
fn generated_recursive_values_round_trip_through_lithograph_json() {
    let edge_values = [
        Value::Integer(i64::MIN),
        Value::Integer(i64::MAX),
        Value::Float(-0.0),
        Value::Float(f64::INFINITY),
        Value::Float(f64::NEG_INFINITY),
    ];
    for value in edge_values {
        assert_eq!(
            decode_json(&encode_json(&value)).expect("edge round trip"),
            value
        );
    }
    for seed in 1..=2_048_u64 {
        let mut rng = DeterministicRng::new(seed);
        let value = generated_value(&mut rng, 3);
        let encoded = encode_json(&value);
        let decoded = decode_json(&encoded).expect("generated value round trip");
        assert_eq!(decoded, value, "seed {seed}: {encoded}");
    }
}

fn generated_value(rng: &mut DeterministicRng, depth: usize) -> Value {
    let variants = if depth == 0 { 5 } else { 7 };
    match rng.usize(variants) {
        0 => Value::Null,
        1 => Value::Boolean(rng.next_u64() & 1 == 1),
        2 => Value::Integer(rng.next_u64() as i64),
        3 => Value::Float((rng.next_u64() as i64) as f64 / 1_024.0),
        4 => Value::String(generated_text(rng)),
        5 => Value::List(
            (0..rng.usize(5))
                .map(|_| generated_value(rng, depth - 1))
                .collect(),
        ),
        6 => {
            let mut values = BTreeMap::new();
            for index in 0..rng.usize(5) {
                let key = if index == 0 && rng.next_u64() & 3 == 0 {
                    "$type".to_owned()
                } else {
                    format!("key_{index}_{}", rng.next_u64())
                };
                values.insert(key, generated_value(rng, depth - 1));
            }
            Value::Map(values)
        }
        _ => unreachable!("bounded generator variant"),
    }
}

fn generated_text(rng: &mut DeterministicRng) -> String {
    let alphabet = ['a', 'Z', '0', ' ', '\n', '\'', '"', '\\', 'λ', '中', '🙂'];
    (0..rng.usize(24))
        .map(|_| alphabet[rng.usize(alphabet.len())])
        .collect()
}

#[derive(Clone, Copy)]
enum LayerMutation {
    AddNode(i64),
    AddLabel(i64, i64),
    SetInteger(i64, i64, i64),
}

impl LayerMutation {
    fn apply(self, layer: &mut LayerBuilder) {
        match self {
            Self::AddNode(node) => layer.add_node(node).expect("generated Node"),
            Self::AddLabel(node, label) => layer.add_label(node, label).expect("generated Label"),
            Self::SetInteger(node, key, value) => layer
                .set_property(OwnerKind::Node, node, key, PropertyValue::Integer(value))
                .expect("generated property"),
        }
    }
}

#[test]
fn generated_layer_permutations_keep_canonical_bytes_and_hash() {
    for seed in 1..=128_u64 {
        let mut rng = DeterministicRng::new(seed);
        let mutations = generated_layer_mutations(&mut rng);
        let expected = build_layer(&mutations);
        let expected_bytes = expected.canonical_lce1().expect("canonical layer");
        let expected_hash = expected.content_hash().expect("layer hash");
        for round in 0..12_u64 {
            let mut shuffled = mutations.clone();
            shuffle(&mut shuffled, &mut DeterministicRng::new(seed * 17 + round));
            let actual = build_layer(&shuffled);
            assert_eq!(
                actual.canonical_lce1().expect("canonical layer"),
                expected_bytes
            );
            assert_eq!(actual.content_hash().expect("layer hash"), expected_hash);
        }
    }
}

fn generated_layer_mutations(rng: &mut DeterministicRng) -> Vec<LayerMutation> {
    let count = 2 + rng.usize(10);
    let mut mutations = Vec::with_capacity(count * 3);
    for offset in 0..count {
        let node = i64::try_from(offset + 1).expect("small node id");
        mutations.push(LayerMutation::AddNode(node));
        mutations.push(LayerMutation::AddLabel(node, 1 + node % 3));
        mutations.push(LayerMutation::SetInteger(
            node,
            1 + node % 5,
            rng.next_u64() as i64,
        ));
    }
    mutations
}

fn build_layer(mutations: &[LayerMutation]) -> LayerBuilder {
    let mut layer = LayerBuilder::default();
    for mutation in mutations {
        mutation.apply(&mut layer);
    }
    layer
}

fn shuffle<T>(values: &mut [T], rng: &mut DeterministicRng) {
    for index in (1..values.len()).rev() {
        values.swap(index, rng.usize(index + 1));
    }
}

#[test]
fn generated_diff_layers_replay_to_the_same_snapshot() {
    for seed in 1..=24_u64 {
        let connection = fresh_storage();
        let label = intern_label(&connection, "Generated").expect("label");
        let key = intern_property_key(&connection, "value").expect("property key");
        let mut rng = DeterministicRng::new(seed);
        let base = append_generated_nodes(&connection, 4, label, key, &mut rng, seed * 10);
        create_branch(&connection, "replay", base).expect("replay branch");
        let target = append_generated_nodes(&connection, 3, label, key, &mut rng, seed * 10 + 1);
        let before = load_snapshot_state(&connection, base).expect("base snapshot");
        let after = load_snapshot_state(&connection, target).expect("target snapshot");
        let delta = layer_between(&before, &after).expect("generated diff");
        let replay = commit_layer(
            &connection,
            "replay",
            base,
            None,
            &delta,
            &metadata(seed * 10 + 2),
        )
        .expect("replay diff");
        assert_eq!(
            load_snapshot_state(&connection, replay).expect("replayed snapshot"),
            after,
            "seed {seed}"
        );
    }
}

#[test]
fn generated_public_diff_patch_round_trips_forward_and_inverse() {
    for seed in 1..=32_u64 {
        let connection = fresh_storage();
        let mut rng = DeterministicRng::new(seed);
        let fixture = generated_patch_fixture(&connection, &mut rng, seed);
        let forward_head =
            apply_generated_patch(&connection, "forward", fixture.base, fixture.target);
        assert_eq!(
            load_snapshot_state(&connection, forward_head).expect("forward state"),
            fixture.target_state,
            "forward seed {seed}"
        );
        let inverse_head =
            apply_generated_patch(&connection, "inverse", fixture.target, fixture.base);
        assert_eq!(
            load_snapshot_state(&connection, inverse_head).expect("inverse state"),
            fixture.base_state,
            "inverse seed {seed}"
        );
    }
}

struct GeneratedPatchFixture {
    base: HashId,
    target: HashId,
    base_state: SnapshotState,
    target_state: SnapshotState,
}

fn generated_patch_fixture(
    connection: &Connection,
    rng: &mut DeterministicRng,
    seed: u64,
) -> GeneratedPatchFixture {
    let label_a = intern_label(connection, "GeneratedA").expect("label A");
    let label_b = intern_label(connection, "GeneratedB").expect("label B");
    let key = intern_property_key(connection, "value").expect("property key");
    let root = branch_head(connection, "main").expect("root head");
    let first = allocate_node_id(connection).expect("first NodeId");
    let second = allocate_node_id(connection).expect("second NodeId");
    let mut base_layer = LayerBuilder::default();
    for node in [first, second] {
        base_layer.add_node(node).expect("base Node");
        base_layer.add_label(node, label_a).expect("base Label");
        base_layer
            .set_property(
                OwnerKind::Node,
                node,
                key,
                PropertyValue::Integer(rng.next_u64() as i64),
            )
            .expect("base property");
    }
    let base = commit_layer(
        connection,
        "main",
        root,
        None,
        &base_layer,
        &metadata(seed * 100),
    )
    .expect("base Commit");
    let added = allocate_node_id(connection).expect("added NodeId");
    let mut target_layer = LayerBuilder::default();
    target_layer
        .remove_property(OwnerKind::Node, first, key)
        .expect("remove property");
    target_layer
        .remove_label(first, label_a)
        .expect("remove Label");
    target_layer
        .add_label(first, label_b)
        .expect("replace Label");
    target_layer
        .set_property(
            OwnerKind::Node,
            second,
            key,
            PropertyValue::Integer(rng.next_u64() as i64),
        )
        .expect("replace property");
    target_layer.add_node(added).expect("add Node");
    target_layer.add_label(added, label_b).expect("add Label");
    target_layer
        .set_property(
            OwnerKind::Node,
            added,
            key,
            PropertyValue::Integer(rng.next_u64() as i64),
        )
        .expect("add property");
    let target = commit_layer(
        connection,
        "main",
        base,
        None,
        &target_layer,
        &metadata(seed * 100 + 1),
    )
    .expect("target Commit");
    GeneratedPatchFixture {
        base,
        target,
        base_state: load_snapshot_state(connection, base).expect("base state"),
        target_state: load_snapshot_state(connection, target).expect("target state"),
    }
}

fn apply_generated_patch(
    connection: &Connection,
    branch: &str,
    from: HashId,
    to: HashId,
) -> HashId {
    let patch = execute_query(
        connection,
        &format!(
            "CALL lithograph.diff('{}', '{}') YIELD patch RETURN patch",
            descriptor(from),
            descriptor(to)
        ),
        BTreeMap::new(),
        ExecutionOptions::default(),
    )[0][0]
        .clone();
    create_branch(connection, branch, from).expect("Patch target Branch");
    let options = ExecutionOptions::parse_text(&format!(r#"{{"branch":"{branch}"}}"#))
        .expect("Branch options");
    execute_query(
        connection,
        "CALL lithograph.patch.apply($patch) YIELD commit RETURN commit",
        BTreeMap::from([("patch".to_owned(), patch)]),
        options,
    );
    branch_head(connection, branch).expect("patched Branch head")
}

fn append_generated_nodes(
    connection: &Connection,
    count: usize,
    label: i64,
    key: i64,
    rng: &mut DeterministicRng,
    seed: u64,
) -> lithograph_core::storage::HashId {
    let head = branch_head(connection, "main").expect("main head");
    let mut layer = LayerBuilder::default();
    for _ in 0..count {
        let node = allocate_node_id(connection).expect("NodeId");
        layer.add_node(node).expect("add Node");
        layer.add_label(node, label).expect("add Label");
        layer
            .set_property(
                OwnerKind::Node,
                node,
                key,
                PropertyValue::Integer(rng.next_u64() as i64),
            )
            .expect("set property");
    }
    commit_layer(connection, "main", head, None, &layer, &metadata(seed)).expect("generated commit")
}

#[test]
fn checkpoint_materialization_preserves_generated_snapshot_state() {
    let connection = fresh_storage();
    let label = intern_label(&connection, "Checkpointed").expect("label");
    let key = intern_property_key(&connection, "seed").expect("property key");
    let mut rng = DeterministicRng::new(0x10_20_30_40);
    for seed in 1..=32_u64 {
        let commit =
            append_generated_nodes(&connection, 1 + rng.usize(4), label, key, &mut rng, seed);
        let expected = load_snapshot_state(&connection, commit).expect("pre-checkpoint snapshot");
        create_checkpoint(&connection, commit).expect("create checkpoint");
        assert_eq!(
            load_snapshot_state(&connection, commit).expect("checkpoint snapshot"),
            expected,
            "checkpoint seed {seed}"
        );
        delete_checkpoint(&connection, commit).expect("delete checkpoint");
        assert_eq!(
            load_snapshot_state(&connection, commit).expect("post-delete snapshot"),
            expected,
            "checkpoint delete seed {seed}"
        );
    }
}
