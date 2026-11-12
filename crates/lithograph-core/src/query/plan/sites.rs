//! Private PROFILE identities; physical depth is never an original Part id.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ProfileRole {
    NodeAccess,
    RelationshipAccess,
    Join,
    EarlyFilter,
    FactorizedAccumulate,
    ASPJoin,
    Flatten,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct OperatorSite {
    pub(crate) clause_start: usize,
    pub(crate) part: Option<usize>,
    pub(crate) role: ProfileRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProfileBinding {
    pub(crate) site: OperatorSite,
    pub(crate) physical_slot: usize,
}

impl super::PreparedQuery {
    pub(super) fn install_match_profile_sites(&mut self) -> super::QueryResult<()> {
        if self.mode != crate::cypher::ExecutionMode::Profile {
            return Ok(());
        }
        let program = self.program.as_ref();
        let write = self.write.as_ref();
        let matches = &self.matches;
        let physical = &self.physical;
        let (joined, sites) = if let Some(program) = program {
            (
                program.match_plans.values().any(|step| step.join.is_some()),
                program.profile_sites.as_slice(),
            )
        } else if let Some(write) = write {
            (
                write
                    .matches_for_explain
                    .iter()
                    .any(|step| step.join.is_some()),
                write.profile_sites.as_slice(),
            )
        } else {
            (
                matches.iter().any(|step| step.join.is_some()),
                self.profile_sites.as_slice(),
            )
        };
        let exact = sites.iter().copied().any(|binding| {
            if joined {
                return true;
            }
            let step = if let Some(program) = program {
                program.match_plans.get(&binding.site.clause_start)
            } else {
                write
                    .map(|write| write.matches_for_explain.as_slice())
                    .unwrap_or(matches)
                    .iter()
                    .find(|step| step.clause_start == binding.site.clause_start)
            };
            let identity = program.is_some_and(|program| {
                program
                    .identity_match_spans
                    .contains(&binding.site.clause_start)
            });
            bound_point_binding(binding, physical, step, identity)
        });
        self.metrics
            .install_profile_sites(sites.iter().copied().filter(|_| exact))
    }
}

fn bound_point_binding(
    binding: ProfileBinding,
    physical: &super::PhysicalPlan,
    step: Option<&super::MatchStep>,
    identity: bool,
) -> bool {
    let Some(part) = binding.site.part else {
        return false;
    };
    if binding.site.role != ProfileRole::NodeAccess
        || !matches!(
            physical.operators.get(binding.physical_slot),
            Some(super::PhysicalOperator::NodeIdSeek { .. })
        )
    {
        return false;
    }
    // The generic AST mapper emits this boundary only for an incoming Node.
    // Temporary identity-display steps are removed after planning; their
    // explicit permission still distinguishes a static WHERE-based seek.
    step.map_or(!identity, |step| {
        step.parts
            .get(part)
            .is_some_and(|part| part.relationship.is_none() && part.start.id_seek.is_none())
    })
}
