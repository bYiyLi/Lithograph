use super::*;

impl Analysis<'_, '_, '_, '_> {
    pub(super) fn total_predicate(
        &mut self,
        expression: &Expr,
    ) -> QueryResult<Option<Vec<PartSet>>> {
        self.total_predicate_at(expression, 1)
    }

    fn total_predicate_at(
        &mut self,
        expression: &Expr,
        depth: usize,
    ) -> QueryResult<Option<Vec<PartSet>>> {
        if depth > MAX_DEPTH || !self.work(1, 2 * std::mem::size_of::<PartSet>())? {
            return Ok(None);
        }
        match expression {
            Expr::Binary(operator, left, right) => self.total_binary(*operator, left, right),
            Expr::IsNull { value, .. } => Ok(self
                .access_providers(value)?
                .map(|providers| guards(providers, PartSet(0)))),
            Expr::Unary(UnaryOp::Not, value) => self.total_predicate_at(value, depth + 1),
            _ => Ok(self.operand(expression)?.and_then(|operand| {
                matches!(operand.family, Family::Boolean | Family::Null)
                    .then(|| guards(operand.providers, PartSet(0)))
            })),
        }
    }

    fn total_binary(
        &mut self,
        operator: BinaryOp,
        left: &Expr,
        right: &Expr,
    ) -> QueryResult<Option<Vec<PartSet>>> {
        match operator {
            BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::Less
            | BinaryOp::LessEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterEqual => {
                let Some(left) = self.operand(left)? else {
                    return Ok(None);
                };
                let Some(right) = self.operand(right)? else {
                    return Ok(None);
                };
                Ok(types::compatible(left.family, right.family)
                    .then(|| guards(left.providers, right.providers)))
            }
            BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains => {
                let Some(left) = self.access_providers(left)? else {
                    return Ok(None);
                };
                Ok(self
                    .access_providers(right)?
                    .map(|right| guards(left, right)))
            }
            BinaryOp::In => self.total_in(left, right),
            // OR, functions, arithmetic and complex boolean trees preserve
            // their original evaluation sites, even behind a constant false.
            _ => Ok(None),
        }
    }

    fn total_in(&mut self, left: &Expr, right: &Expr) -> QueryResult<Option<Vec<PartSet>>> {
        let Some(providers) = self.access_providers(left)? else {
            return Ok(None);
        };
        match right {
            Expr::List(values) => {
                if values.is_empty() {
                    return Ok(Some(guards(providers, PartSet(0))));
                }
                let Some(left) = self.operand(left)? else {
                    return Ok(None);
                };
                for value in values {
                    let Some(right) = self.operand(value)? else {
                        return Ok(None);
                    };
                    // Keep the group representation small: IN collections
                    // contain constants only, never extra partial providers.
                    if right.providers.0 != 0 || !types::compatible(left.family, right.family) {
                        return Ok(None);
                    }
                }
                Ok(Some(guards(left.providers, PartSet(0))))
            }
            Expr::Literal(value) => self.total_in_value(left, value, providers),
            Expr::Parameter(name) => {
                if !self.work(
                    self.context.params.len().saturating_add(1),
                    name.len().saturating_mul(64),
                )? {
                    return Ok(None);
                }
                let Some(value) = self.context.params.get(name) else {
                    return Ok(None);
                };
                self.total_in_value(left, value, providers)
            }
            _ => Ok(None),
        }
    }

    fn total_in_value(
        &mut self,
        left: &Expr,
        value: &Value,
        providers: PartSet,
    ) -> QueryResult<Option<Vec<PartSet>>> {
        match value {
            Value::Null => Ok(Some(guards(providers, PartSet(0)))),
            Value::List(values) if values.is_empty() => Ok(Some(guards(providers, PartSet(0)))),
            Value::List(values) => {
                let Some(left) = self.operand(left)? else {
                    return Ok(None);
                };
                for value in values {
                    if !self.work(1, 0)?
                        || !types::value_family(value)
                            .is_some_and(|right| types::compatible(left.family, right))
                    {
                        return Ok(None);
                    }
                }
                Ok(Some(guards(left.providers, PartSet(0))))
            }
            _ => Ok(None),
        }
    }
}
