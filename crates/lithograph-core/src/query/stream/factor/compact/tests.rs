use super::*;
use crate::cypher::Value;
use crate::storage::RelationshipRecord;

fn edge(id: i64) -> BindingRow {
    let mut row = BindingRow::default();
    row.insert("node".into(), BindingValue::Node(id));
    row.insert(
        "relationship".into(),
        BindingValue::Relationship(RelationshipRecord {
            id,
            source: 1,
            target: id,
            type_id: 1,
        }),
    );
    row.used_relationships.insert(id);
    row
}

fn reserved(row: BindingRow, control: &ExecutionControl<'_>) -> (BindingRow, MemoryReservation) {
    let memory = control
        .reserve(expression::binding_row_bytes_controlled(&row, Some(control)).unwrap())
        .unwrap();
    (row, memory)
}

fn packed(result: PackResult) -> CompactMember {
    let PackResult::Packed(member) = result else {
        panic!("matching layout")
    };
    member
}

#[test]
fn shared_node_relationship_layout_keeps_384_members_below_the_resident_target() {
    const MEMBERS: usize = 384;
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let (sample, sample_memory) = reserved(edge(1), &control);
    let raw = sample_memory.bytes();
    assert!(raw * MEMBERS as u64 > super::super::table::FACTOR_RESIDENT_BYTES);
    let layout = CompactLayout::try_new(&sample, &control).unwrap().unwrap();
    drop((sample, sample_memory));
    let slots = control
        .reserve(compact_member_slot_bytes() * MEMBERS as u64)
        .unwrap();
    let mut members = Vec::with_capacity(MEMBERS);
    for id in 1..=MEMBERS as i64 {
        members.push(packed(
            layout.pack(reserved(edge(id), &control), &control).unwrap(),
        ));
    }
    let resident = slots.bytes()
        + layout.owned_bytes()
        + members.iter().map(CompactMember::owned_bytes).sum::<u64>();
    assert_eq!(resources.used(), resident);
    assert!(resident < super::super::table::FACTOR_RESIDENT_BYTES);
    assert!(
        resources.peak() < super::super::table::FACTOR_RESIDENT_BYTES,
        "old raw row and new cell buffers overlap within the original target"
    );
    for index in [0, MEMBERS - 1] {
        let (row, memory) = layout.read(&members[index], &control).unwrap();
        assert_eq!(row, edge(index as i64 + 1));
        drop((row, memory));
    }
    drop((members, slots, layout));
    assert_eq!(resources.used(), 0);
}

#[test]
fn moved_cells_preserve_nan_bits_order_hidden_bindings_csv_and_string_allocation() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let bits = 0x7ff8_0000_0000_0042;
    let mut row = edge(7);
    let text = "lossless".repeat(1024);
    let original_pointer = text.as_ptr();
    row.insert("text".into(), BindingValue::Scalar(Value::String(text)));
    row.insert(
        "nan".into(),
        BindingValue::Scalar(Value::Float(f64::from_bits(bits))),
    );
    row.insert("null".into(), BindingValue::Null);
    row.order.reverse();
    row.values
        .insert("not_in_order".into(), BindingValue::Node(99));
    row.load_csv_context = Some(LoadCsvContext {
        file: Some("synthetic.csv".into()),
        line: Some(17),
    });
    let order = row.order.clone();
    let layout = CompactLayout::try_new(&row, &control).unwrap().unwrap();
    let member = packed(layout.pack(reserved(row, &control), &control).unwrap());
    let text_position = layout.names.iter().position(|name| name == "text").unwrap();
    let BindingValue::Scalar(Value::String(text)) = &member.values[text_position] else {
        panic!("String cell")
    };
    assert_eq!(
        text.as_ptr(),
        original_pointer,
        "packing moves payloads without cloning"
    );
    let (row, memory) = layout.read(&member, &control).unwrap();
    assert_eq!(row.order, order);
    assert_eq!(row.values["not_in_order"], BindingValue::Node(99));
    assert_eq!(row.values["null"], BindingValue::Null);
    let BindingValue::Scalar(Value::Float(value)) = row.values["nan"] else {
        panic!("Float cell")
    };
    assert_eq!(value.to_bits(), bits);
    assert_eq!(row.load_csv_context.as_ref().unwrap().line, Some(17));
    drop((row, memory, member, layout));
    assert_eq!(resources.used(), 0);
}

#[test]
fn changed_keys_order_and_local_buffer_refusal_return_the_complete_original_input() {
    let resources = QueryResources::new(Some(256 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let layout = CompactLayout::try_new(&edge(1), &control).unwrap().unwrap();
    for changed in [false, true] {
        let mut row = edge(2);
        if changed {
            row.insert("extra".into(), BindingValue::Node(5));
        } else {
            row.order.reverse();
        }
        let expected = row.clone();
        let PackResult::Fallback((row, memory)) =
            layout.pack(reserved(row, &control), &control).unwrap()
        else {
            panic!("mismatched layout must remain raw")
        };
        assert_eq!(row, expected);
        drop((row, memory));
    }
    let input = reserved(edge(3), &control);
    let held = resources
        .reserve(resources.limit().unwrap() - resources.used())
        .unwrap();
    let PackResult::Fallback((row, memory)) = layout.pack(input, &control).unwrap() else {
        panic!("optional cell buffers have no remaining grant")
    };
    assert_eq!(row, edge(3));
    drop((row, memory, held, layout));
    assert_eq!(resources.used(), 0);
}
