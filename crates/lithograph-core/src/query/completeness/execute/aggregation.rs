use super::*;

impl ReadExecutor<'_, '_> {
    pub(super) fn aggregate_projection(
        &mut self,
        input: RowSet,
        specs: &[ProjectionSpec],
        star: bool,
        group_by: Option<&AstNode>,
    ) -> QueryResult<Vec<ProjectedRow>> {
        if star {
            return Err(QueryError::semantic(
                "RETURN/WITH * cannot be combined with aggregation",
            ));
        }
        let plan = grouping_plan(specs, group_by)?;
        let mut groups = self.build_aggregate_groups(input.rows, specs, &plan)?;
        if groups.is_empty() && !has_grouping_keys(specs, &plan) {
            groups.insert("[]".to_owned(), Vec::new());
        }
        self.project_aggregate_groups(groups, specs)
    }

    pub(super) fn build_aggregate_groups(
        &mut self,
        rows: Vec<BindingRow>,
        specs: &[ProjectionSpec],
        plan: &GroupingPlan,
    ) -> QueryResult<BTreeMap<String, Vec<BindingRow>>> {
        let mut groups: BTreeMap<String, Vec<BindingRow>> = BTreeMap::new();
        for row in rows {
            self.control.work(1)?;
            let baseline = self.expression_reservation.bytes();
            let aliases = self.projection_alias_values(specs, &row)?;
            let values = self.grouping_values(specs, plan, &row, &aliases)?;
            let key_memory = helpers::reserve_distinct_key(&values, self.control)?;
            let key = distinct_row_key(&values, Some(self.control))?;
            drop(values);
            drop(aliases);
            self.expression_reservation.resize(baseline)?;
            self.insert_aggregate_group(&mut groups, row, key, key_memory)?;
        }
        Ok(groups)
    }

    pub(super) fn insert_aggregate_group(
        &mut self,
        groups: &mut BTreeMap<String, Vec<BindingRow>>,
        row: BindingRow,
        key: String,
        mut key_memory: MemoryReservation,
    ) -> QueryResult<()> {
        if !groups.contains_key(&key) {
            key_memory.resize(crate::query::resources::checked_add(
                key.capacity() as u64,
                192,
            )?)?;
            self.expression_reservation.merge(key_memory)?;
        }
        let group = groups.entry(key).or_default();
        if group.len() == group.capacity() {
            let additional = group.capacity().max(1);
            self.expression_reservation
                .grow(crate::query::resources::allocation_bytes::<BindingRow>(
                    additional,
                )?)?;
            group.try_reserve_exact(additional).map_err(|_| {
                crate::query::resources::memory_error("unable to allocate aggregate group")
            })?;
        }
        group.push(row);
        Ok(())
    }

    pub(super) fn grouping_values(
        &mut self,
        specs: &[ProjectionSpec],
        plan: &GroupingPlan,
        row: &BindingRow,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<Vec<Value>> {
        if !plan.has_group_by {
            return specs
                .iter()
                .filter(|spec| !spec.aggregate)
                .map(|spec| self.evaluate(&spec.expression, row))
                .collect();
        }
        plan.explicit_keys
            .iter()
            .map(|key| {
                if plan.group_by_all {
                    self.evaluate(key, row)
                } else {
                    self.evaluate_with_aliases(key, row, aliases)
                }
            })
            .collect()
    }

    pub(super) fn project_aggregate_groups(
        &mut self,
        groups: BTreeMap<String, Vec<BindingRow>>,
        specs: &[ProjectionSpec],
    ) -> QueryResult<Vec<ProjectedRow>> {
        self.expression_reservation
            .grow(crate::query::resources::allocation_bytes::<ProjectedRow>(
                groups.len(),
            )?)?;
        let mut projected = Vec::with_capacity(groups.len());
        for group in groups.into_values() {
            let empty = BindingRow::default();
            let representative = group.first().unwrap_or(&empty);
            let mut output = BindingRow::default();
            preserve_common_load_csv_context(&group, &mut output);
            for spec in specs {
                let value = self.evaluate_group_expression(
                    &spec.expression,
                    &group,
                    representative,
                    &BTreeMap::new(),
                )?;
                output.insert(
                    spec.column.clone(),
                    binding_from_value_controlled(&self.snapshot, value, self.control)?,
                );
            }
            projected.push(ProjectedRow { output, group });
        }
        Ok(projected)
    }

    pub(super) fn projection_alias_values(
        &mut self,
        specs: &[ProjectionSpec],
        row: &BindingRow,
    ) -> QueryResult<BTreeMap<String, Value>> {
        let mut aliases = BTreeMap::new();
        for spec in specs.iter().filter(|spec| !spec.aggregate) {
            self.expression_reservation
                .grow(crate::query::resources::checked_add(
                    crate::query::resources::map_entry_bytes::<Value>()?,
                    spec.column.capacity() as u64,
                )?)?;
            aliases.insert(spec.column.clone(), self.evaluate(&spec.expression, row)?);
        }
        Ok(aliases)
    }

    pub(super) fn finish_evaluation(
        &mut self,
        evaluated: expression::EvaluatedValue,
        baseline: u64,
    ) -> QueryResult<Value> {
        self.expression_reservation.merge(evaluated.reservation)?;
        let evaluated = self.take_evaluated_value(evaluated.value, baseline)?;
        self.expression_reservation.merge(evaluated.reservation)?;
        Ok(evaluated.value)
    }

    pub(super) fn evaluate_owned_with_aliases(
        &mut self,
        expression: &expression::Expr,
        row: &BindingRow,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<expression::EvaluatedValue> {
        let baseline = self.expression_reservation.bytes();
        let value = self.evaluate_with_aliases(expression, row, aliases)?;
        self.take_evaluated_value(value, baseline)
    }

    pub(super) fn take_evaluated_value(
        &mut self,
        value: Value,
        baseline: u64,
    ) -> QueryResult<expression::EvaluatedValue> {
        let bytes = expression::value_bytes_with_control(&value, Some(self.control))?;
        let reservation = self.expression_reservation.split(bytes)?;
        self.expression_reservation.resize(baseline)?;
        Ok(expression::EvaluatedValue { value, reservation })
    }

    pub(super) fn evaluate_with_aliases(
        &mut self,
        expression: &expression::Expr,
        row: &BindingRow,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<Value> {
        let baseline = self.expression_reservation.bytes();
        let (expression, _memory) = self.materialize_subqueries(expression, row, aliases)?;
        let evaluated = expression::evaluate_with_aliases_and_access_control(
            &expression,
            &self.snapshot,
            row,
            self.params,
            aliases,
            &self.graph_view.query_access,
            self.control,
        )?;
        drop(expression);
        drop(_memory);
        self.finish_evaluation(evaluated, baseline)
    }

    pub(super) fn evaluate_group_expression(
        &mut self,
        expression: &expression::Expr,
        group: &[BindingRow],
        representative: &BindingRow,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<Value> {
        let baseline = self.expression_reservation.bytes();
        let (expression, _memory) =
            self.materialize_group_expression(expression, group, representative, aliases)?;
        let evaluated = expression::evaluate_with_aliases_and_access_control(
            &expression,
            &self.snapshot,
            representative,
            self.params,
            aliases,
            &self.graph_view.query_access,
            self.control,
        )?;
        drop(expression);
        drop(_memory);
        self.finish_evaluation(evaluated, baseline)
    }

    fn evaluate_owned_group_expression(
        &mut self,
        expression: &expression::Expr,
        group: &[BindingRow],
        row: &BindingRow,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<expression::EvaluatedValue> {
        let baseline = self.expression_reservation.bytes();
        let value = self.evaluate_group_expression(expression, group, row, aliases)?;
        self.take_evaluated_value(value, baseline)
    }

    pub(super) fn materialize_group_expression(
        &mut self,
        value: &expression::Expr,
        group: &[BindingRow],
        representative: &BindingRow,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<(expression::Expr, MemoryReservation)> {
        self.transform_runtime_expression(value, &mut |executor, candidate| {
            if let Some(value) = graph_expression::evaluate_lazy_graph_expression(
                candidate,
                executor.control,
                &mut |nested| {
                    executor.evaluate_owned_group_expression(nested, group, representative, aliases)
                },
            )? {
                executor.expression_reservation.merge(value.reservation)?;
                return Ok(Some(expression::Expr::Literal(value.value)));
            }
            if let Some(value) = graph_expression::evaluate_scoped_graph_expression(
                candidate,
                aliases,
                executor.control,
                &mut |nested, locals| {
                    let (local, _memory) = graph_expression::scoped_binding_row(
                        &executor.snapshot,
                        representative,
                        locals,
                        executor.control,
                    )?;
                    executor.evaluate_owned_group_expression(nested, group, &local, locals)
                },
            )? {
                executor.expression_reservation.merge(value.reservation)?;
                return Ok(Some(expression::Expr::Literal(value.value)));
            }
            let replacement =
                executor.evaluate_group_candidate(candidate, group, representative, aliases)?;
            Ok(replacement.map(expression::Expr::Literal))
        })
    }

    fn evaluate_group_candidate(
        &mut self,
        candidate: &expression::Expr,
        group: &[BindingRow],
        representative: &BindingRow,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<Option<Value>> {
        use expression::Expr;
        let replacement = match candidate {
            Expr::Function {
                name,
                args,
                distinct,
                star,
            } if crate::query::registry::is_aggregating(name) => {
                Some(self.evaluate_aggregate(name, args, *distinct, *star, group)?)
            }
            _ => self.evaluate_graph_runtime_candidate(candidate, representative, aliases)?,
        };
        Ok(replacement)
    }

    pub(super) fn evaluate_aggregate(
        &mut self,
        name: &str,
        args: &[expression::Expr],
        distinct: bool,
        star: bool,
        group: &[BindingRow],
    ) -> QueryResult<Value> {
        let lower = name.to_ascii_lowercase();
        if lower == "count" && (star || args.is_empty()) {
            return Ok(Value::Integer(group.len() as i64));
        }
        let Some(argument) = args.first() else {
            return Err(QueryError::semantic(format!(
                "{name}() requires an argument"
            )));
        };
        let values = self.aggregate_values(argument, group, distinct)?;
        match lower.as_str() {
            "count" => Ok(Value::Integer(values.len() as i64)),
            "collect" | "collect_list" => Ok(Value::List(values)),
            "sum" => aggregate_sum(&values, self.control),
            "avg" => aggregate_average(&values, self.control),
            "min" => aggregate_extreme(&values, false, self.control),
            "max" => aggregate_extreme(&values, true, self.control),
            "percentilecont" | "percentile_cont" => {
                let percentile = self.aggregate_percentile_argument(name, args, group)?;
                aggregate_percentile(&values, percentile, true, self.control)
            }
            "percentiledisc" | "percentile_disc" => {
                let percentile = self.aggregate_percentile_argument(name, args, group)?;
                aggregate_percentile(&values, percentile, false, self.control)
            }
            "stdev" | "stdev_samp" => aggregate_deviation(&values, true, self.control),
            "stdevp" | "stdev_pop" => aggregate_deviation(&values, false, self.control),
            _ => Err(QueryError::internal(format!(
                "registered aggregate {name} has no evaluator"
            ))),
        }
    }

    pub(super) fn aggregate_values(
        &mut self,
        argument: &expression::Expr,
        group: &[BindingRow],
        distinct: bool,
    ) -> QueryResult<Vec<Value>> {
        self.expression_reservation
            .grow(crate::query::resources::allocation_bytes::<Value>(
                group.len(),
            )?)?;
        let mut values = Vec::with_capacity(group.len());
        for row in group {
            let value = self.evaluate(argument, row)?;
            if !matches!(value, Value::Null) {
                values.push(value);
            }
        }
        if distinct {
            values = self.distinct_aggregate_values(values)?;
        }
        Ok(values)
    }

    pub(super) fn distinct_aggregate_values(&self, values: Vec<Value>) -> QueryResult<Vec<Value>> {
        let mut seen = BTreeSet::new();
        let mut seen_memory = self
            .control
            .reserve(crate::query::resources::allocation_bytes::<Value>(
                values.len(),
            )?)?;
        let mut unique = Vec::with_capacity(values.len());
        for value in values {
            self.control.work(1)?;
            let mut key_memory =
                helpers::reserve_distinct_key(std::slice::from_ref(&value), self.control)?;
            let key = distinct_row_key(std::slice::from_ref(&value), Some(self.control))?;
            key_memory.resize(crate::query::resources::checked_add(
                key.capacity() as u64,
                128,
            )?)?;
            if seen.insert(key) {
                seen_memory.merge(key_memory)?;
                unique.push(value);
            }
        }
        Ok(unique)
    }

    pub(super) fn aggregate_percentile_argument(
        &mut self,
        name: &str,
        args: &[expression::Expr],
        group: &[BindingRow],
    ) -> QueryResult<Value> {
        if args.len() != 2 {
            return Err(QueryError::semantic(format!(
                "{name}() expects value and percentile arguments"
            )));
        }
        let empty = BindingRow::default();
        self.evaluate(&args[1], group.first().unwrap_or(&empty))
    }

    pub(super) fn evaluate_projected(
        &mut self,
        expression: &expression::Expr,
        record: &ProjectedRow,
    ) -> QueryResult<Value> {
        let mut aliases_memory = self.control.reserve(0)?;
        let mut aliases = BTreeMap::new();
        for (name, value) in &record.output.values {
            aliases_memory.grow(crate::query::resources::checked_add(
                crate::query::resources::map_entry_bytes::<Value>()?,
                name.capacity() as u64,
            )?)?;
            let evaluated = expression::binding_value_with_memory(
                &self.snapshot,
                value,
                &self.graph_view.query_access,
                self.control,
            )?;
            aliases_memory.merge(evaluated.reservation)?;
            aliases.insert(name.clone(), evaluated.value);
        }
        let representative = record.group.first().unwrap_or(&record.output);
        self.evaluate_group_expression(expression, &record.group, representative, &aliases)
    }
}
