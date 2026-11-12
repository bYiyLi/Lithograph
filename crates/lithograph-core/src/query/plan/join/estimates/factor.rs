//! Reuse collected access and subset cardinalities for executable group arms.
use super::*;
use crate::query::plan::factor::{FactorArm, FactorWork, FactorizedPlan};
use crate::query::plan::join::cost::Cost;

impl JoinEstimates {
    pub(crate) fn factor_advice_work(&self, plan: &JoinPlan) -> usize {
        plan.stages.len().saturating_mul(
            self.rows_work()
                .saturating_mul(3)
                .saturating_add(self.hashes.work())
                .saturating_add(16),
        )
    }

    pub(crate) fn factor_candidate_work(
        &self,
        original: &JoinPlan,
        plan: &FactorizedPlan,
    ) -> Option<FactorWork> {
        let old = self.selected_factor_cost(original)?;
        let a = self.factor_arm_work(original, &plan.accumulate, 0)?;
        let prefix = plan
            .accumulate
            .hops
            .iter()
            .fold(0, |mask, hop| mask | (1_u128 << hop.part));
        let b = self.factor_arm_work(original, &plan.build, prefix)?;
        let output = old.rows?.expected;
        let star = prefix
            | plan
                .build
                .hops
                .iter()
                .fold(0, |mask, hop| mask | (1_u128 << hop.part));
        // A plain Node identity join can filter the star; a NodeId cannot
        // produce two matching D rows. Do not let a heuristic equality domain
        // multiply its expected output above the same star boundary.
        let output = if plan.asp.is_some() {
            output.min(self.rows(star)?.expected)
        } else {
            output
        };
        Some(FactorWork {
            old: old.io?.checked_add(old.cpu?)?,
            output,
            arms: [a, b],
            point_work: self.factor_point_work(original, plan),
        })
    }

    fn factor_point_work(&self, original: &JoinPlan, plan: &FactorizedPlan) -> Option<u64> {
        let asp = plan.asp.as_ref()?;
        let (id, _) = original.templates.iter().enumerate().find(|(_, template)| {
            template.part == asp.part && template.target == ProbeTarget::StartNode
                && matches!(&template.access, ProbeAccess::Identity { source_variable } if source_variable == &asp.probe_variable)
        })?;
        let access = self.probes.get(id)?.as_ref()?;
        access.io?.checked_add(access.cpu?)
    }

    fn selected_factor_cost(&self, plan: &JoinPlan) -> Option<Cost> {
        let mut prefix = 0;
        let mut cost = Cost::root();
        for stage in &plan.stages {
            let next = prefix | (1_u128 << stage.part);
            cost = match stage.access {
                StageAccess::Hash {
                    equality,
                    inner_side,
                    dynamic_filter,
                } => {
                    let id = self.hashes.ids(stage.part).iter().copied().find(|id| {
                        let candidate = self.hashes.candidate(*id);
                        candidate.orientation == stage.orientation
                            && candidate.equality == equality
                            && candidate.inner_side == inner_side
                    })?;
                    if dynamic_filter.is_some() {
                        self.hashes
                            .dynamic_estimate(id, prefix, cost, self.rows(next))?
                            .1
                    } else {
                        self.hashes
                            .estimate(id, prefix, cost)?
                            .append(cost, self.rows(next))
                    }
                }
                access => {
                    cost.append(self.access(stage.part, stage.orientation, access, prefix)?)
                }
            };
            cost.rows = self.rows(next);
            prefix = next;
        }
        Some(cost)
    }

    fn factor_arm_work(
        &self,
        original: &JoinPlan,
        arm: &FactorArm,
        mut prefix: u128,
    ) -> Option<[u64; 3]> {
        let conditioning = if prefix == 0 {
            1
        } else {
            self.rows(prefix)?.expected.max(1)
        };
        let mut rows = 1_u64;
        let mut io = 0_u64;
        let mut cpu = 0_u64;
        for hop in &arm.hops {
            let choice = factor_access(original, *hop, prefix);
            let access = self.access(hop.part, hop.orientation, choice, prefix)?;
            io = io.checked_add(rows.checked_mul(access.io?)?)?;
            cpu = cpu.checked_add(rows.checked_mul(access.cpu?)?)?;
            prefix |= 1_u128 << hop.part;
            // The original order and group cursor cross the same logical
            // subset boundary. B is invoked for one qualified shared identity,
            // not once per duplicate A member retained in its table.
            rows = self.rows(prefix)?.expected.div_ceil(conditioning);
        }
        Some([rows, io, cpu])
    }
}

fn factor_access(plan: &JoinPlan, hop: crate::query::plan::FactorHop, prefix: u128) -> StageAccess {
    let Some(stage) = plan.stages.iter().find(|stage| stage.part == hop.part) else {
        return StageAccess::Existing;
    };
    if stage.orientation != hop.orientation {
        return StageAccess::Existing;
    }
    let StageAccess::Probe(id) = stage.access else {
        return StageAccess::Existing;
    };
    let Some(template) = plan.templates.get(id) else {
        return StageAccess::Existing;
    };
    let bound_start = matches!(
        (template.target, hop.orientation),
        (ProbeTarget::StartNode, PartOrientation::Written)
            | (ProbeTarget::EndNode, PartOrientation::Reversed)
    );
    if bound_start
        && template.source_guard.0 & prefix != 0
        && matches!(template.access, ProbeAccess::Adjacency { .. })
    {
        stage.access
    } else {
        StageAccess::Existing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::plan::FactorHop;

    fn access(rows: u64, io: u64) -> AccessEstimate {
        AccessEstimate {
            rows: Some(CardinalityEstimate::exact(rows)),
            io: Some(io),
            cpu: Some(io * 2),
            memory: 0,
        }
    }

    fn independent_plan() -> JoinPlan {
        JoinPlan {
            clause_start: 0,
            snapshot_identity: crate::storage::HashId::from_bytes([0; 32]),
            declared_columns: vec![],
            predicates: vec![],
            templates: vec![],
            hashes: vec![],
            stages: (0..4)
                .map(|part| JoinStage {
                    part,
                    orientation: PartOrientation::Written,
                    access: StageAccess::Existing,
                    cartesian: false,
                    predicates_after: vec![],
                })
                .collect(),
            factorized: None,
        }
    }

    fn arm(parts: [usize; 2]) -> FactorArm {
        FactorArm {
            hops: parts
                .into_iter()
                .map(|part| FactorHop {
                    part,
                    orientation: PartOrientation::Written,
                })
                .collect(),
        }
    }

    #[test]
    fn both_arms_use_the_same_conditional_prefix_cardinality_as_the_original_order() {
        let estimates = JoinEstimates {
            parts: (1..=4)
                .map(|io| PartEstimate {
                    written: access(128, io),
                    reversed: None,
                    output: Some(CardinalityEstimate::exact(128)),
                })
                .collect(),
            bindings: vec![(0, 0); 4],
            variables: vec![
                VariableDomain {
                    providers: 3,
                    count: Some(8192),
                },
                VariableDomain {
                    providers: 12,
                    count: Some(16384),
                },
                VariableDomain {
                    providers: 6,
                    count: Some(256),
                },
            ],
            ..Default::default()
        };
        let original = independent_plan();
        let left = estimates
            .factor_arm_work(&original, &arm([0, 1]), 0)
            .unwrap();
        assert_eq!(estimates.rows(3).unwrap().expected, 2);
        assert_eq!(left, [2, 257, 514]);
        let right = estimates
            .factor_arm_work(&original, &arm([2, 3]), 3)
            .unwrap();
        assert_eq!(estimates.rows(7).unwrap().expected, 1);
        assert_eq!(estimates.rows(15).unwrap().expected, 1);
        // U is visited once for the conditional B prefix, not 128 times.
        assert_eq!(right, [1, 7, 14]);
    }
}
