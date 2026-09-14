use super::*;

#[test]
fn simple_static_label_recognizes_single_pattern_label() {
    let ast = crate::cypher::parse("MATCH (:ScaleLowDegree)-[:SCALE_LINK*1..4]->(m) RETURN 1")
        .expect("parse pattern");
    let node = ast
        .root
        .descendants()
        .find(|node| node.kind == AstKind::NodePattern)
        .expect("start Node pattern");
    let pattern = parse_node(node).expect("lower Node pattern");
    assert_eq!(
        simple_static_label(&pattern),
        Some("ScaleLowDegree"),
        "labels={:#?}",
        pattern.labels
    );
}
