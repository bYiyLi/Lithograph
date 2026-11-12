use super::*;
use crate::query::plan::{NodeSpec, PatternPart};
use crate::query::{ExecutionOptions, QueryErrorKind, QueryResources};
use crate::storage::{
    ConstraintDefinition, IndexDefinition, IndexTarget, PropertyRule, SchemaState,
    StandardIndexKind,
};

fn part(variable: &str, labels: &[&str]) -> PatternPart {
    PatternPart {
        path_variable: None,
        relationship: None,
        end: None,
        start: NodeSpec {
            variable: Some(variable.into()),
            labels: vec![],
            label_names: labels.iter().map(|s| (*s).into()).collect(),
            scan_label: None,
            scan_label_name: None,
            index_seek: None,
            id_seek: None,
            impossible: false,
        },
    }
}

fn step(parts: Vec<PatternPart>, predicate: Expr) -> MatchStep {
    MatchStep {
        clause_start: 0,
        optional: false,
        parts,
        predicate: Some(predicate),
        early_predicates: vec![],
        join: None,
    }
}

fn property(variable: &str) -> Expr {
    Expr::Property(Box::new(Expr::Variable(variable.into())), "k".into())
}

fn equality(left: Expr, right: Expr) -> Expr {
    Expr::Binary(BinaryOp::Equal, Box::new(left), Box::new(right))
}

fn typed(schema: &mut SchemaState, label: &str, property_type: PropertyType, index: bool) {
    schema.constraints.insert(
        label.into(),
        ConstraintDefinition {
            name: label.into(),
            target: SchemaTarget::Node {
                label: label.into(),
            },
            properties: vec!["k".into()],
            kind: ConstraintDefinitionKind::Type {
                rule: PropertyRule {
                    property_type,
                    required: false,
                },
            },
            origin: None,
        },
    );
    if index {
        schema.indexes.insert(
            label.into(),
            IndexDefinition {
                name: label.into(),
                kind: StandardIndexKind::Range,
                target: IndexTarget::NodeProperties {
                    label: label.into(),
                    properties: vec!["k".into()],
                },
                owning_constraint: None,
                labels_or_types: vec![],
                additional_properties: vec![],
                configuration: None,
            },
        );
    }
}

fn analyze(
    step: &MatchStep,
    schema: &SchemaState,
    incoming: &[String],
    options: ExecutionOptions,
    params: &BTreeMap<String, Value>,
    budget: &mut PlanningBudget,
    interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<JoinAnalysis>> {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
    crate::storage::create_storage_schema(&db).unwrap();
    let root = crate::storage::initialize_root(&db).unwrap().root;
    let snapshot = Snapshot::resolve(&db, root).unwrap();
    let resources = QueryResources::default();
    let control = crate::query::ExecutionControl::new(&resources, interrupted);
    let view = ResolvedGraphView::default().with_query_access(options.access.read.query);
    let scope = JoinClauseScope {
        clause_start: 0,
        snapshot_identity: root,
        incoming,
        declared_columns: &[],
    };
    let context = JoinPlanningContext {
        snapshot: &snapshot,
        schema,
        view: &view,
        params,
        control: &control,
    };
    analyze_join(step, &scope, &context, budget)
}

fn ordinary(step: &MatchStep, schema: &SchemaState) -> Option<JoinAnalysis> {
    analyze(
        step,
        schema,
        &[],
        ExecutionOptions::default(),
        &BTreeMap::new(),
        &mut PlanningBudget::default(),
        &|| false,
    )
    .unwrap()
}

#[test]
fn typed_property_provider_is_not_an_earlier_untyped_binding() {
    let mut schema = SchemaState::default();
    typed(&mut schema, "Typed", PropertyType::Integer, true);
    typed(&mut schema, "M", PropertyType::Integer, true);
    let step = step(
        vec![part("n", &[]), part("m", &["M"]), part("n", &["Typed"])],
        equality(property("n"), property("m")),
    );
    let analysis = ordinary(&step, &schema).unwrap();
    assert_eq!(analysis.predicates[0].guards, vec![PartSet(4), PartSet(2)]);
    assert_eq!(analysis.hashes.len(), 1);
    assert_eq!(analysis.hashes[0].left.providers, PartSet(4));
    assert_eq!(analysis.hashes[0].right.providers, PartSet(2));
    let property_templates = analysis
        .templates
        .iter()
        .filter(|template| matches!(template.access, ProbeAccess::IndexEquality { .. }))
        .collect::<Vec<_>>();
    assert_eq!(property_templates.len(), 2);
    assert!(
        property_templates
            .iter()
            .any(|template| template.part == 1 && template.source_guard == PartSet(4))
    );
    assert!(
        property_templates
            .iter()
            .any(|template| template.part == 2 && template.source_guard == PartSet(2))
    );
    assert!(
        !property_templates
            .iter()
            .any(|template| template.part == 0 || template.source_guard == PartSet(1))
    );
}

#[test]
fn any_typed_provider_is_sufficient_but_the_inner_is_never_a_source() {
    let mut schema = SchemaState::default();
    typed(&mut schema, "N", PropertyType::Integer, true);
    typed(&mut schema, "M", PropertyType::Float, true);
    let step = step(
        vec![part("n", &["N"]), part("m", &["M"]), part("n", &["N"])],
        equality(property("n"), property("m")),
    );
    let analysis = ordinary(&step, &schema).unwrap();
    assert_eq!(analysis.predicates[0].guards, vec![PartSet(5), PartSet(2)]);
    assert_eq!(analysis.hashes.len(), 1);
    assert_eq!(analysis.hashes[0].left.providers, PartSet(5));
    assert_eq!(analysis.hashes[0].right.providers, PartSet(2));
    assert!(analysis.templates.iter().any(|template| template.part == 1
        && template.source_guard == PartSet(5)
        && matches!(template.access, ProbeAccess::IndexEquality { .. })));
    for template in analysis.templates {
        assert_eq!(template.source_guard.0 & bit(template.part).0, 0);
    }
}

#[test]
fn shared_node_identity_produces_adjacency_without_a_where_equality() {
    let step = step(
        vec![part("n", &[]), part("n", &[])],
        Expr::Literal(Value::Boolean(true)),
    );
    let analysis = ordinary(&step, &SchemaState::default()).unwrap();
    assert_eq!(analysis.templates.len(), 2);
    for template in analysis.templates {
        assert!(matches!(template.access, ProbeAccess::Adjacency { .. }));
        assert_eq!(template.source_guard, bit(1 - template.part));
    }
}

#[test]
fn alternative_providers_are_pairwise_topology_edges_not_all_of_endpoints() {
    let step = step(
        vec![part("n", &[]), part("n", &[]), part("n", &[])],
        Expr::Literal(Value::Boolean(true)),
    );
    let analysis = ordinary(&step, &SchemaState::default()).unwrap();
    let edges = analysis
        .edges
        .iter()
        .map(|edge| (edge.left.0, edge.right.0))
        .collect::<Vec<_>>();
    assert_eq!(edges, vec![(1, 2), (1, 4), (2, 4)]);
    for template in analysis.templates {
        assert_eq!(template.source_guard.0.count_ones(), 2);
    }
}

#[test]
fn unsupported_residual_rejects_the_complete_analysis() {
    let mut schema = SchemaState::default();
    typed(&mut schema, "N", PropertyType::Integer, true);
    typed(&mut schema, "M", PropertyType::Integer, true);
    let equal = equality(property("n"), property("m"));
    for residual in [
        Expr::Binary(
            BinaryOp::Divide,
            Box::new(Expr::Literal(Value::Integer(1))),
            Box::new(Expr::Literal(Value::Integer(0))),
        ),
        Expr::Binary(
            BinaryOp::Or,
            Box::new(Expr::Literal(Value::Boolean(true))),
            Box::new(Expr::Literal(Value::Boolean(false))),
        ),
        Expr::Unary(UnaryOp::Negative, Box::new(Expr::Literal(Value::Null))),
    ] {
        let step = step(
            vec![part("n", &["N"]), part("m", &["M"])],
            Expr::Binary(BinaryOp::And, Box::new(equal.clone()), Box::new(residual)),
        );
        assert!(ordinary(&step, &schema).is_none());
    }
}

#[test]
fn optional_incoming_and_path_keep_the_canonical_scope() {
    let mut step = step(
        vec![part("n", &[]), part("m", &[])],
        Expr::Literal(Value::Boolean(true)),
    );
    assert!(
        analyze(
            &step,
            &SchemaState::default(),
            &["n".into()],
            ExecutionOptions::default(),
            &BTreeMap::new(),
            &mut PlanningBudget::default(),
            &|| false
        )
        .unwrap()
        .is_none()
    );
    step.optional = true;
    assert!(ordinary(&step, &SchemaState::default()).is_none());
    step.optional = false;
    step.parts[0].path_variable = Some("p".into());
    assert!(ordinary(&step, &SchemaState::default()).is_none());
}

#[test]
fn masking_proves_null_without_using_canonical_property_type_or_index() {
    let mut schema = SchemaState::default();
    typed(&mut schema, "N", PropertyType::Integer, true);
    typed(&mut schema, "M", PropertyType::String, true);
    let step = step(
        vec![part("n", &["N"]), part("m", &["M"])],
        equality(property("n"), property("m")),
    );
    assert!(ordinary(&step, &schema).is_none());
    let options =
        ExecutionOptions::parse_text(r#"{"readAccess":{"query":{"denyProperties":["k"]}}}"#)
            .unwrap();
    let analysis = analyze(
        &step,
        &schema,
        &[],
        options,
        &BTreeMap::new(),
        &mut PlanningBudget::default(),
        &|| false,
    )
    .unwrap()
    .unwrap();
    assert!(analysis.templates.is_empty());
    assert!(analysis.hashes.is_empty());
}

#[test]
fn incompatible_last_in_member_and_nested_types_do_not_form_a_proof() {
    let mut schema = SchemaState::default();
    typed(&mut schema, "N", PropertyType::Integer, false);
    let mut values = (0..32)
        .map(|i| Expr::Literal(Value::Integer(i)))
        .collect::<Vec<_>>();
    values.push(Expr::Literal(Value::String("bad last member".into())));
    let candidate = step(
        vec![part("n", &["N"]), part("m", &[])],
        Expr::Binary(
            BinaryOp::In,
            Box::new(property("n")),
            Box::new(Expr::List(values)),
        ),
    );
    assert!(ordinary(&candidate, &schema).is_none());
    for property_type in [
        PropertyType::List {
            element: Box::new(PropertyType::Integer),
        },
        PropertyType::Vector {
            coordinate: "FLOAT64".into(),
            dimension: 3,
        },
    ] {
        typed(&mut schema, "N", property_type.clone(), false);
        typed(&mut schema, "M", property_type, false);
        let equal = step(
            vec![part("n", &["N"]), part("m", &["M"])],
            equality(property("n"), property("m")),
        );
        assert!(ordinary(&equal, &schema).is_none());
    }
}

#[test]
fn dynamic_index_templates_match_the_actual_bound_consumer_families() {
    for (property_type, kind, supported) in [
        (PropertyType::Boolean, StandardIndexKind::Range, true),
        (PropertyType::Integer, StandardIndexKind::Range, true),
        (PropertyType::Float, StandardIndexKind::Range, true),
        (PropertyType::String, StandardIndexKind::Range, true),
        (PropertyType::String, StandardIndexKind::Text, true),
        (PropertyType::Date, StandardIndexKind::Range, false),
        (PropertyType::LocalTime, StandardIndexKind::Range, false),
        (PropertyType::ZonedTime, StandardIndexKind::Range, false),
        (PropertyType::LocalDateTime, StandardIndexKind::Range, false),
        (PropertyType::ZonedDateTime, StandardIndexKind::Range, false),
        (PropertyType::Duration, StandardIndexKind::Range, false),
        (PropertyType::Uuid, StandardIndexKind::Range, false),
        (PropertyType::Point, StandardIndexKind::Range, false),
        (PropertyType::Point, StandardIndexKind::Point, false),
    ] {
        let mut schema = SchemaState::default();
        typed(&mut schema, "N", property_type.clone(), true);
        typed(&mut schema, "M", property_type.clone(), false);
        schema.indexes.get_mut("N").unwrap().kind = kind;
        let step = step(
            vec![part("n", &["N"]), part("m", &["M"])],
            equality(property("n"), property("m")),
        );
        let analysis =
            ordinary(&step, &schema).expect("the complete scalar comparison remains total");
        let has_probe = analysis
            .templates
            .iter()
            .any(|template| matches!(template.access, ProbeAccess::IndexEquality { .. }));
        assert_eq!(has_probe, supported, "{property_type:?} {kind:?}");
        assert_eq!(analysis.hashes.len(), usize::from(supported));
        assert_eq!(analysis.predicates.len(), 1);
        assert_eq!(analysis.edges.len(), 1);
    }
}

#[test]
fn hash_five_families_are_proven_without_any_index() {
    for (property_type, family) in [
        (PropertyType::Boolean, HashFamily::Boolean),
        (
            PropertyType::Union {
                members: vec![PropertyType::Integer, PropertyType::Float],
            },
            HashFamily::Number,
        ),
        (PropertyType::String, HashFamily::String),
    ] {
        let mut schema = SchemaState::default();
        typed(&mut schema, "N", property_type.clone(), false);
        typed(&mut schema, "M", property_type, false);
        let step = step(
            vec![part("n", &["N"]), part("m", &["M"])],
            Expr::Binary(
                BinaryOp::And,
                Box::new(Expr::Literal(Value::Boolean(true))),
                Box::new(equality(property("n"), property("m"))),
            ),
        );
        let analysis = ordinary(&step, &schema).unwrap();
        assert!(schema.indexes.is_empty());
        assert!(analysis.templates.is_empty());
        assert_eq!(analysis.hashes.len(), 1);
        let hash = &analysis.hashes[0];
        assert_eq!(hash.family, family);
        assert_eq!(hash.predicate, 1);
        assert_eq!(hash.left.variable, "n");
        assert_eq!(hash.right.variable, "m");
        assert_eq!(hash.left.property.as_deref(), Some("k"));
        assert_eq!(hash.left.providers, PartSet(1));
        assert_eq!(hash.right.providers, PartSet(2));
    }
    let entity_step = step(
        vec![part("n", &[]), part("m", &[])],
        equality(Expr::Variable("n".into()), Expr::Variable("m".into())),
    );
    let analysis = ordinary(&entity_step, &SchemaState::default()).unwrap();
    assert_eq!(analysis.hashes[0].family, HashFamily::NodeIdentity);
    assert!(analysis.hashes[0].left.property.is_none());

    let mut relationship_step = step(
        vec![part("a", &[]), part("b", &[])],
        equality(Expr::Variable("r".into()), Expr::Variable("s".into())),
    );
    for (part, variable) in relationship_step.parts.iter_mut().zip(["r", "s"]) {
        part.relationship = Some(crate::query::plan::RelationshipSpec {
            variable: Some(variable.into()),
            type_id: None,
            type_name: Some("E".into()),
            direction: crate::query::plan::Direction::Outgoing,
            index_seek: None,
            id_seek: None,
            impossible: false,
        });
        part.end = Some(super::tests::part("end", &[]).start);
    }
    let analysis = ordinary(&relationship_step, &SchemaState::default()).unwrap();
    assert_eq!(analysis.hashes.len(), 1);
    assert_eq!(analysis.hashes[0].family, HashFamily::RelationshipIdentity);
    assert!(analysis.hashes[0].left.property.is_none());
}

#[test]
fn union_numeric_is_total_and_budget_exhaustion_is_optional_but_cancel_is_not() {
    let mut schema = SchemaState::default();
    typed(
        &mut schema,
        "N",
        PropertyType::Union {
            members: vec![PropertyType::Integer, PropertyType::Float],
        },
        false,
    );
    typed(&mut schema, "M", PropertyType::Integer, false);
    let step = step(
        vec![part("n", &["N"]), part("m", &["M"])],
        equality(property("n"), property("m")),
    );
    assert!(ordinary(&step, &schema).is_some());
    let mut budget = PlanningBudget::default();
    assert!(budget.work(4090, 0));
    assert!(
        analyze(
            &step,
            &schema,
            &[],
            ExecutionOptions::default(),
            &BTreeMap::new(),
            &mut budget,
            &|| false
        )
        .unwrap()
        .is_none()
    );
    assert!(budget.exhausted());
    assert_eq!(
        analyze(
            &step,
            &schema,
            &[],
            ExecutionOptions::default(),
            &BTreeMap::new(),
            &mut PlanningBudget::default(),
            &|| true
        )
        .unwrap_err()
        .kind,
        QueryErrorKind::Interrupted
    );
}
