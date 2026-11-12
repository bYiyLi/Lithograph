use super::*;
use crate::cypher::Value;
use crate::query::QueryErrorKind;
use crate::query::resources::QueryResources;
use crate::query::stream::factor::FactorTableBuilder;
use crate::storage::RelationshipRecord;
use std::cell::Cell;
use std::rc::Rc;

fn node(name: &str, id: i64) -> BindingRow {
    let mut row = BindingRow::default();
    row.insert(name.to_owned(), BindingValue::Node(id));
    row
}

fn id(row: &BindingRow, name: &str) -> i64 {
    let BindingValue::Node(id) = row.values[name] else {
        panic!("expected Node identity")
    };
    id
}

fn packet(
    prefix: BindingRow,
    arms: Vec<Vec<BindingRow>>,
    control: &ExecutionControl<'_>,
) -> GroupPacket {
    let mut builder = FactorTableBuilder::new(prefix, arms.len(), control).unwrap();
    for (arm, rows) in arms.into_iter().enumerate() {
        for row in rows {
            builder.push(arm, row, control).unwrap();
        }
    }
    GroupPacket::new(Rc::new(builder.finish(control).unwrap()), control).unwrap()
}

#[test]
fn exact_three_arm_odometer_keeps_zipped_columns_duplicates_and_empty_products() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut middle = Vec::new();
    for value in [10, 20, 30] {
        let mut row = node("b", value);
        row.insert(
            "zipped".into(),
            BindingValue::Scalar(Value::Integer(value * 10)),
        );
        middle.push(row);
    }
    let mut group = packet(
        node("p", 99),
        vec![
            vec![node("a", 1), node("a", 1)],
            middle,
            vec![node("c", 100), node("c", 200)],
        ],
        &control,
    );
    group
        .set_projections(
            &[GroupProjection::ElementId {
                name: "p".into(),
                output: "prefix".into(),
            }],
            &control,
        )
        .unwrap();
    let mut flatten = FlattenContinuation::new(&control).unwrap();
    flatten
        .install(BindingPacket::Group(group), &control)
        .unwrap();
    let mut actual = Vec::new();
    while let Some(tuple) = flatten
        .with_next_row(&control, |row, projection| {
            assert_eq!(projection.unwrap()[0].names(), ("p", "prefix"));
            assert_eq!(id(row, "p"), 99);
            assert_eq!(
                row.values["zipped"],
                BindingValue::Scalar(Value::Integer(id(row, "b") * 10))
            );
            Ok((id(row, "a"), id(row, "b"), id(row, "c")))
        })
        .unwrap()
    {
        actual.push(tuple);
    }
    let mut expected = Vec::new();
    for a in [1, 1] {
        for b in [10, 20, 30] {
            for c in [100, 200] {
                expected.push((a, b, c));
            }
        }
    }
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 12);
    for arms in [
        Vec::new(),
        vec![vec![node("a", 1)], Vec::new(), vec![node("c", 2)]],
    ] {
        flatten
            .install(
                BindingPacket::Group(packet(node("p", 99), arms, &control)),
                &control,
            )
            .unwrap();
        assert!(
            flatten
                .with_next_row(&control, |_, _| -> QueryResult<()> {
                    panic!("an empty product has no callback")
                })
                .unwrap()
                .is_none()
        );
    }
    drop(flatten);
    assert_eq!(resources.used(), 0);
}

fn edge(name: &str, rel: &str, node_id: i64, relationship_id: i64) -> BindingRow {
    let mut row = node(name, node_id);
    row.insert(
        rel.to_owned(),
        BindingValue::Relationship(RelationshipRecord {
            id: relationship_id,
            source: 99,
            target: node_id,
            type_id: 1,
        }),
    );
    row.used_relationships.insert(relationship_id);
    row
}

#[test]
fn relationship_conflicts_reject_combinations_without_deduplicating_legal_bag_rows() {
    let resources = QueryResources::default();
    let control = ExecutionControl::new(&resources, &|| false);
    let group = packet(
        edge("p", "rp", 99, 9),
        vec![
            vec![
                edge("a", "ra", 1, 1),
                edge("a", "ra", 1, 1),
                edge("a", "ra", 2, 2),
                edge("a", "ra", 9, 9),
            ],
            vec![
                edge("b", "rb", 10, 1),
                edge("b", "rb", 11, 3),
                edge("b", "rb", 11, 3),
            ],
        ],
        &control,
    );
    let mut flatten = FlattenContinuation::new(&control).unwrap();
    flatten
        .install(BindingPacket::Group(group), &control)
        .unwrap();
    let mut actual = Vec::new();
    while let Some(pair) = flatten
        .with_next_row(&control, |row, _| {
            assert_eq!(row.used_relationships.len(), 3);
            assert!(row.used_relationships.contains(&9));
            Ok((id(row, "a"), id(row, "b")))
        })
        .unwrap()
    {
        actual.push(pair);
    }
    assert_eq!(
        actual,
        [
            (1, 11),
            (1, 11),
            (1, 11),
            (1, 11),
            (2, 10),
            (2, 11),
            (2, 11)
        ]
    );
    drop(flatten);
    assert_eq!(resources.used(), 0);
}

fn big_prefix_group(control: &ExecutionControl<'_>) -> GroupPacket {
    let mut prefix = node("p", 7);
    prefix.insert(
        "payload".into(),
        BindingValue::Scalar(Value::String("p".repeat(64 * 1024))),
    );
    packet(
        prefix,
        vec![
            (1..=4).map(|id| node("a", id)).collect(),
            (11..=14).map(|id| node("b", id)).collect(),
        ],
        control,
    )
}

fn first_prefix(flatten: &mut FlattenContinuation, control: &ExecutionControl<'_>) -> (usize, u64) {
    flatten
        .with_next_row(control, |row, _| {
            let BindingValue::Scalar(Value::String(payload)) = &row.values["payload"] else {
                panic!("large prefix String")
            };
            assert_eq!(payload.len(), 64 * 1024);
            Ok((
                payload.as_ptr() as usize,
                expression::binding_row_bytes_controlled(row, Some(control))?,
            ))
        })
        .unwrap()
        .unwrap()
}

fn hold_half_copy_grant(resources: &QueryResources, row_bytes: u64) -> MemoryReservation {
    let free = resources.limit().unwrap() - resources.used();
    resources
        .reserve(
            free.checked_sub(row_bytes / 2)
                .expect("first real row leaves room for a copy"),
        )
        .unwrap()
}

fn callbacks_reuse_prefix_under_the_measured_copy_grant() {
    let resources = QueryResources::new(Some(2 * 1024 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut flatten = FlattenContinuation::new(&control).unwrap();
    flatten
        .install(BindingPacket::Group(big_prefix_group(&control)), &control)
        .unwrap();
    let (pointer, row_bytes) = first_prefix(&mut flatten, &control);
    let held = hold_half_copy_grant(&resources, row_bytes);
    let mut rows = 1;
    while let Some(()) = flatten
        .with_next_row(&control, |row, _| {
            let BindingValue::Scalar(Value::String(payload)) = &row.values["payload"] else {
                panic!("prefix String")
            };
            assert_eq!(
                payload.as_ptr() as usize,
                pointer,
                "the shared prefix allocation survives every output"
            );
            Ok(())
        })
        .unwrap()
    {
        rows += 1;
    }
    assert_eq!(rows, 16);
    drop((flatten, held));
    assert_eq!(resources.used(), 0);
}

fn owned_rows_pay_and_transfer_their_real_copy() {
    let resources = QueryResources::new(Some(2 * 1024 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut flatten = FlattenContinuation::new(&control).unwrap();
    flatten
        .install(BindingPacket::Group(big_prefix_group(&control)), &control)
        .unwrap();
    let (_, row_bytes) = first_prefix(&mut flatten, &control);
    let held = hold_half_copy_grant(&resources, row_bytes);
    assert_eq!(
        flatten.next_row(&control).unwrap_err().kind,
        QueryErrorKind::Resource
    );
    assert!(
        flatten.pending.is_none(),
        "a failed owned copy cannot replay the product"
    );
    drop((flatten, held));
    assert_eq!(resources.used(), 0);

    let mut flatten = FlattenContinuation::new(&control).unwrap();
    flatten
        .install(BindingPacket::Group(big_prefix_group(&control)), &control)
        .unwrap();
    let row = flatten.next_row(&control).unwrap().unwrap();
    let memory = flatten.split_output_memory(&row, &control).unwrap();
    assert_eq!(
        memory.bytes(),
        expression::binding_row_bytes_controlled(&row, Some(&control)).unwrap()
    );
    drop(flatten);
    assert_eq!(
        resources.used(),
        memory.bytes(),
        "only the transferred final row remains owned"
    );
    drop((row, memory));
    assert_eq!(resources.used(), 0);
}

#[test]
fn large_prefix_callbacks_avoid_final_row_copies_and_owned_rows_transfer_their_lease() {
    callbacks_reuse_prefix_under_the_measured_copy_grant();
    owned_rows_pay_and_transfer_their_real_copy();
}

#[derive(Clone, Copy)]
enum Terminal {
    Close,
    Interrupt,
    CallbackError,
    ForeignOwner,
}

fn spilled_terminal(terminal: Terminal) {
    let resources = QueryResources::new(Some(1024 * 1024));
    let cancelled = Cell::new(false);
    let interrupted = || cancelled.get();
    let control = ExecutionControl::new(&resources, &interrupted);
    let mut builder = FactorTableBuilder::new(node("p", 99), 2, &control).unwrap();
    for value in 1..=1000 {
        builder.push(0, node("a", value), &control).unwrap();
        builder.push(1, node("b", value + 1000), &control).unwrap();
    }
    let table = Rc::new(builder.finish(&control).unwrap());
    assert!(
        table.is_spilled(),
        "this continuation reads the actual private TEMP table"
    );
    let weak = Rc::downgrade(&table);
    let mut flatten = FlattenContinuation::new(&control).unwrap();
    flatten
        .install(
            BindingPacket::Group(GroupPacket::new(table, &control).unwrap()),
            &control,
        )
        .unwrap();
    for b in [1001, 1002] {
        assert_eq!(
            flatten
                .with_next_row(&control, |row, _| Ok((id(row, "a"), id(row, "b"))))
                .unwrap(),
            Some((1, b))
        );
    }
    let error = match terminal {
        Terminal::Close => {
            drop(flatten);
            assert!(weak.upgrade().is_none());
            assert_eq!(resources.used(), 0);
            return;
        }
        Terminal::Interrupt => {
            cancelled.set(true);
            flatten.with_next_row(&control, |_, _| Ok(())).unwrap_err()
        }
        Terminal::CallbackError => flatten
            .with_next_row(&control, |_, _| {
                Err::<(), _>(QueryError::invalid_argument("factor callback failed"))
            })
            .unwrap_err(),
        Terminal::ForeignOwner => {
            let foreign = QueryResources::default();
            let foreign_control = ExecutionControl::new(&foreign, &|| false);
            let error = flatten
                .with_next_row(&foreign_control, |_, _| Ok(()))
                .unwrap_err();
            assert_eq!(foreign.used(), 0);
            error
        }
    };
    let expected = match terminal {
        Terminal::Interrupt => QueryErrorKind::Interrupted,
        Terminal::CallbackError => QueryErrorKind::InvalidArgument,
        Terminal::ForeignOwner => QueryErrorKind::Internal,
        Terminal::Close => unreachable!(),
    };
    assert_eq!(error.kind, expected);
    assert!(flatten.pending.is_none());
    assert!(
        weak.upgrade().is_none(),
        "terminal failure drops the actual spilled owner"
    );
    assert_eq!(resources.used(), flatten._memory.bytes());
    drop(flatten);
    assert_eq!(resources.used(), 0);
}

#[test]
fn spilled_products_resume_then_release_on_close_interrupt_and_real_consumer_error() {
    for terminal in [
        Terminal::Close,
        Terminal::Interrupt,
        Terminal::CallbackError,
        Terminal::ForeignOwner,
    ] {
        spilled_terminal(terminal);
    }
}
