//! Prepare-time effects, including expressions inside atomic interpolation nodes.

use crate::cypher::{AstKind, AstNode, ClauseKind, ExpressionKind};

use super::super::{QueryResult, registry, safety};

#[derive(Debug, Clone, Default)]
pub(crate) struct ProgramEffects {
    pub(crate) load_csv: bool,
    pub(crate) semantic_external_io: bool,
    pub(crate) semantic_maintenance: bool,
    pub(crate) version_procedures: Vec<String>,
}

impl ProgramEffects {
    pub(super) fn collect(root: &AstNode) -> QueryResult<Self> {
        let mut effects = Self::default();
        effects.visit(root)?;
        Ok(effects)
    }

    fn visit(&mut self, root: &AstNode) -> QueryResult<()> {
        for node in root.descendants() {
            match node.kind {
                AstKind::Clause(ClauseKind::LoadCsv) => self.load_csv = true,
                AstKind::FunctionName => {
                    if let Some(name) = node.text.as_deref() {
                        self.record_procedure(name);
                    }
                }
                AstKind::Expression(ExpressionKind::InterpolatedString) => {
                    safety::validate_interpolation_with(node, |expression| self.visit(expression))?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn record_procedure(&mut self, name: &str) {
        self.semantic_external_io |= registry::is_semantic_direct_only(name);
        self.semantic_maintenance |= registry::is_semantic_maintenance(name);
        if registry::is_version_procedure(name) {
            self.version_procedures.push(name.to_ascii_lowercase());
        }
    }

    pub(crate) fn has_external_io(&self) -> bool {
        self.load_csv || self.semantic_external_io
    }

    pub(super) fn has_version_procedure(&self, name: &str) -> bool {
        self.version_procedures.iter().any(|value| value == name)
    }
}
