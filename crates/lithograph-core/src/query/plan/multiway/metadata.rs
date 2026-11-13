//! The existing descriptor's charged metadata, also protected from advice.

use super::*;

pub(super) fn description_bytes(step: &MatchStep) -> usize {
    step.parts.iter().fold(0usize, |bytes, part| {
        [Some(&part.start), part.end.as_ref()]
            .into_iter()
            .flatten()
            .fold(bytes.saturating_add(512), |bytes, node| {
                bytes
                    .saturating_add(variable_bytes(node).saturating_mul(4))
                    .saturating_add(
                        node.label_names
                            .iter()
                            .fold(0usize, |bytes, name| bytes.saturating_add(name.len()))
                            .saturating_mul(2),
                    )
                    .saturating_add(node.labels.len().saturating_mul(64))
            })
    })
}

pub(crate) fn headroom_bytes(step: &MatchStep) -> usize {
    let names = step.parts.iter().fold(0usize, |bytes, part| {
        bytes
            .saturating_add(variable_bytes(&part.start))
            .saturating_add(part.end.as_ref().map_or(0, variable_bytes))
    });
    // Interning compares every occurrence with at most two Nodes per Part;
    // a cyclic component's first order contains at most one Node per edge.
    description_bytes(step)
        .saturating_add(step.parts.len().saturating_mul(2).saturating_mul(names))
        .saturating_add(step.parts.len().saturating_mul(32))
}

fn variable_bytes(node: &NodeSpec) -> usize {
    node.variable.as_ref().map_or(0, String::len)
}
