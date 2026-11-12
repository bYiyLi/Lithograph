//! Pure advice over distributions already collected for this query's snapshot.
//! No key value, SQLite read, or conditional-domain estimate is created here.

use super::super::HashInputKey;
use crate::query::plan::{MatchStep, NodeSpec, PatternPart};
use crate::query::schema::IndexDistribution;
use crate::query::stats::CardinalityEstimate;
use crate::storage::{IndexDefinition, IndexTarget};

/// These are facts about the Index's entire owner domain, not a filtered part.
/// `providers` says which occurrences require the matching Label/Type. Before
/// using the NDV, the caller must prove its actual input has that whole scope.
/// In particular a GraphView, extra Label, local predicate, bound endpoint, or
/// relationship expansion must not silently turn this into a subset NDV.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HashKeyAdvice {
    pub(super) providers: u128,
    pub(super) indexed_owners: u64,
    pub(super) ndistinct: CardinalityEstimate,
    pub(super) max_frequency: CardinalityEstimate,
    pub(super) max_observed_frequency: u64,
    /// A same-Schema single-column Constraint, rather than sampled uniqueness.
    pub(super) unique: bool,
}

/// The definition and sample must come from the same already-validated query
/// collection entry. Checking its hash again would serialize the definition.
/// Entity identity keys have no property Index domain and remain unknown.
pub(super) fn key_advice(
    step: &MatchStep,
    key: &HashInputKey,
    index: &IndexDefinition,
    sample: IndexDistribution,
) -> Option<HashKeyAdvice> {
    let property = key.property.as_deref()?;
    if !valid_sample(&sample) {
        return None;
    }
    let providers = step
        .parts
        .iter()
        .enumerate()
        .take(u128::BITS as usize)
        .filter(|(ordinal, _)| key.providers.0 & (1_u128 << *ordinal) != 0)
        .filter(|(_, part)| covers(part, key.variable.as_str(), property, &index.target))
        .fold(0, |mask, (ordinal, _)| mask | (1_u128 << ordinal));
    (providers != 0).then_some(HashKeyAdvice {
        providers,
        indexed_owners: sample.indexed_owners,
        ndistinct: sample.ndistinct,
        max_frequency: sample.max_frequency,
        max_observed_frequency: sample.max_observed_frequency,
        unique: false,
    })
}

fn covers(part: &PatternPart, variable: &str, property: &str, target: &IndexTarget) -> bool {
    match target {
        IndexTarget::NodeProperties { label, properties } => {
            properties.as_slice() == [property]
                && (node_covers(&part.start, variable, label)
                    || part
                        .end
                        .as_ref()
                        .is_some_and(|node| node_covers(node, variable, label)))
        }
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => {
            properties.as_slice() == [property]
                && part.relationship.as_ref().is_some_and(|relationship| {
                    relationship.variable.as_deref() == Some(variable)
                        && relationship.type_name.as_ref() == Some(relationship_type)
                })
        }
        _ => false,
    }
}

fn node_covers(node: &NodeSpec, variable: &str, label: &str) -> bool {
    node.variable.as_deref() == Some(variable) && node.label_names.iter().any(|name| name == label)
}

fn valid_sample(sample: &IndexDistribution) -> bool {
    let owners = sample.indexed_owners;
    if !valid_interval(sample.ndistinct, owners)
        || !valid_interval(sample.max_frequency, owners)
        || sample.sampled_owners as u128 > u128::from(owners)
        || sample.max_observed_frequency > sample.max_frequency.lower
    {
        return false;
    }
    if owners == 0 {
        return sample.sampled_owners == 0 && sample.max_observed_frequency == 0;
    }
    sample.ndistinct.lower != 0
        && sample.max_frequency.lower != 0
        && sample.sampled_owners != 0
        && sample.max_observed_frequency <= sample.sampled_owners as u64
        && sample
            .ndistinct
            .lower
            .checked_sub(1)
            .and_then(|rest| rest.checked_add(sample.max_frequency.lower))
            .is_some_and(|observed| observed <= owners)
}

fn valid_interval(estimate: CardinalityEstimate, owners: u64) -> bool {
    estimate.lower <= estimate.expected
        && estimate.expected <= estimate.upper
        && estimate.upper <= owners
}

/// Estimate equal-key candidate pairs K before other predicates produce J.
/// Advice must already pass the caller's full-cohort and provider-ready gates.
/// NDV supplies a uniform-overlap expectation only; no overlap or NULL fraction
/// is proved, so the retained interval is always 0..the Cartesian upper bound.
/// With one or both NDVs absent, retain the full product as the expectation.
/// MCVs are not multiplied: the two sides' most common values may differ.
pub(super) fn key_matches(
    outer: Option<CardinalityEstimate>,
    inner: Option<CardinalityEstimate>,
    outer_key: Option<&HashKeyAdvice>,
    inner_key: Option<&HashKeyAdvice>,
) -> Option<CardinalityEstimate> {
    if outer.is_some_and(|rows| rows.upper == 0) || inner.is_some_and(|rows| rows.upper == 0) {
        return Some(CardinalityEstimate::exact(0));
    }
    let (outer, inner) = (outer?, inner?);
    let product = u128::from(outer.expected) * u128::from(inner.expected);
    let divisor = match (outer_key, inner_key) {
        (Some(left), Some(right))
            if left.ndistinct.expected != 0 && right.ndistinct.expected != 0 =>
        {
            left.ndistinct
                .expected
                .min(outer.expected.max(1))
                .max(right.ndistinct.expected.min(inner.expected.max(1)))
        }
        _ => 1,
    };
    let mut upper = outer.upper.saturating_mul(inner.upper);
    if outer_key.is_some_and(|key| key.unique) {
        upper = upper.min(inner.upper);
    }
    if inner_key.is_some_and(|key| key.unique) {
        upper = upper.min(outer.upper);
    }
    Some(CardinalityEstimate {
        expected: (product
            .div_ceil(u128::from(divisor))
            .min(u128::from(u64::MAX)) as u64)
            .min(upper),
        lower: 0,
        upper,
    })
}

#[cfg(test)]
mod tests {
    use super::super::super::PartSet;
    use super::*;
    use crate::query::plan::{Direction, RelationshipSpec};
    use crate::storage::{HashId, StandardIndexKind};

    fn node(variable: &str, labels: &[&str]) -> NodeSpec {
        NodeSpec {
            variable: Some(variable.into()),
            labels: Vec::new(),
            label_names: labels.iter().map(|label| (*label).into()).collect(),
            scan_label: None,
            scan_label_name: None,
            index_seek: None,
            id_seek: None,
            impossible: false,
        }
    }

    fn part(variable: &str, labels: &[&str]) -> PatternPart {
        PatternPart {
            path_variable: None,
            start: node(variable, labels),
            relationship: None,
            end: None,
        }
    }

    fn step(parts: Vec<PatternPart>) -> MatchStep {
        MatchStep {
            clause_start: 0,
            optional: false,
            parts,
            predicate: None,
            early_predicates: Vec::new(),
            join: None,
        }
    }

    fn key(variable: &str, property: Option<&str>, providers: u128) -> HashInputKey {
        HashInputKey {
            variable: variable.into(),
            property: property.map(Into::into),
            providers: PartSet(providers),
        }
    }

    fn index(target: IndexTarget) -> IndexDefinition {
        IndexDefinition {
            name: "sample".into(),
            kind: StandardIndexKind::Range,
            target,
            owning_constraint: None,
            labels_or_types: Vec::new(),
            additional_properties: Vec::new(),
            configuration: None,
        }
    }

    fn node_index(label: &str, property: &str) -> IndexDefinition {
        index(IndexTarget::NodeProperties {
            label: label.into(),
            properties: vec![property.into()],
        })
    }

    fn sample(owners: u64, distinct: u64, frequency: u64) -> IndexDistribution {
        IndexDistribution {
            indexed_owners: owners,
            ndistinct: CardinalityEstimate::exact(distinct),
            max_frequency: CardinalityEstimate::exact(frequency),
            max_observed_frequency: frequency.min(32),
            sampled_owners: owners.min(32) as usize,
            generation_id: 1,
            generation_anchor: HashId::from_bytes([0; 32]),
            definition_hash: HashId::from_bytes([1; 32]),
        }
    }

    fn advice(owners: u64, distinct: u64, frequency: u64) -> HashKeyAdvice {
        key_advice(
            &step(vec![part("a", &["A"])]),
            &key("a", Some("k"), 1),
            &node_index("A", "k"),
            sample(owners, distinct, frequency),
        )
        .unwrap()
    }

    #[test]
    fn providers_require_the_same_variable_property_and_index_owner_scope() {
        let step = step(vec![
            part("a", &["A"]),
            part("a", &["Other"]),
            part("b", &["A"]),
            part("a", &["A", "Extra"]),
        ]);
        let result = key_advice(
            &step,
            &key("a", Some("k"), 0b1111),
            &node_index("A", "k"),
            sample(100, 100, 1),
        )
        .unwrap();
        assert_eq!(result.providers, 0b1001);
        assert_eq!(result.ndistinct, CardinalityEstimate::exact(100));
        // Bit3 indicates coverage only, not a claim about the Extra subset.
        assert_eq!(result.indexed_owners, 100);
        assert!(
            key_advice(
                &step,
                &key("a", Some("x"), 1),
                &node_index("A", "k"),
                sample(100, 100, 1)
            )
            .is_none()
        );
        assert!(
            key_advice(
                &step,
                &key("a", Some("k"), 0b0010),
                &node_index("A", "k"),
                sample(100, 100, 1)
            )
            .is_none()
        );
    }

    #[test]
    fn endpoint_and_relationship_providers_keep_their_owner_kinds() {
        let mut relationship = part("start", &["Start"]);
        relationship.end = Some(node("a", &["A"]));
        relationship.relationship = Some(RelationshipSpec {
            variable: Some("r".into()),
            type_id: None,
            type_name: Some("R".into()),
            direction: Direction::Outgoing,
            index_seek: None,
            id_seek: None,
            impossible: false,
        });
        let step = step(vec![relationship]);
        assert!(
            key_advice(
                &step,
                &key("a", Some("k"), 1),
                &node_index("A", "k"),
                sample(10, 10, 1)
            )
            .is_some()
        );
        let index = index(IndexTarget::RelationshipProperties {
            relationship_type: "R".into(),
            properties: vec!["weight".into()],
        });
        assert!(
            key_advice(
                &step,
                &key("r", Some("weight"), 1),
                &index,
                sample(10, 10, 1)
            )
            .is_some()
        );
        assert!(
            key_advice(
                &step,
                &key("a", Some("weight"), 1),
                &index,
                sample(10, 10, 1)
            )
            .is_none()
        );
        assert!(key_advice(&step, &key("r", None, 1), &index, sample(10, 10, 1)).is_none());
    }

    #[test]
    fn composite_lookup_and_unproved_identity_domains_remain_unknown() {
        let step = step(vec![part("a", &["A"])]);
        let mut composite = node_index("A", "k");
        if let IndexTarget::NodeProperties { properties, .. } = &mut composite.target {
            properties.push("other".into());
        }
        assert!(
            key_advice(
                &step,
                &key("a", Some("k"), 1),
                &composite,
                sample(10, 10, 1)
            )
            .is_none()
        );
        assert!(
            key_advice(
                &step,
                &key("a", None, 1),
                &node_index("A", "k"),
                sample(10, 10, 1)
            )
            .is_none()
        );
        assert!(
            key_advice(
                &step,
                &key("a", Some("k"), 1),
                &index(IndexTarget::NodeLookup),
                sample(10, 10, 1)
            )
            .is_none()
        );
    }

    #[test]
    fn two_ready_ndvs_estimate_candidates_without_proving_overlap_or_null_ratio() {
        let left = advice(100, 100, 1);
        let right = advice(10_000, 10_000, 1);
        let rows = key_matches(
            Some(CardinalityEstimate::exact(100)),
            Some(CardinalityEstimate::exact(10_000)),
            Some(&left),
            Some(&right),
        )
        .unwrap();
        assert_eq!(
            rows,
            CardinalityEstimate {
                expected: 100,
                lower: 0,
                upper: 1_000_000
            }
        );
        assert_eq!(
            key_matches(
                Some(CardinalityEstimate::exact(10_000)),
                Some(CardinalityEstimate::exact(100)),
                Some(&right),
                Some(&left)
            ),
            Some(rows)
        );
    }

    #[test]
    fn unknown_ndv_uses_product_and_unknown_rows_never_become_zero() {
        let known = advice(100, 100, 1);
        let rows = Some(CardinalityEstimate::exact(100));
        for (left, right) in [(None, None), (Some(&known), None), (None, Some(&known))] {
            assert_eq!(
                key_matches(rows, rows, left, right).unwrap().expected,
                10_000
            );
        }
        assert!(key_matches(None, rows, None, None).is_none());
        assert!(key_matches(rows, None, Some(&known), None).is_none());
        assert_eq!(
            key_matches(None, Some(CardinalityEstimate::exact(0)), None, None),
            Some(CardinalityEstimate::exact(0))
        );
    }

    #[test]
    fn empty_index_does_not_prove_positive_inputs_have_no_candidates() {
        let empty = advice(0, 0, 0);
        assert_eq!(
            key_matches(
                Some(CardinalityEstimate::exact(20)),
                Some(CardinalityEstimate::exact(30)),
                Some(&empty),
                Some(&empty)
            )
            .unwrap(),
            CardinalityEstimate {
                expected: 600,
                lower: 0,
                upper: 600
            }
        );
    }

    #[test]
    fn wide_arithmetic_divides_before_saturating() {
        let unique = advice(u64::MAX, u64::MAX, 1);
        let rows = Some(CardinalityEstimate::exact(u64::MAX));
        assert_eq!(
            key_matches(rows, rows, Some(&unique), Some(&unique)).unwrap(),
            CardinalityEstimate {
                expected: u64::MAX,
                lower: 0,
                upper: u64::MAX
            }
        );
        let small = Some(CardinalityEstimate::exact(3));
        assert_eq!(
            key_matches(small, small, Some(&advice(2, 2, 1)), Some(&advice(2, 2, 1)))
                .unwrap()
                .expected,
            5
        );
    }

    #[test]
    fn retained_mcv_is_owner_advice_not_a_repeated_row_bucket_upper_bound() {
        let skew = advice(100, 10, 91);
        assert_eq!(skew.max_frequency, CardinalityEstimate::exact(91));
        assert_eq!(skew.max_observed_frequency, 32);
        let repeated = Some(CardinalityEstimate::exact(1_000));
        assert_eq!(
            key_matches(repeated, repeated, Some(&skew), Some(&skew)).unwrap(),
            CardinalityEstimate {
                expected: 100_000,
                lower: 0,
                upper: 1_000_000
            }
        );
    }

    #[test]
    fn inconsistent_derived_intervals_are_unknown() {
        let step = step(vec![part("a", &["A"])]);
        let key = key("a", Some("k"), 1);
        let index = node_index("A", "k");
        for broken in [
            sample(10, 11, 1),
            sample(10, 10, 2),
            sample(0, 1, 0),
            sample(10, 0, 1),
        ] {
            assert!(key_advice(&step, &key, &index, broken).is_none());
        }
        let mut broken = sample(10, 10, 1);
        broken.ndistinct.lower = 11;
        assert!(key_advice(&step, &key, &index, broken).is_none());
    }

    #[test]
    fn schema_unique_bounds_k_for_both_hash_directions_without_proving_overlap() {
        let mut unique = advice(1000, 1000, 1);
        unique.unique = true;
        let repeated = advice(1024, 96, 342);
        let a = Some(CardinalityEstimate::exact(1000));
        let b = Some(CardinalityEstimate::exact(1024));
        let forward = key_matches(a, b, Some(&unique), Some(&repeated)).unwrap();
        let reverse = key_matches(b, a, Some(&repeated), Some(&unique)).unwrap();
        assert_eq!(forward, reverse);
        assert_eq!(forward.upper, 1024);
        assert_eq!(forward.lower, 0);
    }
}
