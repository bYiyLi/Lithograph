use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Family {
    Null,
    Boolean,
    Number,
    String,
    Date,
    LocalTime,
    ZonedTime,
    LocalDateTime,
    ZonedDateTime,
    Duration,
    Point,
    Uuid,
    Node,
    Relationship,
}

#[derive(Clone, Copy)]
pub(super) struct Operand<'a> {
    pub(super) family: Family,
    /// One ready provider both binds the value and proves its family.
    pub(super) providers: PartSet,
    pub(super) key: Option<Key<'a>>,
}

#[derive(Clone, Copy)]
pub(super) enum Key<'a> {
    Entity(&'a str),
    Property {
        variable: &'a str,
        property: &'a str,
    },
}

impl Analysis<'_, '_, '_, '_> {
    pub(super) fn operand<'e>(&mut self, expression: &'e Expr) -> QueryResult<Option<Operand<'e>>> {
        if !self.work(1, lookup_bytes(expression))? {
            return Ok(None);
        }
        if matches!(expression, Expr::Parameter(_)) && !self.work(self.context.params.len(), 0)? {
            return Ok(None);
        }
        let value = match expression {
            Expr::Literal(value) => Some(value),
            Expr::Parameter(name) => self.context.params.get(name),
            _ => None,
        };
        if let Some(value) = value {
            return Ok(value_family(value).map(|family| Operand {
                family,
                providers: PartSet(0),
                key: None,
            }));
        }
        match expression {
            Expr::Variable(name) => Ok(self.variables.get(name.as_str()).map(|var| Operand {
                family: match var.kind {
                    Kind::Node => Family::Node,
                    Kind::Relationship => Family::Relationship,
                },
                providers: var.providers,
                key: Some(Key::Entity(name)),
            })),
            Expr::Property(base, property) => self.property_operand(base, property),
            _ => Ok(None),
        }
    }

    fn property_operand<'e>(
        &mut self,
        base: &'e Expr,
        property: &'e str,
    ) -> QueryResult<Option<Operand<'e>>> {
        let Expr::Variable(variable) = base else {
            return Ok(None);
        };
        let Some(binding) = self.variables.get(variable.as_str()) else {
            return Ok(None);
        };
        if self.context.view.query_access.denies(property) {
            return Ok(Some(Operand {
                family: Family::Null,
                providers: binding.providers,
                key: Some(Key::Property { variable, property }),
            }));
        }
        let count = binding.occurrences.len();
        let mut family = None;
        let mut providers = PartSet(0);
        // Borrowed copies avoid retaining a second variable/proof IR.
        for index in 0..count {
            if !self.work(1, variable.len().saturating_mul(64))? {
                return Ok(None);
            }
            let occurrence = self.variables[variable.as_str()].occurrences[index];
            if let Some(proven) = self.scope_family(occurrence.scope, property)? {
                if family.is_none() {
                    family = Some(proven);
                }
                if family == Some(proven) {
                    providers.0 |= bit(occurrence.part).0;
                }
            }
        }
        Ok(family.map(|family| Operand {
            family,
            providers,
            key: Some(Key::Property { variable, property }),
        }))
    }

    pub(super) fn access_providers(&mut self, expression: &Expr) -> QueryResult<Option<PartSet>> {
        if !self.work(1, lookup_bytes(expression))? {
            return Ok(None);
        }
        match expression {
            Expr::Property(base, _) => {
                let Expr::Variable(name) = base.as_ref() else {
                    return Ok(None);
                };
                Ok(self
                    .variables
                    .get(name.as_str())
                    .map(|variable| variable.providers))
            }
            _ => Ok(self.operand(expression)?.map(|operand| operand.providers)),
        }
    }

    pub(super) fn scope_family(
        &mut self,
        scope: variables::Scope<'_>,
        property: &str,
    ) -> QueryResult<Option<Family>> {
        for constraint in self.context.schema.constraints.values() {
            let name_bytes = constraint
                .properties
                .first()
                .map_or(0, String::len)
                .saturating_add(property.len());
            if !self.work(1, name_bytes)? {
                return Ok(None);
            }
            if constraint.properties.as_slice() != [property] {
                continue;
            }
            let scope_bytes = match (&constraint.target, scope) {
                (SchemaTarget::Node { label }, variables::Scope::Node(labels)) => {
                    label.len().saturating_mul(labels.len())
                }
                (SchemaTarget::Relationship { relationship_type }, _) => relationship_type.len(),
                _ => 0,
            };
            if !self.work(1, scope_bytes)? {
                return Ok(None);
            }
            if variables::target_matches(&constraint.target, scope)
                && let ConstraintDefinitionKind::Type { rule } = &constraint.kind
                && let Some(family) = self.property_family(&rule.property_type, 1)?
            {
                return Ok(Some(family));
            }
        }
        Ok(None)
    }

    fn property_family(
        &mut self,
        property_type: &PropertyType,
        depth: usize,
    ) -> QueryResult<Option<Family>> {
        if depth > MAX_DEPTH || !self.work(1, 0)? {
            return Ok(None);
        }
        let family = match property_type {
            PropertyType::Boolean => Family::Boolean,
            PropertyType::Integer | PropertyType::Float => Family::Number,
            PropertyType::String => Family::String,
            PropertyType::Date => Family::Date,
            PropertyType::LocalTime => Family::LocalTime,
            PropertyType::ZonedTime => Family::ZonedTime,
            PropertyType::LocalDateTime => Family::LocalDateTime,
            PropertyType::ZonedDateTime => Family::ZonedDateTime,
            PropertyType::Duration => Family::Duration,
            PropertyType::Point => Family::Point,
            PropertyType::Uuid => Family::Uuid,
            PropertyType::Union { members } => {
                let Some(first) = members.first() else {
                    return Ok(None);
                };
                let Some(family) = self.property_family(first, depth + 1)? else {
                    return Ok(None);
                };
                for member in &members[1..] {
                    if self.property_family(member, depth + 1)? != Some(family) {
                        return Ok(None);
                    }
                }
                family
            }
            // Nested collection/vector compatibility needs a recursive proof,
            // not merely a shared top-level discriminant.
            PropertyType::Any | PropertyType::List { .. } | PropertyType::Vector { .. } => {
                return Ok(None);
            }
        };
        Ok(Some(family))
    }
}

fn lookup_bytes(expression: &Expr) -> usize {
    match expression {
        Expr::Variable(name) | Expr::Parameter(name) => name.len().saturating_mul(64),
        Expr::Property(base, property) => {
            let variable = match base.as_ref() {
                Expr::Variable(name) => name.len(),
                _ => 0,
            };
            property.len().saturating_add(variable).saturating_mul(64)
        }
        _ => 0,
    }
}

pub(super) fn value_family(value: &Value) -> Option<Family> {
    Some(match value {
        Value::Null => Family::Null,
        Value::Boolean(_) => Family::Boolean,
        Value::Integer(_) | Value::Float(_) => Family::Number,
        Value::String(_) => Family::String,
        Value::Date(_) => Family::Date,
        Value::LocalTime(_) => Family::LocalTime,
        Value::Time(_) => Family::ZonedTime,
        Value::LocalDateTime(_) => Family::LocalDateTime,
        Value::ZonedDateTime(_) => Family::ZonedDateTime,
        Value::Duration(_) => Family::Duration,
        Value::Point(_) => Family::Point,
        Value::Uuid(_) => Family::Uuid,
        _ => return None,
    })
}

pub(super) fn compatible(left: Family, right: Family) -> bool {
    left == Family::Null || right == Family::Null || left == right
}
