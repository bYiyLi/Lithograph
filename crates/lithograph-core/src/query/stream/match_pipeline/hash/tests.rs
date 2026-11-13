use super::*;

fn append_identity_part(row: &mut BindingRow, names: [&str; 3], id: i64, named: bool) {
    if !row.values.contains_key(names[0]) {
        row.insert(names[0].to_owned(), BindingValue::Node(1));
    }
    if named {
        row.insert(
            names[1].to_owned(),
            BindingValue::Relationship(RelationshipRecord {
                id,
                source: 1,
                type_id: 1,
                target: 2,
            }),
        );
    }
    if !row.values.contains_key(names[2]) {
        row.insert(names[2].to_owned(), BindingValue::Node(2));
    }
    row.used_relationships.insert(id);
}

#[test]
fn shared_identity_prefix_has_one_row_skeleton_and_bounds_cloned_order_growth() {
    for named in [false, true] {
        let mut row = BindingRow::default();
        append_identity_part(&mut row, ["n0", "r0", "n1"], 1, named);
        row = row.clone();
        append_identity_part(&mut row, ["n1", "r1", "n2"], 2, named);
        row = row.clone();
        append_identity_part(&mut row, ["n2", "r2", "n0"], 3, named);
        let variables = row.values.len();
        let names = row.values.keys().map(|name| name.len() as u64).sum();
        let capacity = (variables * 2).max(4);
        assert!(row.order.capacity() <= capacity);
        let bound = identity_binding_row_bytes(variables, names, 3, capacity).unwrap();
        let actual = expression::binding_row_bytes(&row).unwrap();
        assert!(actual <= bound, "actual={actual} bound={bound}");
        assert_eq!(row.used_relationships.len(), 3);
        assert_eq!(variables, if named { 6 } else { 3 });
        let per_part =
            identity_binding_row_bytes(if named { 3 } else { 2 }, if named { 6 } else { 4 }, 1, 4)
                .unwrap();
        assert!(bound < per_part * 3);
    }
}

#[test]
fn empty_and_anonymous_relationship_shapes_keep_the_real_used_set_claim() {
    let mut empty = BindingRow::default();
    let bound = identity_binding_row_bytes(0, 0, 0, 0).unwrap();
    assert_eq!(bound, expression::binding_row_bytes(&empty).unwrap());
    empty.used_relationships.insert(7);
    assert_eq!(
        identity_binding_row_bytes(0, 0, 1, 0).unwrap(),
        expression::binding_row_bytes(&empty).unwrap(),
    );
}
