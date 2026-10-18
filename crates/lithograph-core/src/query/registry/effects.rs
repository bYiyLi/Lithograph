//! Procedure effects are explicit because READ procedures can perform host I/O.

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ProcedureEffects {
    pub(crate) external_io: bool,
    pub(crate) schema: bool,
    pub(crate) version: bool,
    pub(crate) maintenance: bool,
}

pub(crate) fn procedure_effects(name: &str) -> Option<ProcedureEffects> {
    let mut effects = ProcedureEffects::default();
    match name.to_ascii_lowercase().as_str() {
        "db.labels"
        | "db.propertykeys"
        | "db.relationshiptypes"
        | "lithograph.branch.list"
        | "lithograph.commit.get"
        | "lithograph.tag.list"
        | "lithograph.log"
        | "lithograph.diff"
        | "lithograph.merge.get"
        | "lithograph.merge.list"
        | "lithograph.merge.conflicts" => {}
        "db.index.fulltext.querynodes"
        | "db.index.fulltext.queryrelationships"
        | "db.index.semantic.querynodes"
        | "db.index.semantic.queryrelationships" => effects.external_io = true,
        "db.index.semantic.createnodeindex" | "db.index.semantic.createrelationshipindex" => {
            effects.external_io = true;
            effects.schema = true;
        }
        "db.index.semantic.rebuild" => {
            effects.external_io = true;
            effects.maintenance = true;
        }
        "lithograph.index.rebuild" | "lithograph.gc" => effects.maintenance = true,
        "lithograph.branch.create"
        | "lithograph.branch.checkout"
        | "lithograph.branch.delete"
        | "lithograph.commit.create"
        | "lithograph.commit.data.set"
        | "lithograph.commit.data.clear"
        | "lithograph.tag.create"
        | "lithograph.tag.move"
        | "lithograph.tag.delete"
        | "lithograph.patch.apply"
        | "lithograph.merge.start"
        | "lithograph.merge.resolve"
        | "lithograph.merge.finalize"
        | "lithograph.merge.abort"
        | "lithograph.rebase"
        | "lithograph.squash"
        | "lithograph.reset"
        | "lithograph.revert" => effects.version = true,
        _ => return None,
    }
    Some(effects)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_registered_procedure_has_an_explicit_effect_classification() {
        let procedures = super::super::procedures().collect::<Vec<_>>();
        assert_eq!(procedures.len(), 38);
        for procedure in procedures {
            assert!(
                super::procedure_effects(procedure.name).is_some(),
                "{}",
                procedure.name
            );
        }
        assert!(super::procedure_effects("unknown.procedure").is_none());
    }
}
