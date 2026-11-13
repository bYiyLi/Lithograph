//! One bounded degree request per physical cohort in this Join collection.
//! Candidate costing reads this map; only collection can touch the Snapshot.

use super::*;
use crate::storage::{LabelId, RelationshipTypeId};

mod endpoints;

type Cohort = (Option<LabelId>, u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DegreeKey {
    relationship_type: Option<RelationshipTypeId>,
    incoming: Option<bool>,
    label: Option<LabelId>,
    count: u64,
}

#[derive(Debug, Default)]
pub(super) struct DegreeSamples {
    values: BTreeMap<DegreeKey, Option<CardinalityEstimate>>,
    source_cohorts: BTreeMap<usize, Cohort>,
    endpoint_values: BTreeMap<endpoints::EndpointKey, Option<CardinalityEstimate>>,
}

impl DegreeSamples {
    pub(super) fn source_cohort(&self, template: usize) -> Option<Cohort> {
        self.source_cohorts.get(&template).copied()
    }

    pub(super) fn source_probe(
        &self,
        step: &MatchStep,
        template: &ParameterTemplate,
    ) -> Option<CardinalityEstimate> {
        self.endpoint_values
            .get(&endpoints::key(step, template)?)
            .copied()
            .flatten()
    }

    pub(super) fn existing(
        &self,
        part: &PatternPart,
        reversed: bool,
        context: &JoinPlanningContext<'_, '_, '_>,
        statistics: &PlannerStatistics,
    ) -> Option<CardinalityEstimate> {
        let (target, cohort) = existing_cohort(part, reversed, context, statistics)?;
        self.probe(part, target, Some(cohort))
    }

    pub(super) fn probe(
        &self,
        part: &PatternPart,
        target: ProbeTarget,
        domain: Option<Cohort>,
    ) -> Option<CardinalityEstimate> {
        let key = degree_key(part, target, domain?)?;
        self.values.get(&key).copied().flatten()
    }

    fn request(
        &mut self,
        part: &PatternPart,
        target: ProbeTarget,
        cohort: Cohort,
        context: &JoinPlanningContext<'_, '_, '_>,
        budget: &mut PlanningBudget,
        headroom: usize,
    ) -> QueryResult<()> {
        let Some(key) = degree_key(part, target, cohort) else {
            return Ok(());
        };
        if self.values.contains_key(&key) || !admit(budget, headroom, 1, 128) {
            return Ok(());
        }
        context.control.work(1)?;
        // sample_degree owns the existing 512-work / 16-KiB scratch admission.
        // Retain None too: an exhausted or unknown sample is never re-read.
        let sample =
            super::collect::sample_degree(part, target, Some(cohort), context, budget, headroom)?;
        self.values.insert(key, sample);
        Ok(())
    }
}

pub(super) fn collect_degree_samples(
    step: &MatchStep,
    analysis: &JoinAnalysis,
    context: &JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    budget: &mut PlanningBudget,
    headroom: usize,
) -> QueryResult<DegreeSamples> {
    context.control.check()?;
    let mut samples = DegreeSamples::default();
    if step.parts.len() > crate::query::plan::budget::MAX_PARTS {
        return Ok(samples);
    }
    for part in &step.parts {
        context.control.work(1)?;
        for reversed in [false, true] {
            if let Some((target, cohort)) = existing_cohort(part, reversed, context, statistics) {
                samples.request(part, target, cohort, context, budget, headroom)?;
            }
        }
    }
    collect_template_samples(
        step,
        analysis,
        context,
        statistics,
        budget,
        headroom,
        &mut samples,
    )?;
    Ok(samples)
}

fn collect_template_samples(
    step: &MatchStep,
    analysis: &JoinAnalysis,
    context: &JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    budget: &mut PlanningBudget,
    headroom: usize,
    samples: &mut DegreeSamples,
) -> QueryResult<()> {
    for (id, template) in analysis.templates.iter().enumerate() {
        let Some(part) = step.parts.get(template.part) else {
            continue;
        };
        if part.relationship.is_none() {
            continue;
        }
        if let Some(cohort) = source_cohort(step, template, context, statistics, budget, headroom)?
            && admit(budget, headroom, 1, 64)
        {
            samples.source_cohorts.insert(id, cohort);
            samples.request(part, template.target, cohort, context, budget, headroom)?;
            if cohort.1 > 32 {
                endpoints::collect(
                    step,
                    template,
                    context,
                    statistics,
                    budget,
                    headroom,
                    &mut samples.endpoint_values,
                )?;
            }
        }
    }
    Ok(())
}

fn degree_key(part: &PatternPart, target: ProbeTarget, cohort: Cohort) -> Option<DegreeKey> {
    let rel = part.relationship.as_ref()?;
    if rel.impossible
        || cohort.1 == 0
        || cohort.1 > 32
        || target == ProbeTarget::Relationship
        || (target == ProbeTarget::EndNode && part.end.is_none())
    {
        return None;
    }
    let reversed = target == ProbeTarget::EndNode;
    let incoming = match (rel.direction, reversed) {
        (Direction::Outgoing, false) | (Direction::Incoming, true) => Some(false),
        (Direction::Incoming, false) | (Direction::Outgoing, true) => Some(true),
        (Direction::Undirected, _) => None,
    };
    Some(DegreeKey {
        relationship_type: rel.type_id,
        incoming,
        label: cohort.0,
        count: cohort.1,
    })
}

fn existing_cohort(
    part: &PatternPart,
    reversed: bool,
    context: &JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
) -> Option<(ProbeTarget, Cohort)> {
    let rel = part.relationship.as_ref()?;
    // These producers bypass start-node scanning in the real PartCursor.
    if rel.id_seek.is_some() || rel.index_seek.is_some() {
        return None;
    }
    let (target, node) = if reversed {
        (ProbeTarget::EndNode, part.end.as_ref()?)
    } else {
        (ProbeTarget::StartNode, &part.start)
    };
    Some((target, node_cohort(node, context, statistics)?))
}

fn node_cohort(
    node: &NodeSpec,
    context: &JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
) -> Option<Cohort> {
    if node.impossible
        || node.id_seek.is_some()
        || context.view.is_empty()
        || node
            .index_seek
            .as_ref()
            .is_some_and(|seek| seek.kind != crate::storage::StandardIndexKind::Lookup)
    {
        return None;
    }
    // A logical min over several Labels is not the physical sampled cohort.
    // An identity/property subset is not the first N owners of its scan Label.
    let label = node.scan_label.or_else(|| context.view.scan_label());
    let count = match label {
        Some(label) => statistics.label_count(label)?,
        None => statistics.node_count,
    };
    // Keep a complete large cohort as a descriptor for the parent's existing
    // Type-total fallback. degree_key separately refuses to sample it.
    (count > 0).then_some((label, count))
}

fn source_cohort(
    step: &MatchStep,
    template: &ParameterTemplate,
    context: &JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    budget: &mut PlanningBudget,
    headroom: usize,
) -> QueryResult<Option<Cohort>> {
    let ProbeAccess::Adjacency { source_variable } = &template.access else {
        return Ok(None);
    };
    // At most two borrowed NodeSpecs per Part; charge before inspecting them.
    if !admit(budget, headroom, step.parts.len().saturating_mul(2), 0) {
        return Ok(None);
    }
    let mut cohort = None;
    for (id, part) in step.parts.iter().enumerate() {
        if template.source_guard.0 & (1_u128 << id) == 0 {
            continue;
        }
        for node in std::iter::once(&part.start).chain(part.end.iter()) {
            context.control.work(1)?;
            if node.variable.as_ref() != Some(source_variable) {
                continue;
            }
            let Some(candidate) = node_cohort(node, context, statistics) else {
                return Ok(None);
            };
            // Any provider can supply this binding. Different Labels/counts
            // cannot be collapsed into a fabricated small union cohort.
            if cohort.is_some_and(|old| old != candidate) {
                return Ok(None);
            }
            cohort = Some(candidate);
        }
    }
    Ok(cohort)
}

fn admit(budget: &mut PlanningBudget, headroom: usize, work: usize, bytes: usize) -> bool {
    if work.saturating_add(headroom)
        > crate::query::plan::budget::MAX_STATES.saturating_sub(budget.states)
        || bytes > crate::query::plan::budget::MAX_BYTES.saturating_sub(budget.bytes)
    {
        return false;
    }
    budget.work(work, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{ExecutionOptions, QueryCursor, QueryResources, prepare};
    use rusqlite::Connection;

    fn run(db: &Connection, text: &str) {
        let prepared = prepare(db, text, BTreeMap::new(), ExecutionOptions::default()).unwrap();
        let mut cursor = QueryCursor::new(prepared);
        while !cursor.next_batch(db, 32).unwrap().done {}
        cursor.complete(db).unwrap();
    }

    fn with_fixture(
        test: impl FnOnce(
            &MatchStep,
            &JoinAnalysis,
            &JoinPlanningContext<'_, '_, '_>,
            &PlannerStatistics,
        ),
    ) {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
        crate::storage::create_storage_schema(&db).unwrap();
        crate::storage::initialize_root(&db).unwrap();
        run(&db, "CREATE (:Rare)-[:R]->(),(:Other)-[:S]->() FINISH");
        run(
            &db,
            "UNWIND range(0,39) AS i CREATE (:Noise)-[:R]->() FINISH",
        );
        crate::storage::create_checkpoint(&db, crate::storage::branch_head(&db, "main").unwrap())
            .unwrap();
        let prepared = prepare(
            &db,
            "MATCH(a:Rare)-[r:R]->(b),(a)-[s:R]->(c) RETURN 0",
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .unwrap();
        let mut step = prepared.matches[0].clone();
        let mut reversed = step.parts[0].clone();
        let end = reversed.end.as_mut().unwrap();
        std::mem::swap(&mut reversed.start, end);
        reversed.relationship.as_mut().unwrap().direction = Direction::Incoming;
        step.parts.push(reversed);
        let template = ParameterTemplate {
            part: 1,
            target: ProbeTarget::StartNode,
            source_guard: PartSet(1),
            access: ProbeAccess::Adjacency {
                source_variable: "a".into(),
            },
        };
        let analysis = JoinAnalysis {
            hashes: Vec::new(),
            predicates: Vec::new(),
            edges: Vec::new(),
            templates: vec![template.clone(), template],
        };
        let snapshot =
            Snapshot::from_resolved_state(&db, prepared.resolved_state.as_ref().unwrap());
        let schema = snapshot.schema_state().unwrap();
        let labels = ["Rare", "Other", "Noise"]
            .map(|name| crate::storage::find_label(&db, name).unwrap().unwrap())
            .into_iter()
            .collect();
        let types = ["R", "S"]
            .map(|name| {
                crate::storage::find_relationship_type(&db, name)
                    .unwrap()
                    .unwrap()
            })
            .into_iter()
            .collect();
        let statistics =
            PlannerStatistics::bounded(&snapshot, &labels, &types, true, 4096).unwrap();
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let context = JoinPlanningContext {
            snapshot: &snapshot,
            schema: &schema,
            view: &prepared.graph_view,
            params: &prepared.params,
            control: &control,
        };
        test(&step, &analysis, &context, &statistics);
        assert_eq!(resources.used(), 0, "only bounded scratch leases were used");
    }

    #[test]
    fn existing_orientations_and_duplicate_probes_share_one_real_sample() {
        with_fixture(|step, analysis, context, statistics| {
            let mut budget = PlanningBudget::default();
            let samples =
                collect_degree_samples(step, analysis, context, statistics, &mut budget, 64)
                    .unwrap();
            assert_eq!(samples.values.len(), 1);
            assert_eq!(samples.source_cohorts.len(), 2);
            assert_eq!(budget.states, 512 + 1 + 2 * (step.parts.len() * 2 + 1));
            assert_eq!(budget.bytes, 16 * 1024 + 128 + 2 * 64);
            let edge = step.parts[0]
                .relationship
                .as_ref()
                .unwrap()
                .type_id
                .unwrap();
            assert_eq!(
                statistics.type_counts[&edge], 41,
                "global R total is not Rare degree"
            );
            let expected = Some(expectation(1, 1));
            for _ in 0..100 {
                assert_eq!(
                    samples.existing(&step.parts[0], false, context, statistics),
                    expected
                );
                assert_eq!(
                    samples.existing(&step.parts[2], true, context, statistics),
                    expected
                );
                assert_eq!(
                    samples.existing(&step.parts[0], true, context, statistics),
                    None
                );
                assert_eq!(
                    samples.probe(
                        &step.parts[1],
                        ProbeTarget::StartNode,
                        samples.source_cohort(0)
                    ),
                    expected
                );
            }
        });
    }

    #[test]
    fn directed_type_and_count_keys_do_not_alias_and_incident_orientations_do() {
        with_fixture(|step, _, _, _| {
            let original = &step.parts[0];
            let cohort = (Some(1), 1);
            let written = degree_key(original, ProbeTarget::StartNode, cohort);
            assert_eq!(
                written,
                degree_key(&step.parts[2], ProbeTarget::EndNode, cohort)
            );
            assert_ne!(written, degree_key(original, ProbeTarget::EndNode, cohort));
            assert_ne!(
                written,
                degree_key(original, ProbeTarget::StartNode, (cohort.0, 2))
            );
            let mut other = original.clone();
            other.relationship.as_mut().unwrap().type_id = None;
            assert_ne!(written, degree_key(&other, ProbeTarget::StartNode, cohort));
            other.relationship.as_mut().unwrap().direction = Direction::Undirected;
            assert_eq!(
                degree_key(&other, ProbeTarget::StartNode, cohort),
                degree_key(&other, ProbeTarget::EndNode, cohort)
            );
        });
    }

    #[test]
    fn physical_cohort_is_not_a_point_seek_property_subset_or_label_intersection() {
        with_fixture(|step, _, context, statistics| {
            let mut node = step.parts[0].start.clone();
            let rare = node.scan_label.unwrap();
            assert_eq!(
                node_cohort(&node, context, statistics),
                Some((Some(rare), 1))
            );
            node.id_seek = Some(1);
            assert!(node_cohort(&node, context, statistics).is_none());
            node.id_seek = None;
            node.index_seek = Some(crate::query::schema::StandardIndexSeek {
                index_name: "subset".into(),
                kind: crate::storage::StandardIndexKind::Range,
                predicates: Vec::new(),
            });
            assert!(node_cohort(&node, context, statistics).is_none());
            node.index_seek = None;
            let noise =
                crate::storage::find_label(context.snapshot.connection_for_query(), "Noise")
                    .unwrap()
                    .unwrap();
            node.labels.push(noise);
            node.scan_label = Some(noise);
            let broad = node_cohort(&node, context, statistics).unwrap();
            assert_eq!(broad, (Some(noise), 40));
            assert!(degree_key(&step.parts[0], ProbeTarget::StartNode, broad).is_none());
            let mut unknown = statistics.clone();
            unknown.label_counts.remove(&noise);
            assert!(node_cohort(&node, context, &unknown).is_none());
            node.scan_label = Some(rare);
            unknown.label_counts.insert(rare, 0);
            assert!(node_cohort(&node, context, &unknown).is_none());
            unknown.label_counts.insert(rare, 33);
            let broad = node_cohort(&node, context, &unknown).unwrap();
            assert_eq!(broad, (Some(rare), 33));
            assert!(degree_key(&step.parts[0], ProbeTarget::StartNode, broad).is_none());
        });
    }

    #[test]
    fn differing_any_provider_cohorts_do_not_reuse_one_providers_cached_sample() {
        with_fixture(|step, analysis, context, statistics| {
            let mut step = step.clone();
            let other =
                crate::storage::find_label(context.snapshot.connection_for_query(), "Other")
                    .unwrap()
                    .unwrap();
            step.parts[2].end.as_mut().unwrap().scan_label = Some(other);
            step.parts.push(step.parts[2].clone());
            let mut analysis = JoinAnalysis {
                hashes: Vec::new(),
                predicates: Vec::new(),
                edges: Vec::new(),
                templates: analysis.templates.clone(),
            };
            for template in &mut analysis.templates {
                template.source_guard = PartSet(5);
            }
            let mut budget = PlanningBudget::default();
            let samples =
                collect_degree_samples(&step, &analysis, context, statistics, &mut budget, 64)
                    .unwrap();
            assert_eq!(samples.values.len(), 2);
            assert!(
                budget.states < 3 * 512,
                "the repeated None did not re-sample"
            );
            assert!(
                samples
                    .existing(&step.parts[0], false, context, statistics)
                    .is_some()
            );
            assert!(samples.source_cohort(0).is_none());
            assert!(
                samples
                    .probe(
                        &step.parts[1],
                        ProbeTarget::StartNode,
                        samples.source_cohort(0)
                    )
                    .is_none()
            );
            assert!(
                samples.values.values().any(Option::is_none),
                "zero-edge or unknown samples are retained once"
            );
        });
    }

    #[test]
    fn headroom_refuses_optional_sampling_without_exhausting_the_query_budget() {
        with_fixture(|step, analysis, context, statistics| {
            let mut budget = PlanningBudget::default();
            budget.states = crate::query::plan::budget::MAX_STATES - 64;
            let before = budget.states;
            let samples =
                collect_degree_samples(step, analysis, context, statistics, &mut budget, 64)
                    .unwrap();
            assert!(samples.values.is_empty());
            assert!(samples.source_cohorts.is_empty());
            assert_eq!(budget.states, before);
            assert!(!budget.exhausted());
        });
    }

    #[test]
    fn node_only_adjacency_points_do_not_collect_or_charge_degree_requests() {
        with_fixture(|step, analysis, context, statistics| {
            let mut step = step.clone();
            for part in &mut step.parts {
                part.relationship = None;
                part.end = None;
            }
            let mut budget = PlanningBudget::default();
            let samples =
                collect_degree_samples(&step, analysis, context, statistics, &mut budget, 64)
                    .unwrap();
            assert!(samples.values.is_empty());
            assert!(samples.source_cohorts.is_empty());
            assert_eq!((budget.states, budget.bytes), (0, 0));
        });
    }

    #[test]
    fn complete_large_source_cohort_is_retained_without_a_sampling_grant() {
        with_fixture(|step, analysis, context, statistics| {
            let mut step = step.clone();
            step.parts[1].start.variable = Some("b".into());
            let analysis = JoinAnalysis {
                hashes: Vec::new(),
                predicates: Vec::new(),
                edges: Vec::new(),
                templates: analysis
                    .templates
                    .iter()
                    .cloned()
                    .map(|mut template| {
                        template.access = ProbeAccess::Adjacency {
                            source_variable: "b".into(),
                        };
                        template
                    })
                    .collect(),
            };
            let mut budget = PlanningBudget::default();
            let samples =
                collect_degree_samples(&step, &analysis, context, statistics, &mut budget, 64)
                    .unwrap();
            assert_eq!(
                samples.source_cohort(0),
                Some((None, statistics.node_count))
            );
            assert!(statistics.node_count > 32);
            assert!(
                samples
                    .probe(
                        &step.parts[1],
                        ProbeTarget::StartNode,
                        samples.source_cohort(0)
                    )
                    .is_none()
            );
            assert_eq!(
                samples.values.len(),
                1,
                "only the separate Rare cohort was sampled"
            );
            assert_eq!(budget.states, 512 + 1 + 2 * (step.parts.len() * 2 + 1));
        });
    }

    #[test]
    fn exhaustive_skew_uses_mean_for_expected_and_maximum_for_one_outer_bound() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT,database_id TEXT,storage_format INTEGER)").unwrap();
        crate::storage::create_storage_schema(&db).unwrap();
        crate::storage::initialize_root(&db).unwrap();
        run(&db, "UNWIND range(0,3) AS i CREATE (:Cohort{k:i}) FINISH");
        run(
            &db,
            "MATCH(h:Cohort) WHERE h.k=3 UNWIND range(0,7) AS i CREATE (h)-[:R]->(:Sink) FINISH",
        );
        crate::storage::create_checkpoint(&db, crate::storage::branch_head(&db, "main").unwrap())
            .unwrap();
        let prepared = prepare(
            &db,
            "MATCH(a:Cohort)-[r:R]->(b),(a)-[s:R]->(c) RETURN 0",
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .unwrap();
        let step = &prepared.matches[0];
        let analysis = JoinAnalysis {
            hashes: Vec::new(),
            predicates: Vec::new(),
            edges: Vec::new(),
            templates: vec![ParameterTemplate {
                part: 1,
                target: ProbeTarget::StartNode,
                source_guard: PartSet(1),
                access: ProbeAccess::Adjacency {
                    source_variable: "a".into(),
                },
            }],
        };
        let snapshot =
            Snapshot::from_resolved_state(&db, prepared.resolved_state.as_ref().unwrap());
        let schema = snapshot.schema_state().unwrap();
        let label = crate::storage::find_label(&db, "Cohort").unwrap().unwrap();
        let edge = crate::storage::find_relationship_type(&db, "R")
            .unwrap()
            .unwrap();
        let sources = snapshot.scan_label_after(label, 0, 8).unwrap().items;
        let degrees = sources
            .iter()
            .map(|node| {
                snapshot
                    .scan_outgoing_page(*node, Some(edge), None, 16)
                    .unwrap()
                    .items
                    .len()
            })
            .collect::<Vec<_>>();
        assert_eq!(degrees, vec![0, 0, 0, 8], "real pinned cohort degrees");
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let context = JoinPlanningContext {
            snapshot: &snapshot,
            schema: &schema,
            view: &prepared.graph_view,
            params: &prepared.params,
            control: &control,
        };
        let samples = collect_degree_samples(
            step,
            &analysis,
            &context,
            &prepared.statistics,
            &mut PlanningBudget::default(),
            64,
        )
        .unwrap();
        let expected = Some(CardinalityEstimate {
            expected: 2,
            lower: 0,
            upper: 8,
        });
        assert_eq!(
            samples.existing(&step.parts[0], false, &context, &prepared.statistics),
            expected
        );
        assert_eq!(samples.source_cohort(0), Some((Some(label), 4)));
        assert_eq!(
            samples.probe(
                &step.parts[1],
                ProbeTarget::StartNode,
                samples.source_cohort(0)
            ),
            expected
        );
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn cancellation_is_propagated_before_optional_collection() {
        with_fixture(|step, analysis, context, statistics| {
            let control = ExecutionControl::new(context.control.resources(), &|| true);
            let cancelled = JoinPlanningContext {
                control: &control,
                ..*context
            };
            let error = collect_degree_samples(
                step,
                analysis,
                &cancelled,
                statistics,
                &mut PlanningBudget::default(),
                64,
            )
            .unwrap_err();
            assert_eq!(error.kind, QueryErrorKind::Interrupted);
        });
    }
}
