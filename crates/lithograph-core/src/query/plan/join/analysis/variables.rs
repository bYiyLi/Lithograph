use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Node,
    Relationship,
}

#[derive(Clone, Copy)]
pub(super) enum Scope<'a> {
    Node(&'a [String]),
    Relationship(Option<&'a str>),
}

#[derive(Clone, Copy)]
pub(super) struct Occurrence<'a> {
    pub(super) part: usize,
    pub(super) target: ProbeTarget,
    pub(super) scope: Scope<'a>,
}

pub(super) struct Variable<'a> {
    pub(super) kind: Kind,
    pub(super) providers: PartSet,
    pub(super) occurrences: Vec<Occurrence<'a>>,
}

impl<'a> Analysis<'a, '_, '_, '_> {
    pub(super) fn collect_variables(
        &mut self,
        step: &'a MatchStep,
        incoming: &[String],
    ) -> QueryResult<bool> {
        for (part_id, part) in step.parts.iter().enumerate() {
            // Path orientation/reuse and incoming runtime kinds retain the
            // canonical execution order until separately proved by the cursor.
            if part.path_variable.is_some() {
                return Ok(false);
            }
            let nodes = [
                (Some(&part.start), ProbeTarget::StartNode),
                (part.end.as_ref(), ProbeTarget::EndNode),
            ];
            for (node, target) in nodes {
                let Some(node) = node else {
                    continue;
                };
                if !self.insert_variable(
                    node.variable.as_deref(),
                    Kind::Node,
                    Occurrence {
                        part: part_id,
                        target,
                        scope: Scope::Node(&node.label_names),
                    },
                    incoming,
                )? {
                    return Ok(false);
                }
            }
            if let Some(rel) = &part.relationship
                && !self.insert_variable(
                    rel.variable.as_deref(),
                    Kind::Relationship,
                    Occurrence {
                        part: part_id,
                        target: ProbeTarget::Relationship,
                        scope: Scope::Relationship(rel.type_name.as_deref()),
                    },
                    incoming,
                )?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn insert_variable(
        &mut self,
        name: Option<&'a str>,
        kind: Kind,
        occurrence: Occurrence<'a>,
        incoming: &[String],
    ) -> QueryResult<bool> {
        let Some(name) = name else {
            return self.work(1, 0);
        };
        // Charge name comparisons before walking potentially long prefixes.
        for input in incoming {
            if !self.work(1, name.len().min(input.len()))? || input == name {
                return Ok(false);
            }
        }
        if !self.work(1, name.len().saturating_mul(64).saturating_add(256))? {
            return Ok(false);
        }
        let variable = self.variables.entry(name).or_insert_with(|| Variable {
            kind,
            providers: PartSet(0),
            occurrences: Vec::new(),
        });
        if variable.kind != kind || (kind == Kind::Relationship && !variable.occurrences.is_empty())
        {
            return Ok(false);
        }
        variable.providers.0 |= bit(occurrence.part).0;
        variable.occurrences.push(occurrence);
        Ok(true)
    }
}

pub(super) fn target_matches(target: &SchemaTarget, scope: Scope<'_>) -> bool {
    match (target, scope) {
        (SchemaTarget::Node { label }, Scope::Node(labels)) => labels.contains(label),
        (SchemaTarget::Relationship { relationship_type }, Scope::Relationship(Some(name))) => {
            relationship_type == name
        }
        _ => false,
    }
}
