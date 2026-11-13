//! Reuse the same pinned access advice as the selected binary MATCH.

use super::*;

impl JoinEstimates {
    pub(crate) fn multiway_possible(&self, parts: usize) -> bool {
        self.node_variables >= 3
            && self.relationship_parts >= 3
            && (self.relationship_parts < parts || parts >= self.node_variables)
    }

    pub(crate) fn multiway_output_work(&self) -> usize {
        self.parts.len().saturating_add(self.variables.len())
    }

    pub(crate) fn multiway_output_rows(&self) -> Option<u64> {
        let mask = if self.parts.len() == 128 {
            u128::MAX
        } else {
            (1_u128 << self.parts.len()) - 1
        };
        self.multiway_component_output_rows(mask)
    }

    pub(crate) fn multiway_component_output_rows(&self, mask: u128) -> Option<u64> {
        // Multiway emits relationship tuples before the original residual.
        // WHERE equality and filter selectivity cannot discount this work.
        Some(
            self.identity_selectivity(mask, self.raw_part_rows(mask)?)?
                .expected,
        )
    }

    /// Rows read by one typed adjacency cursor from its bound endpoint.
    /// This is advice, not an existence proof or a runtime pruning rule.
    pub(crate) fn multiway_degree(&self, part: usize, reversed: bool) -> Option<u64> {
        let pair = self.bound.get(part)?;
        let access = if reversed { pair.1 } else { pair.0 }?;
        // Local total predicates may reduce `rows` without reducing the
        // adjacency page reads. Keep the unfiltered physical access work.
        access.io?.checked_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(rows: u64) -> PartEstimate {
        let rows = CardinalityEstimate::exact(rows);
        PartEstimate {
            written: AccessEstimate::stream(Some(rows), 1),
            reversed: None,
            output: Some(rows),
        }
    }

    fn domains() -> Vec<VariableDomain> {
        vec![
            VariableDomain {
                providers: 3,
                count: Some(2),
            },
            VariableDomain {
                providers: 6,
                count: Some(3),
            },
            VariableDomain {
                providers: 5,
                count: Some(4),
            },
        ]
    }

    #[test]
    fn residual_selectivity_cannot_discount_intersection_or_relationship_enumeration() {
        let mut estimate = JoinEstimates {
            parts: vec![part(12), part(20), part(30)],
            variables: domains(),
            equalities: vec![EqualityDomain {
                left: 1,
                right: 2,
                distinct: 10,
                left_key: 0,
                right_key: 1,
            }],
            equality_keys: 2,
            filters: vec![LocalFilter {
                providers: 1,
                divisor: 100,
                empty: false,
            }],
            ..JoinEstimates::default()
        };
        assert_eq!(estimate.rows(7).unwrap().expected, 1);
        assert_eq!(estimate.multiway_output_rows(), Some(300));
        let mut access = AccessEstimate::stream(Some(CardinalityEstimate::exact(64)), 1);
        access.rows = Some(CardinalityEstimate::exact(1));
        estimate.bound.push((Some(access), None));
        assert_eq!(estimate.multiway_degree(0, false), Some(64));
        assert_eq!(estimate.multiway_degree(0, true), None);
    }

    #[test]
    fn false_residual_does_not_turn_unknown_physical_output_into_known_empty() {
        let mut unknown = part(1);
        unknown.output = None;
        let estimate = JoinEstimates {
            parts: vec![unknown],
            filters: vec![LocalFilter {
                providers: 1,
                divisor: 1,
                empty: true,
            }],
            ..JoinEstimates::default()
        };
        assert_eq!(estimate.multiway_output_rows(), None);
    }

    #[test]
    fn component_delivery_ignores_tail_multiplicity_and_keeps_shared_identity() {
        let estimate = JoinEstimates {
            parts: vec![part(12), part(20), part(30), part(1_000)],
            variables: domains(),
            ..JoinEstimates::default()
        };
        assert_eq!(estimate.multiway_component_output_rows(7), Some(300));
        assert_eq!(estimate.multiway_output_rows(), Some(300_000));
    }

    #[test]
    fn full_part_mask_does_not_shift_past_the_representation() {
        let estimate = JoinEstimates {
            parts: (0..128).map(|_| part(1)).collect(),
            ..JoinEstimates::default()
        };
        assert_eq!(estimate.multiway_output_rows(), Some(1));
    }
}
