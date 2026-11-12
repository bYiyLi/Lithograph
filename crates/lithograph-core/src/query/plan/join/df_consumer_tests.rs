//! These tests force an already admitted DF candidate, independently of CBO.
//! They prove the real resident/Grace consumer and do not claim cost selection.

use std::collections::BTreeMap;

use crate::cypher::Value;
use crate::query::{
    ExecutionOptions, HashJoinRuntimeMetrics, OperatorRuntimeMetrics, QueryCursor, QueryError,
    prepare,
};
use crate::storage;
use rusqlite::Connection;

use super::{
    HashFamily, HashOperandSide, JoinPlan, JoinStage, PartOrientation, ProbeAccess, ProbeTarget,
    ResolvedGraphView, StageAccess,
};
use crate::query::plan::{PreparedQuery, build_logical, build_physical};
use crate::query::stream::QueryMetrics;

const PROBE_ROWS: usize = 1000;
const TARGETS: [usize; 3] = [101, 501, 901];
const JOIN: &str = "MATCH(p:DFProbe),(b:DFBuild) WHERE p.k=b.k \
    RETURN elementId(p) AS probe,elementId(b) AS build,p.k AS probeKey,b.k AS buildKey";

struct ReadScope<'a>(&'a Connection);

impl<'a> ReadScope<'a> {
    fn new(db: &'a Connection) -> Self {
        db.execute_batch("SAVEPOINT phase22_df_read").unwrap();
        let _: String = db
            .query_row(
                "SELECT database_id FROM main._lithograph_meta WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        Self(db)
    }
}

impl Drop for ReadScope<'_> {
    fn drop(&mut self) {
        self.0.execute_batch("RELEASE phase22_df_read").unwrap();
    }
}

#[derive(Debug)]
struct Observed {
    columns: Vec<String>,
    plan: String,
    rows: Vec<Vec<Value>>,
    profile: Option<Vec<OperatorRuntimeMetrics>>,
}

fn consume(
    db: &Connection,
    text: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
    batch_size: usize,
) -> Result<Observed, QueryError> {
    let prepared = prepare(db, text, params, options)?;
    drain(db, prepared, batch_size)
}

fn drain(
    db: &Connection,
    prepared: PreparedQuery,
    batch_size: usize,
) -> Result<Observed, QueryError> {
    let columns = prepared.columns.clone();
    let plan = prepared.physical.explain();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let result = collect_remaining(db, &mut cursor, columns, plan, Vec::new(), batch_size);
    drop(cursor);
    assert_eq!(
        resources.used(),
        0,
        "complete/error must release all leases"
    );
    if let Some(limit) = resources.limit() {
        assert!(
            resources.peak() <= limit,
            "query peak {} exceeds {limit}",
            resources.peak()
        );
    }
    result
}

fn collect_remaining(
    db: &Connection,
    cursor: &mut QueryCursor,
    columns: Vec<String>,
    plan: String,
    mut rows: Vec<Vec<Value>>,
    batch_size: usize,
) -> Result<Observed, QueryError> {
    loop {
        let batch = cursor.next_batch(db, batch_size)?;
        let done = batch.done;
        rows.extend(batch.rows);
        if done {
            let summary = cursor.complete(db)?;
            let profile = summary
                .metrics
                .operator_profile()
                .map(|operators| operators.to_vec());
            if let Some(operators) = &profile {
                assert_eq!(
                    operators
                        .iter()
                        .map(|operator| operator.db_hits)
                        .sum::<u64>(),
                    summary.metrics.db_hits,
                    "exact storage accounting: {plan}; {operators:?}"
                );
            }
            return Ok(Observed {
                columns,
                plan,
                rows,
                profile,
            });
        }
    }
}

fn write(db: &Connection, text: &str) -> Observed {
    consume(db, text, BTreeMap::new(), ExecutionOptions::default(), 256)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

fn read(db: &Connection, text: &str, options: ExecutionOptions, batch_size: usize) -> Observed {
    let head = storage::branch_head(db, "main").unwrap();
    let scope = ReadScope::new(db);
    let result = consume(db, text, BTreeMap::new(), options, batch_size)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"));
    drop(scope);
    assert!(db.is_autocommit());
    assert_eq!(storage::branch_head(db, "main").unwrap(), head);
    result
}

fn empty_db() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY,magic TEXT NOT NULL,\
         database_id TEXT NOT NULL,storage_format INTEGER NOT NULL)",
    )
    .unwrap();
    db.execute(
        "INSERT INTO main._lithograph_meta VALUES(1,'lithograph-format-v1',\
         '00000000-0000-4000-8000-000000000026',?1)",
        [storage::STORAGE_FORMAT],
    )
    .unwrap();
    storage::create_storage_schema(&db).unwrap();
    storage::initialize_root(&db).unwrap();
    db
}

#[derive(Debug)]
struct Seed {
    id: Value,
    key: Value,
}

fn seed(db: &Connection, label: &str, keys: &[Value]) -> Vec<Seed> {
    let mut observed = Vec::with_capacity(keys.len());
    for (chunk, values) in keys.chunks(256).enumerate() {
        let first = chunk * 256 + 1;
        let text = format!(
            "UNWIND range(0,size($keys)-1) AS i \
             CREATE(n:{label}:DFVisible {{seq:$first+i,k:$keys[i]}}) \
             RETURN elementId(n),n.k,n.seq"
        );
        let result = consume(
            db,
            &text,
            BTreeMap::from([
                ("keys".into(), Value::List(values.to_vec())),
                ("first".into(), Value::Integer(first as i64)),
            ]),
            ExecutionOptions::default(),
            256,
        )
        .unwrap();
        let mut rows = result.rows;
        rows.sort_by_key(|row| match row[2] {
            Value::Integer(seq) => seq,
            _ => panic!("public seed sequence"),
        });
        assert_eq!(rows.len(), values.len());
        for (offset, row) in rows.into_iter().enumerate() {
            assert_eq!(row.len(), 3);
            assert!(matches!(row[0], Value::String(_)));
            assert_eq!(row[1], values[offset]);
            assert_eq!(row[2], Value::Integer((first + offset) as i64));
            observed.push(Seed {
                id: row[0].clone(),
                key: row[1].clone(),
            });
        }
    }
    observed
}

fn rebuild_and_checkpoint(
    db: &Connection,
    build_rows: usize,
    probe_rows: usize,
    probe_index: &str,
) {
    let head = storage::branch_head(db, "main").unwrap();
    let version = format!("commit/{}", head.to_hex());
    for (name, rows) in [(probe_index, probe_rows), ("df_build_k_range", build_rows)] {
        let result = consume(
            db,
            "CALL lithograph.index.rebuild($name,$version) \
             YIELD name,commit,indexedEntities RETURN name,commit,indexedEntities",
            BTreeMap::from([
                ("name".into(), Value::String(name.into())),
                ("version".into(), Value::String(version.clone())),
            ]),
            ExecutionOptions::default(),
            3,
        )
        .unwrap();
        assert_eq!(
            result.rows,
            vec![vec![
                Value::String(name.into()),
                Value::String(version.clone()),
                Value::Integer(rows as i64),
            ]]
        );
        assert_eq!(storage::branch_head(db, "main").unwrap(), head);
    }
    storage::create_checkpoint(db, head).unwrap();
    assert_eq!(storage::branch_head(db, "main").unwrap(), head);
}

struct Fixture {
    db: Connection,
    expected: Vec<Vec<Value>>,
}

fn fixture(build_rows: usize) -> Fixture {
    fixture_with_targets(build_rows, TARGETS)
}

fn fixture_with_targets(build_rows: usize, targets: [usize; 3]) -> Fixture {
    assert!(0 < targets[0] && targets[0] < targets[1]);
    assert!(targets[1] < targets[2] && targets[2] <= PROBE_ROWS);
    let db = empty_db();
    let probe_keys = (1..=PROBE_ROWS)
        .map(|seq| {
            let key = if let Some(key) = targets.iter().position(|target| *target == seq) {
                key
            } else {
                let skipped = targets.iter().filter(|target| **target < seq).count();
                PROBE_ROWS - 1 - (seq - 1 - skipped)
            };
            Value::Integer(key as i64)
        })
        .collect::<Vec<_>>();
    // Probe is created first; verify the public IDs instead of inferring them.
    let probe = seed(&db, "DFProbe", &probe_keys);
    for seq in targets {
        assert_eq!(probe[seq - 1].id, Value::String(format!("n:{seq}")));
    }
    let build_keys = (1..=build_rows)
        .map(|seq| Value::Integer((seq % 3) as i64))
        .collect::<Vec<_>>();
    let build = seed(&db, "DFBuild", &build_keys);
    for text in [
        "CREATE CONSTRAINT df_probe_k_type FOR(p:DFProbe) REQUIRE p.k IS :: INTEGER",
        "CREATE CONSTRAINT df_build_k_type FOR(b:DFBuild) REQUIRE b.k IS :: INTEGER",
        "CREATE CONSTRAINT df_probe_k_key FOR(p:DFProbe) REQUIRE p.k IS NODE KEY",
        "CREATE RANGE INDEX df_build_k_range FOR(b:DFBuild) ON (b.k)",
    ] {
        write(&db, text);
    }
    rebuild_and_checkpoint(&db, build_rows, PROBE_ROWS, "df_probe_k_key");
    let expected = build
        .iter()
        .enumerate()
        .map(|(offset, row)| {
            let key = (offset + 1) % 3;
            assert_eq!(row.key, Value::Integer(key as i64));
            let probe = &probe[targets[key] - 1];
            assert_eq!(probe.key, row.key);
            vec![
                probe.id.clone(),
                row.id.clone(),
                Value::Integer(key as i64),
                Value::Integer(key as i64),
            ]
        })
        .collect();
    Fixture { db, expected }
}

fn bag(rows: &[Vec<Value>]) -> Vec<String> {
    let mut rows = rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

fn checked_join(fixture: &Fixture, options: ExecutionOptions, batch_size: usize) -> Observed {
    checked_join_expected(fixture, options, batch_size, &fixture.expected)
}

fn checked_join_expected(
    fixture: &Fixture,
    options: ExecutionOptions,
    batch_size: usize,
    expected: &[Vec<Value>],
) -> Observed {
    let actual = read_forced_candidate(
        &fixture.db,
        &format!("PROFILE {JOIN}"),
        options.clone(),
        batch_size,
    );
    let original = read(
        &fixture.db,
        &format!(
            "PROFILE {}",
            JOIN.replace("WHERE p.k=b.k", "WHERE (p.k=b.k) OR false")
        ),
        options,
        batch_size,
    );
    assert!(
        !original.plan.contains("HashJoin"),
        "the OR-false oracle must retain the original MATCH path: {}",
        original.plan
    );
    assert_eq!(actual.columns, ["probe", "build", "probeKey", "buildKey"]);
    assert_eq!(actual.columns, original.columns);
    assert_eq!(bag(&actual.rows), bag(expected));
    assert_eq!(bag(&actual.rows), bag(&original.rows));
    actual
}

/// Select only an alternative whose typed Hash equality and pinned Range
/// template were already admitted by normal preparation. The fixture's public
/// TYPE/Key DDL and complete rebuild provide the additional DF proof. No cost
/// or production admission rule is changed by this cfg(test)-only replacement.
fn force_admitted_dynamic_filter(prepared: &mut PreparedQuery) {
    let (probe, build) = assert_admitted_shape(prepared);
    let step = &mut prepared.matches[0];
    let original = step.join.as_ref().expect("normally admitted JoinPlan");
    assert_eq!(original.predicates.len(), 1);
    let (equality, inner_side) = admitted_hash_equality(original, probe, build);
    let template = admitted_probe_template(original, probe, build);
    let forced = JoinPlan {
        clause_start: original.clause_start,
        snapshot_identity: original.snapshot_identity,
        declared_columns: original.declared_columns.clone(),
        predicates: original.predicates.clone(),
        templates: original.templates.clone(),
        hashes: original.hashes.clone(),
        stages: dynamic_filter_stages(probe, build, equality, inner_side, template),
    };
    step.join = Some(std::sync::Arc::new(forced));
    refresh_selected_candidate(prepared);
}

fn assert_admitted_shape(prepared: &PreparedQuery) -> (usize, usize) {
    assert!(prepared.program.is_none() && prepared.write.is_none() && prepared.schema.is_none());
    assert_eq!(prepared.graph_view, ResolvedGraphView::default());
    assert_eq!(prepared.matches.len(), 1);
    let step = &prepared.matches[0];
    assert_eq!(step.parts.len(), 2);
    assert!(!step.optional);
    assert!(step.parts.iter().all(|part| {
        part.relationship.is_none()
            && part.end.is_none()
            && part.path_variable.is_none()
            && part.start.index_seek.is_none()
            && part.start.id_seek.is_none()
    }));
    let probe = step
        .parts
        .iter()
        .position(|part| part.start.variable.as_deref() == Some("p"))
        .unwrap();
    let build = step
        .parts
        .iter()
        .position(|part| part.start.variable.as_deref() == Some("b"))
        .unwrap();
    (probe, build)
}

fn admitted_hash_equality(
    original: &JoinPlan,
    probe: usize,
    build: usize,
) -> (usize, HashOperandSide) {
    original
        .hashes
        .iter()
        .enumerate()
        .find_map(|(id, equality)| {
            if equality.family != HashFamily::Number {
                return None;
            }
            for inner in [HashOperandSide::Left, HashOperandSide::Right] {
                let (build_key, probe_key) = equality.keys(inner);
                if build_key.variable == "b"
                    && build_key.property.as_deref() == Some("k")
                    && build_key.providers.0 == 1_u128 << build
                    && probe_key.variable == "p"
                    && probe_key.property.as_deref() == Some("k")
                    && probe_key.providers.0 == 1_u128 << probe
                {
                    assert_eq!(equality.predicate, 0);
                    return Some((id, inner));
                }
            }
            None
        })
        .expect("normally admitted typed property Hash equality")
}

fn admitted_probe_template(original: &JoinPlan, probe: usize, build: usize) -> usize {
    original
        .templates
        .iter()
        .position(|template| {
            template.part == probe
                && template.target == ProbeTarget::StartNode
                && template.source_guard.0 == 1_u128 << build
                && matches!(&template.access, ProbeAccess::IndexEquality {
                source_variable, source_property, index_name, kind, ..
            } if source_variable == "b" && source_property == "k"
                && index_name == "df_probe_k_key" && *kind == storage::StandardIndexKind::Range)
        })
        .expect("normally admitted pinned Probe Key Range template")
}

fn dynamic_filter_stages(
    probe: usize,
    build: usize,
    equality: usize,
    inner_side: HashOperandSide,
    template: usize,
) -> Vec<JoinStage> {
    vec![
        JoinStage {
            part: probe,
            orientation: PartOrientation::Written,
            access: StageAccess::Existing,
            cartesian: false,
            predicates_after: Vec::new(),
        },
        JoinStage {
            part: build,
            orientation: PartOrientation::Written,
            access: StageAccess::Hash {
                equality,
                inner_side,
                dynamic_filter: Some(template),
            },
            cartesian: false,
            predicates_after: vec![0],
        },
    ]
}

/// The diagnostic baseline is the already admitted Build-to-Probe INL
/// alternative, independently of whichever alternative normal CBO selected.
fn force_admitted_index_loop(prepared: &mut PreparedQuery) {
    force_admitted_dynamic_filter(prepared);
    let plan = std::sync::Arc::get_mut(prepared.matches[0].join.as_mut().unwrap()).unwrap();
    let probe = plan.stages[0].part;
    let build = plan.stages[1].part;
    let StageAccess::Hash {
        dynamic_filter: Some(template),
        ..
    } = plan.stages[1].access
    else {
        panic!("retain the existing admitted Probe Range template")
    };
    assert_eq!(plan.templates[template].part, probe);
    assert_eq!(plan.templates[template].source_guard.0, 1_u128 << build);
    plan.stages = vec![
        JoinStage {
            part: build,
            orientation: PartOrientation::Written,
            access: StageAccess::Existing,
            cartesian: false,
            predicates_after: Vec::new(),
        },
        JoinStage {
            part: probe,
            orientation: PartOrientation::Written,
            access: StageAccess::Probe(template),
            cartesian: false,
            predicates_after: vec![0],
        },
    ];
    refresh_selected_candidate(prepared);
}

fn refresh_selected_candidate(prepared: &mut PreparedQuery) {
    let (logical, sites) = build_logical(
        &prepared.matches,
        !prepared.order.is_empty(),
        prepared.skip > 0,
        prepared.limit.is_some(),
        prepared.distinct,
        prepared.aggregate,
    );
    prepared.physical = build_physical(&logical, &prepared.matches, !prepared.order.is_empty());
    prepared.logical = logical;
    prepared.profile_sites = sites;
    prepared.metrics = QueryMetrics::default();
    prepared.metrics = QueryMetrics::for_execution_with_resources(
        prepared.mode,
        &prepared.physical,
        &prepared.resources,
    )
    .unwrap();
    prepared.install_match_profile_sites().unwrap();
}

fn read_forced_candidate(
    db: &Connection,
    text: &str,
    options: ExecutionOptions,
    batch_size: usize,
) -> Observed {
    let head = storage::branch_head(db, "main").unwrap();
    let scope = ReadScope::new(db);
    let mut prepared = prepare(db, text, BTreeMap::new(), options).unwrap();
    force_admitted_dynamic_filter(&mut prepared);
    let result = drain(db, prepared, batch_size).unwrap();
    drop(scope);
    assert!(db.is_autocommit());
    assert_eq!(storage::branch_head(db, "main").unwrap(), head);
    result
}

fn assert_dynamic_filter(actual: &Observed, build_rows: usize) -> &HashJoinRuntimeMetrics {
    assert!(actual.plan.contains("HashJoin"), "{}", actual.plan);
    assert!(!actual.plan.contains("IndexNestedLoop"), "{}", actual.plan);
    let profile = actual.profile.as_ref().expect("complete PROFILE execution");
    let hashes = profile
        .iter()
        .filter(|operator| operator.operator == "HashJoin")
        .collect::<Vec<_>>();
    assert_eq!(hashes.len(), 1, "{}; {profile:?}", actual.plan);
    assert_eq!(hashes[0].rows, build_rows as u64);
    let hash = hashes[0]
        .hash
        .as_ref()
        .expect("actual Hash runtime metrics");
    let filter = hash.dynamic_filter.as_ref().expect("actual Dynamic Filter");
    assert!(filter.applied, "{hash:?}");
    assert_eq!(hash.build_rows, build_rows as u64, "{hash:?}");
    assert_eq!(filter.build_rows, build_rows as u64, "{filter:?}");
    assert_eq!(filter.distinct_keys, 3, "{filter:?}");
    assert_eq!(filter.prefix_rows_read, 1, "{filter:?}");
    assert_eq!(filter.equality_seeks, 3, "{filter:?}");
    assert_eq!(filter.tail_owners, 3, "{filter:?}");
    assert_eq!(hash.probe_rows, 4, "{hash:?}");
    assert!(
        hash.probe_rows < PROBE_ROWS as u64,
        "actual keyed skip: {hash:?}"
    );
    assert!(filter.keyset_peak_bytes > 0, "{filter:?}");
    hash
}

#[test]
fn dynamic_filter_forced_admitted_resident_candidate_preserves_the_complete_frontier_bag() {
    let fixture = fixture(64);
    for batch_size in [1, 3, 256] {
        let actual = checked_join(&fixture, ExecutionOptions::default(), batch_size);
        let hash = assert_dynamic_filter(&actual, 64);
        assert_eq!(hash.spill_partitions, 0, "resident Hash: {hash:?}");
        assert_eq!(hash.spill_batches, 0, "resident Hash: {hash:?}");
        assert_eq!(hash.temp_allocated_bytes, 0, "resident Hash: {hash:?}");
    }
}

#[test]
fn dynamic_filter_forced_admitted_grace_candidate_preserves_build_multiplicity() {
    let fixture = fixture(1024);
    let options = ExecutionOptions {
        memory_limit_bytes: Some(512 * 1024),
        ..ExecutionOptions::default()
    };
    let actual = checked_join(&fixture, options, 3);
    let hash = assert_dynamic_filter(&actual, 1024);
    assert!(hash.spill_partitions > 1, "real Grace partitions: {hash:?}");
    assert!(hash.spill_batches > 0, "real Grace replay: {hash:?}");
    assert!(hash.spill_vm_steps > 0, "real SQLite spill work: {hash:?}");
    assert!(
        hash.spill_encoded_bytes > 0,
        "typed spill payload: {hash:?}"
    );
    assert!(
        hash.temp_allocated_bytes > 0,
        "allocated TEMP pages: {hash:?}"
    );
    let counts = TARGETS.map(|seq| {
        let id = Value::String(format!("n:{seq}"));
        actual.rows.iter().filter(|row| row[0] == id).count()
    });
    assert_eq!(counts, [341, 342, 341]);
}

fn prepare_limited_resident(db: &Connection, limit: u64) -> PreparedQuery {
    let options = ExecutionOptions {
        memory_limit_bytes: Some(limit),
        ..ExecutionOptions::default()
    };
    let mut prepared = prepare(db, &format!("PROFILE {JOIN}"), BTreeMap::new(), options).unwrap();
    force_admitted_dynamic_filter(&mut prepared);
    prepared
}

struct PausedResident {
    columns: Vec<String>,
    plan: String,
    resources: crate::query::QueryResources,
    cursor: QueryCursor,
    rows: Vec<Vec<Value>>,
}

fn pause_resident_after_first_owner(db: &Connection, prepared: PreparedQuery) -> PausedResident {
    let columns = prepared.columns.clone();
    let plan = prepared.physical.explain();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let mut first = cursor.next_batch(db, 1).unwrap();
    assert_eq!(first.rows.len(), 1);
    assert!(!first.done);
    assert_eq!(first.rows[0][0], Value::String("n:1".into()));
    let rows = std::mem::take(&mut first.rows);
    drop(first);
    PausedResident {
        columns,
        plan,
        resources,
        cursor,
        rows,
    }
}

fn complete_resident_scan(fixture: &Fixture, paused: &mut PausedResident) -> Observed {
    let actual = collect_remaining(
        &fixture.db,
        &mut paused.cursor,
        std::mem::take(&mut paused.columns),
        std::mem::take(&mut paused.plan),
        std::mem::take(&mut paused.rows),
        1,
    )
    .unwrap();
    assert_eq!(bag(&actual.rows), bag(&fixture.expected));
    actual
}

fn observed_hash(actual: &Observed) -> &HashJoinRuntimeMetrics {
    actual
        .profile
        .as_ref()
        .unwrap()
        .iter()
        .find(|operator| operator.operator == "HashJoin")
        .unwrap()
        .hash
        .as_ref()
        .unwrap()
}

/// Measure a conservative continuation budget using the same resident Hash
/// producer with DF disabled. Its complete execution peak includes every real
/// row/page allocation; the pause matches the late-lease case below.
fn original_tail_headroom(fixture: &Fixture, limit: u64) -> u64 {
    let scope = ReadScope::new(&fixture.db);
    let mut prepared = prepare_limited_resident(&fixture.db, limit);
    let plan = std::sync::Arc::get_mut(prepared.matches[0].join.as_mut().unwrap()).unwrap();
    let StageAccess::Hash { dynamic_filter, .. } = &mut plan.stages[1].access else {
        panic!("the baseline retains the same resident Hash consumer")
    };
    *dynamic_filter = None;
    let mut paused = pause_resident_after_first_owner(&fixture.db, prepared);
    let paused_used = paused.resources.used();
    let actual = complete_resident_scan(fixture, &mut paused);
    let hash = observed_hash(&actual);
    assert!(hash.dynamic_filter.is_none());
    assert_eq!(hash.build_rows, 64);
    assert_eq!(hash.probe_rows, PROBE_ROWS as u64);
    assert_eq!(hash.spill_partitions, 0);
    let peak = paused.resources.peak();
    drop(paused.cursor);
    assert_eq!(paused.resources.used(), 0);
    assert!(peak <= limit);
    drop(scope);
    // Do not clamp the measured budget to force the window: if it no longer
    // fits below the optional 64 KiB grant, this regression needs a new fixture.
    let headroom = peak.saturating_sub(paused_used).saturating_add(16 * 1024);
    assert!(
        headroom < 64 * 1024,
        "original continuation requires {headroom} bytes; paused={paused_used}, peak={peak}"
    );
    headroom
}

#[test]
fn dynamic_filter_late_caller_lease_abandons_the_tail_and_completes_the_original_scan() {
    const LIMIT: u64 = 4 * 1024 * 1024;
    // The first physical owner matches, so its first output pauses after the
    // complete Build but before the optional owner>1 tail has been consumed.
    let fixture = fixture_with_targets(64, [1, 501, 901]);
    let head = storage::branch_head(&fixture.db, "main").unwrap();
    let headroom = original_tail_headroom(&fixture, LIMIT);
    let scope = ReadScope::new(&fixture.db);
    let prepared = prepare_limited_resident(&fixture.db, LIMIT);
    let mut paused = pause_resident_after_first_owner(&fixture.db, prepared);
    let available = LIMIT.checked_sub(paused.resources.used()).unwrap();
    assert!(available > headroom);
    let held = paused.resources.reserve(available - headroom).unwrap();
    assert_eq!(paused.resources.limit(), Some(LIMIT));
    assert_eq!(LIMIT - paused.resources.used(), headroom);
    // The caller keeps this claim throughout all remaining pulls. Success is
    // required; Resource is not an acceptable advisory-fallback outcome.
    let actual = complete_resident_scan(&fixture, &mut paused);
    let hash = observed_hash(&actual);
    let filter = hash
        .dynamic_filter
        .as_ref()
        .expect("installed before the pause");
    assert!(
        !filter.applied,
        "late grant refusal must be advisory: {hash:?}"
    );
    assert_eq!(filter.distinct_keys, 3);
    assert_eq!(filter.equality_seeks, 0);
    assert_eq!(filter.tail_owners, 0);
    assert_eq!(filter.prefix_rows_read, 1);
    assert_eq!(hash.build_rows, 64);
    assert_eq!(hash.probe_rows, PROBE_ROWS as u64);
    assert_eq!(hash.spill_partitions, 0);
    assert!(paused.resources.peak() <= LIMIT);
    drop(paused.cursor);
    assert_eq!(paused.resources.used(), held.bytes());
    drop(held);
    assert_eq!(paused.resources.used(), 0);
    assert_eq!(paused.resources.limit(), Some(LIMIT));
    drop(scope);
    assert!(fixture.db.is_autocommit());
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), head);
    let original = read(
        &fixture.db,
        &format!(
            "PROFILE {}",
            JOIN.replace("WHERE p.k=b.k", "WHERE (p.k=b.k) OR false")
        ),
        ExecutionOptions::default(),
        3,
    );
    assert_eq!(bag(&actual.rows), bag(&original.rows));
}

struct DiagnosticProgressGuard<'a>(&'a Connection);

impl Drop for DiagnosticProgressGuard<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>).unwrap();
    }
}

/// Exactly one complete workload invocation. Fixture setup, transaction-scope
/// setup/release and the independent bag assertions stay outside the counters.
struct DiagnosticMeasurement {
    order: usize,
    forced_df: bool,
    prepare_main_vm: u64,
    prepare_wall_us: u128,
    rewrite_main_vm: u64,
    rewrite_wall_us: u128,
    runtime_main_vm: u64,
    runtime_wall_us: u128,
    source_slot: usize,
    source_variable: String,
    normal_cbo_plan: String,
    normal_cbo_stages: String,
}

fn diagnostic_single_read(fixture: &Fixture, forced_df: bool, order: usize) -> Observed {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    let head = storage::branch_head(&fixture.db, "main").unwrap();
    let scope = ReadScope::new(&fixture.db);
    let steps = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&steps);
    fixture
        .db
        .progress_handler(
            1,
            Some(move || {
                observed.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
    let counter = DiagnosticProgressGuard(&fixture.db);
    let prepare_started = Instant::now();
    let mut prepared = prepare(
        &fixture.db,
        &format!("PROFILE {JOIN}"),
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap();
    let prepare_wall_us = prepare_started.elapsed().as_micros();
    let prepare_main_vm = steps.load(Ordering::Relaxed);
    let normal_cbo_plan = prepared.physical.explain();
    let normal_cbo_stages = format!("{:?}", prepared.matches[0].join.as_ref().unwrap().stages);
    let rewrite_started = Instant::now();
    if forced_df {
        force_admitted_dynamic_filter(&mut prepared);
    } else {
        force_admitted_index_loop(&mut prepared);
    }
    let rewrite_wall_us = rewrite_started.elapsed().as_micros();
    let rewrite_main_vm = steps.load(Ordering::Relaxed) - prepare_main_vm;
    let step = &prepared.matches[0];
    let source = step.join.as_ref().unwrap().stages[0].part;
    let source_variable = step.parts[source].start.variable.clone().unwrap();
    let source_slot = prepared
        .profile_sites
        .iter()
        .find(|binding| {
            binding.site.clause_start == step.clause_start
                && binding.site.part == Some(source)
                && binding.site.role == crate::query::plan::ProfileRole::NodeAccess
        })
        .expect("actual first producer PROFILE site")
        .physical_slot;
    let runtime_start_vm = steps.load(Ordering::Relaxed);
    let runtime_started = Instant::now();
    // This interval includes QueryCursor construction, every pull, complete,
    // PROFILE copying/accounting checks and cursor/resource cleanup in drain.
    let actual = drain(&fixture.db, prepared, 3).unwrap();
    let runtime_wall_us = runtime_started.elapsed().as_micros();
    let runtime_main_vm = steps.load(Ordering::Relaxed) - runtime_start_vm;
    drop(counter);
    drop(scope);
    assert!(fixture.db.is_autocommit());
    assert_eq!(storage::branch_head(&fixture.db, "main").unwrap(), head);
    assert_eq!(bag(&actual.rows), bag(&fixture.expected));
    report_diagnostic(
        &actual,
        DiagnosticMeasurement {
            order,
            forced_df,
            prepare_main_vm,
            prepare_wall_us,
            rewrite_main_vm,
            rewrite_wall_us,
            runtime_main_vm,
            runtime_wall_us,
            source_slot,
            source_variable,
            normal_cbo_plan,
            normal_cbo_stages,
        },
    );
    actual
}

fn report_diagnostic(actual: &Observed, measurement: DiagnosticMeasurement) {
    let DiagnosticMeasurement {
        order,
        forced_df,
        prepare_main_vm,
        prepare_wall_us,
        rewrite_main_vm,
        rewrite_wall_us,
        runtime_main_vm,
        runtime_wall_us,
        source_slot,
        source_variable,
        normal_cbo_plan,
        normal_cbo_stages,
    } = measurement;
    let profile = actual.profile.as_ref().expect("complete PROFILE execution");
    let private_spill_vm = profile
        .iter()
        .filter_map(|operator| operator.hash.as_ref())
        .map(|hash| hash.spill_vm_steps)
        .sum::<u64>();
    let (probes, probes_basis) = if forced_df {
        let hash = assert_dynamic_filter(actual, 1024);
        (hash.probe_rows, "Hash.probe_rows")
    } else {
        assert!(actual.plan.contains("IndexNestedLoop"), "{}", actual.plan);
        assert!(!actual.plan.contains("HashJoin"), "{}", actual.plan);
        assert!(
            profile
                .iter()
                .any(|operator| operator.operator == "IndexNestedLoop" && operator.rows > 0),
            "actual INL execution: {profile:?}"
        );
        assert_eq!(private_spill_vm, 0);
        // Both fixture sides have non-NULL keys and no prefix filter. Each
        // actual first-producer row starts exactly one INL equality probe.
        (
            profile[source_slot].rows,
            "outer_NodeAccess.rows_nonnull_fixture",
        )
    };
    assert!(prepare_main_vm > 0 && runtime_main_vm > 0);
    assert_eq!(
        rewrite_main_vm, 0,
        "the cfg(test) candidate rewrite runs no SQL"
    );
    let runtime_vm_sum = runtime_main_vm.checked_add(private_spill_vm).unwrap();
    let invocation_main_vm = prepare_main_vm
        .checked_add(rewrite_main_vm)
        .and_then(|value| value.checked_add(runtime_main_vm))
        .unwrap();
    let invocation_vm_sum = invocation_main_vm.checked_add(private_spill_vm).unwrap();
    let variant = if forced_df {
        "forced_df_consumer"
    } else {
        "admitted_INL_baseline"
    };
    let warm_state = if forced_df {
        "same_connection_after_admitted_INL_baseline"
    } else {
        "after_public_fixture_setup_no_workload_warmup"
    };
    println!(
        "dynamic_filter_diagnostic normal_cbo_original_plan order={order} \
         physical={} stages={normal_cbo_stages}",
        normal_cbo_plan.replace('\n', " | "),
    );
    println!(
        "dynamic_filter_diagnostic order={order} variant={variant} warm_state={warm_state} \
         prepare_main_vm={prepare_main_vm} prepare_wall_us={prepare_wall_us} \
         test_rewrite_main_vm={rewrite_main_vm} test_rewrite_wall_us={rewrite_wall_us} \
         runtime_main_vm={runtime_main_vm} private_spill_vm={private_spill_vm} \
         runtime_vm_sum={runtime_vm_sum} runtime_wall_us={runtime_wall_us} \
         invocation_main_vm={invocation_main_vm} invocation_vm_sum={invocation_vm_sum} \
         phase_wall_sum_us={} probes={probes} probes_basis={probes_basis} \
         source_variable={source_variable} rows={} memory_limit_bytes=none \
         plan={}",
        prepare_wall_us + rewrite_wall_us + runtime_wall_us,
        actual.rows.len(),
        actual.plan.replace('\n', " | "),
    );
}

#[test]
fn dynamic_filter_diagnostic_single_admitted_inl_then_forced_df_main_and_private_vm() {
    let fixture = fixture(1024);
    println!(
        "dynamic_filter_diagnostic diagnostic_only=true build_rows=1024 probe_rows=1000 \
         samples_per_variant=1 order=admitted_INL_baseline_then_forced_df same_connection=true \
         workload_warmup_replays=0 cache_equivalence_asserted=false \
         scope_setup_release_and_oracle_checks_excluded=true \
         main_vm_source=main_connection_progress_handler_1 \
         private_vm_source=Hash.spill_vm_steps_disjoint_private_connection \
         runtime_boundary=QueryCursor_new_through_complete_and_drop \
         wall_includes_profile_and_progress_handler_overhead=true \
         cost_or_latency_acceptance=false"
    );
    let baseline = diagnostic_single_read(&fixture, false, 1);
    let forced = diagnostic_single_read(&fixture, true, 2);
    assert_eq!(baseline.columns, forced.columns);
    assert_eq!(bag(&baseline.rows), bag(&forced.rows));
}
