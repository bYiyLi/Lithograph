use super::{NodeSpec, node_needs_scan_statistics};

fn node(labels: &[i64], id_seek: Option<i64>) -> NodeSpec {
    NodeSpec {
        variable: Some("n".to_owned()),
        scan_label: labels.first().copied(),
        scan_label_name: labels.first().map(|_| "Label".to_owned()),
        index_seek: None,
        id_seek,
        impossible: false,
        labels: labels.to_vec(),
        label_names: labels.iter().map(|_| "Label".to_owned()).collect(),
    }
}

#[test]
fn single_label_scan_does_not_collect_global_planner_statistics() {
    assert!(!node_needs_scan_statistics(&node(&[1], None)));
}

#[test]
fn multi_label_scan_collects_statistics_to_choose_its_start_label() {
    assert!(node_needs_scan_statistics(&node(&[1, 2], None)));
}

#[test]
fn identity_seek_does_not_collect_statistics_for_its_labels() {
    assert!(!node_needs_scan_statistics(&node(&[1, 2, 3], Some(42))));
}
