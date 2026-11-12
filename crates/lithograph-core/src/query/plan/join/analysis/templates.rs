use crate::storage::{IndexTarget, StandardIndexKind};

use super::types::Key;
use super::*;

impl Analysis<'_, '_, '_, '_> {
    pub(super) fn shared_nodes(&mut self) -> QueryResult<bool> {
        // BTreeMap keys are borrowed. One small borrowed-key vector makes
        // mutation of the output independent of the variable table borrow.
        if !self.work(
            1,
            self.variables
                .len()
                .saturating_mul(std::mem::size_of::<&str>()),
        )? {
            return Ok(false);
        }
        let names = self.variables.keys().copied().collect::<Vec<_>>();
        for name in names {
            if !self.shared_node_templates(name)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn shared_node_templates(&mut self, name: &str) -> QueryResult<bool> {
        if !self.work(1, name.len().saturating_mul(64))? {
            return Ok(false);
        }
        let variable = &self.variables[name];
        if variable.kind != Kind::Node || variable.providers.0.count_ones() < 2 {
            return Ok(true);
        }
        let providers = variable.providers;
        let count = variable.occurrences.len();
        if !self.provider_edges(providers, providers)? {
            return Ok(false);
        }
        for index in 0..count {
            if !self.work(1, name.len().saturating_mul(64))? {
                return Ok(false);
            }
            let target = self.variables[name].occurrences[index];
            let source = without(providers, target.part);
            if source.0 == 0 {
                continue;
            }
            let source_variable = self.owned_name(name)?;
            if !self.push_template(target, source, ProbeAccess::Adjacency { source_variable })? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn equality_templates(
        &mut self,
        expression: &Expr,
        predicate: usize,
    ) -> QueryResult<bool> {
        let Expr::Binary(BinaryOp::Equal, left, right) = expression else {
            return Ok(true);
        };
        let Some(left) = self.operand(left)? else {
            return Ok(false);
        };
        let Some(right) = self.operand(right)? else {
            return Ok(false);
        };
        if left.providers.0 == 0 || right.providers.0 == 0 {
            return Ok(true);
        }
        if !self.provider_edges(left.providers, right.providers)? {
            return Ok(false);
        }
        Ok(self.hash_equality(predicate, left, right)?
            && self.direction_templates(left, right)?
            && self.direction_templates(right, left)?)
    }

    fn direction_templates(
        &mut self,
        target: Operand<'_>,
        source: Operand<'_>,
    ) -> QueryResult<bool> {
        if target.family == Family::Null || source.family == Family::Null {
            return Ok(true);
        }
        match (target.key, source.key) {
            (Some(Key::Entity(target_name)), Some(Key::Entity(source_name))) => {
                if !self.work(1, target_name.len().saturating_mul(64))? {
                    return Ok(false);
                }
                let count = self.variables[target_name].occurrences.len();
                for index in 0..count {
                    if !self.work(1, target_name.len().saturating_mul(64))? {
                        return Ok(false);
                    }
                    let occurrence = self.variables[target_name].occurrences[index];
                    let source_guard = without(source.providers, occurrence.part);
                    if source_guard.0 != 0 {
                        let name = self.owned_name(source_name)?;
                        if !self.push_template(
                            occurrence,
                            source_guard,
                            ProbeAccess::Identity {
                                source_variable: name,
                            },
                        )? {
                            return Ok(false);
                        }
                    }
                }
                Ok(true)
            }
            (
                Some(Key::Property { variable, property }),
                Some(Key::Property {
                    variable: source_variable,
                    property: source_property,
                }),
            ) => self.property_templates(
                variable,
                property,
                source_variable,
                source_property,
                target,
                source,
            ),
            _ => Ok(true),
        }
    }

    fn property_templates(
        &mut self,
        variable: &str,
        property: &str,
        source_variable: &str,
        source_property: &str,
        target: Operand<'_>,
        source: Operand<'_>,
    ) -> QueryResult<bool> {
        if !self.work(1, variable.len().saturating_mul(64))? {
            return Ok(false);
        }
        let count = self.variables[variable].occurrences.len();
        for ordinal in 0..count {
            if !self.work(1, variable.len().saturating_mul(64))? {
                return Ok(false);
            }
            let occurrence = self.variables[variable].occurrences[ordinal];
            let source_guard = without(source.providers, occurrence.part);
            if source_guard.0 == 0
                || self.scope_family(occurrence.scope, property)? != Some(target.family)
            {
                continue;
            }
            if !self.occurrence_indexes(
                occurrence,
                source_guard,
                property,
                source_variable,
                source_property,
                target.family,
            )? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn occurrence_indexes(
        &mut self,
        occurrence: Occurrence<'_>,
        source_guard: PartSet,
        property: &str,
        source_variable: &str,
        source_property: &str,
        family: Family,
    ) -> QueryResult<bool> {
        for index in self.context.schema.indexes.values() {
            let comparison_bytes = index_comparison_bytes(index, occurrence.scope, property);
            if !self.work(1, comparison_bytes)? {
                return Ok(false);
            }
            if !index_matches(index, occurrence.scope, property, family) {
                continue;
            }
            if !self.cache_allowed()? {
                return Ok(!self.budget.exhausted());
            }
            let Some(definition_hash) = crate::query::schema::join_index_definition_hash(
                index,
                self.context.control,
                self.budget,
            )?
            else {
                if self.budget.exhausted() {
                    return Ok(false);
                }
                continue;
            };
            let source_variable = self.owned_name(source_variable)?;
            let source_property = self.owned_name(source_property)?;
            let index_name = self.owned_name(&index.name)?;
            if !self.push_template(
                occurrence,
                source_guard,
                ProbeAccess::IndexEquality {
                    source_variable,
                    source_property,
                    index_name,
                    definition_hash,
                    kind: index.kind,
                },
            )? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn cache_allowed(&mut self) -> QueryResult<bool> {
        if let Some(allowed) = self.cache_allowed {
            return Ok(allowed);
        }
        let allowed = crate::query::schema::join_index_cache_allowed(
            self.context.snapshot,
            self.context.control,
            self.budget,
        )?;
        self.cache_allowed = Some(allowed);
        Ok(allowed)
    }

    fn owned_name(&mut self, name: &str) -> QueryResult<String> {
        if !self.work(1, name.len())? {
            return Ok(String::new());
        }
        Ok(name.to_owned())
    }

    fn push_template(
        &mut self,
        occurrence: Occurrence<'_>,
        source_guard: PartSet,
        access: ProbeAccess,
    ) -> QueryResult<bool> {
        if !self.work(1, 2 * std::mem::size_of::<ParameterTemplate>())? {
            return Ok(false);
        }
        self.templates.push(ParameterTemplate {
            part: occurrence.part,
            target: occurrence.target,
            source_guard,
            access,
        });
        Ok(true)
    }

    fn add_edge(&mut self, left: PartSet, right: PartSet) -> QueryResult<bool> {
        if !self.work(1, 2 * std::mem::size_of::<Hyperedge>())? {
            return Ok(false);
        }
        self.edges.push(Hyperedge { left, right });
        Ok(true)
    }

    /// Provider groups are alternatives, whereas a hyperedge endpoint means
    /// all its Parts. Each sufficient provider pair is a real simple edge.
    fn provider_edges(&mut self, left: PartSet, right: PartSet) -> QueryResult<bool> {
        let mut left_bits = left.0;
        while left_bits != 0 {
            let a = left_bits.trailing_zeros() as usize;
            left_bits &= left_bits - 1;
            let mut right_bits = right.0;
            while right_bits != 0 {
                let b = right_bits.trailing_zeros() as usize;
                right_bits &= right_bits - 1;
                if !self.work(1, 0)? {
                    return Ok(false);
                }
                if a == b || (left == right && a > b) {
                    continue;
                }
                if !self.add_edge(bit(a.min(b)), bit(a.max(b)))? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}

fn index_comparison_bytes(
    index: &crate::storage::IndexDefinition,
    scope: variables::Scope<'_>,
    property: &str,
) -> usize {
    match &index.target {
        IndexTarget::NodeProperties { label, properties } => {
            let labels = match scope {
                variables::Scope::Node(labels) => labels.len(),
                _ => 0,
            };
            label.len().saturating_mul(labels).saturating_add(
                properties
                    .first()
                    .map_or(0, String::len)
                    .saturating_add(property.len()),
            )
        }
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => relationship_type
            .len()
            .saturating_add(properties.first().map_or(0, String::len))
            .saturating_add(property.len()),
        _ => 0,
    }
}

fn index_matches(
    index: &crate::storage::IndexDefinition,
    scope: variables::Scope<'_>,
    property: &str,
    family: Family,
) -> bool {
    let property_matches = match (&index.target, scope) {
        (IndexTarget::NodeProperties { label, properties }, variables::Scope::Node(labels)) => {
            labels.contains(label) && properties.as_slice() == [property]
        }
        (
            IndexTarget::RelationshipProperties {
                relationship_type,
                properties,
            },
            variables::Scope::Relationship(Some(name)),
        ) => relationship_type == name && properties.as_slice() == [property],
        _ => false,
    };
    property_matches
        && match index.kind {
            StandardIndexKind::Range => {
                // Total equality and an executable parameterized Index key
                // are separate proofs. BoundRead currently consumes only
                // these scalar families; other total joins retain scans.
                matches!(family, Family::Boolean | Family::Number | Family::String)
            }
            StandardIndexKind::Text => family == Family::String,
            _ => false,
        }
}
