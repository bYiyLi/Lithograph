#![forbid(unsafe_code)]

use lithograph_core::storage::{LayerBuilder, OwnerKind, PropertyValue, RelationshipRecord};

fn main() {
    let mut layer = LayerBuilder::default();
    layer.add_node(7).expect("golden NodeId must be valid");
    layer.add_label(7, 3).expect("golden LabelId must be valid");
    layer
        .add_relationship(RelationshipRecord {
            id: 11,
            source: 7,
            type_id: 5,
            target: 9,
        })
        .expect("golden relationship must be valid");
    layer
        .set_property(
            OwnerKind::Node,
            7,
            4,
            PropertyValue::List(vec![
                PropertyValue::Integer(-17),
                PropertyValue::String("Lithograph".to_owned()),
                PropertyValue::Float(-0.0),
            ]),
        )
        .expect("golden property must be valid");

    let bytes = layer.canonical_lce1().expect("golden layer must encode");
    let hash = layer.content_hash().expect("golden layer must hash");
    println!("{}", hex(&bytes));
    println!("{}", hash.to_hex());
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}
