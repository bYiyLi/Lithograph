//! Query-local samples from bounded keyset reads of the resolved Snapshot.

use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, params};

use super::super::{LabelId, NodeId, RelationshipTypeId, StorageResult};
use super::{DeltaOp, Snapshot};

const MAX_SAMPLES: usize = 32;
const DEGREE_CAP: usize = 32;
const LABEL_SCRATCH_BYTES: usize = 4096;
const DEGREE_SCRATCH_BYTES: usize = 16 * 1024;
const NODE_PAGE_WORK: usize = 4;

#[derive(Debug)]
pub(crate) struct LabelCombinationSample {
    pub(crate) expected: u64,
    pub(crate) lower: u64,
    pub(crate) upper: u64,
    pub(crate) sample_rows: usize,
    pub(crate) sample_matches: usize,
    pub(crate) exhaustive: bool,
    pub(crate) work: usize,
    pub(crate) bytes: usize,
}

#[derive(Debug)]
pub(crate) struct DegreeStatisticsSample {
    pub(crate) expected: u64,
    pub(crate) lower: u64,
    pub(crate) upper: Option<u64>,
    pub(crate) mean: f64,
    pub(crate) max_observed: u64,
    /// Node counts for degrees 0, 1, 2..=4, 5..=16, and 17+.
    pub(crate) buckets: [u64; 5],
    pub(crate) sample_rows: usize,
    pub(crate) truncated_nodes: usize,
    pub(crate) exhaustive: bool,
    pub(crate) work: usize,
    pub(crate) bytes: usize,
}

struct SampleBudget {
    limit: usize,
    spent: usize,
}

impl SampleBudget {
    fn spend(&mut self, work: usize) -> bool {
        if work > self.limit.saturating_sub(self.spent) {
            return false;
        }
        self.spent += work;
        true
    }
}

struct NodeSample {
    nodes: [NodeId; MAX_SAMPLES],
    len: usize,
    exhaustive: bool,
}

impl NodeSample {
    fn push(&mut self, node: NodeId) {
        if !self.nodes[..self.len].contains(&node) {
            self.nodes[self.len] = node;
            self.len += 1;
        }
    }
}

impl Snapshot<'_> {
    /// `domain` is the smallest requested Label and its count from statistics
    /// of this same Snapshot. The caller charges one shared planning budget and
    /// claims the scratch memory before entering; no sample survives this call.
    pub(crate) fn bounded_label_combination(
        &self,
        domain: (LabelId, u64),
        labels: &BTreeSet<LabelId>,
        max_samples: usize,
        max_work: usize,
        max_bytes: usize,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<Option<LabelCombinationSample>> {
        check()?;
        self.read_checkpoint()?;
        if max_bytes < LABEL_SCRATCH_BYTES || !labels.contains(&domain.0) {
            return Ok(None);
        }
        let mut budget = SampleBudget {
            limit: max_work,
            spent: 0,
        };
        let Some(nodes) =
            self.sample_domain((Some(domain.0), domain.1), max_samples, &mut budget, check)?
        else {
            return Ok(None);
        };
        let mut sample_rows = 0;
        let mut sample_matches = 0;
        for &node in &nodes.nodes[..nodes.len] {
            // Precharge every requested membership, including short-circuited
            // ones. A large Label set cannot multiply the fixed read budget.
            if !budget.spend(labels.len()) {
                break;
            }
            let matches = self.sampled_label_matches(node, labels, check)?;
            sample_rows += 1;
            sample_matches += usize::from(matches);
        }
        if sample_rows == 0 {
            return Ok(None);
        }
        let lower = sample_matches as u64;
        let upper = lower + domain.1.saturating_sub(sample_rows as u64);
        // Even a zero-match sample is an uncertain estimate, never an empty
        // access-path proof. Exhaustive zero samples use the unknown fallback.
        if upper == 0 {
            return Ok(None);
        }
        let expected = scaled_count(lower, domain.1, sample_rows)
            .max(1)
            .clamp(lower, upper);
        Ok(Some(LabelCombinationSample {
            expected,
            lower,
            upper,
            sample_rows,
            sample_matches,
            exhaustive: nodes.exhaustive && sample_rows == nodes.len,
            work: budget.spent,
            bytes: LABEL_SCRATCH_BYTES,
        }))
    }

    fn sampled_label_matches(
        &self,
        node: NodeId,
        labels: &BTreeSet<LabelId>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<bool> {
        for &label in labels {
            check()?;
            self.read_checkpoint()?;
            let present = match self.state.overlay.labels.get(&(node, label)) {
                Some(op) => *op == DeltaOp::Add,
                None => self.checkpoint_label_exists(node, label)?,
            };
            if !present {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `incoming`: Some(false) outgoing, Some(true) incoming, None incident.
    /// Parallel relationships count separately; incident self loops count once.
    /// `lower` is observed candidate rows over the whole domain; unsampled or
    /// capped tails have no finite upper bound. `mean` can underestimate a tail.
    #[allow(
        clippy::too_many_arguments,
        reason = "Snapshot sampling keeps its domain, direction, and three caller-owned bounds explicit"
    )]
    pub(crate) fn bounded_degree_statistics(
        &self,
        domain: (Option<LabelId>, u64),
        type_id: Option<RelationshipTypeId>,
        incoming: Option<bool>,
        max_samples: usize,
        max_work: usize,
        max_bytes: usize,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<Option<DegreeStatisticsSample>> {
        check()?;
        self.read_checkpoint()?;
        if max_bytes < DEGREE_SCRATCH_BYTES {
            return Ok(None);
        }
        let mut budget = SampleBudget {
            limit: max_work,
            spent: 0,
        };
        // Spread the sample using the minimum one-page cost. High degree or
        // tombstones consume the grant early and retain an unknown tail;
        // low-degree domains must still obtain useful samples in a small grant.
        let per_sample_work = if incoming.is_none() { 40 } else { 24 };
        let slots = max_samples.min(max_work / per_sample_work);
        let Some(nodes) = self.sample_domain(domain, slots, &mut budget, check)? else {
            return Ok(None);
        };
        let mut sample_rows = 0;
        let mut sum = 0;
        let mut max_observed = 0;
        let mut truncated_nodes = 0;
        let mut buckets = [0; 5];
        for &node in &nodes.nodes[..nodes.len] {
            let Some((degree, truncated)) =
                self.sample_degree(node, type_id, incoming, &mut budget, check)?
            else {
                break;
            };
            sample_rows += 1;
            sum += degree;
            max_observed = max_observed.max(degree);
            truncated_nodes += usize::from(truncated);
            buckets[degree_bucket(degree)] += 1;
        }
        if sample_rows == 0 || (nodes.exhaustive && sum == 0) {
            return Ok(None);
        }
        let exhaustive = nodes.exhaustive && sample_rows == nodes.len && truncated_nodes == 0;
        Ok(Some(DegreeStatisticsSample {
            expected: scaled_count(sum, domain.1, sample_rows).max(1),
            lower: sum,
            upper: exhaustive.then_some(sum),
            mean: sum as f64 / sample_rows as f64,
            max_observed,
            buckets,
            sample_rows,
            truncated_nodes,
            exhaustive,
            work: budget.spent,
            bytes: DEGREE_SCRATCH_BYTES,
        }))
    }

    fn sample_domain(
        &self,
        domain: (Option<LabelId>, u64),
        max_samples: usize,
        budget: &mut SampleBudget,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<Option<NodeSample>> {
        let slots = max_samples.min(MAX_SAMPLES);
        if slots == 0 || domain.1 == 0 {
            return Ok(None);
        }
        let mut sample = NodeSample {
            nodes: [0; MAX_SAMPLES],
            len: 0,
            exhaustive: false,
        };
        if domain.1 <= slots as u64 {
            let mut after = 0;
            while sample.len <= domain.1 as usize {
                let Some((node, next)) = self.sample_node_after(domain.0, after, budget, check)?
                else {
                    break;
                };
                if let Some(node) = node {
                    // Inconsistent derived totals must not become an exact
                    // estimate, or overflow the fixed sample buffer.
                    if sample.len == domain.1 as usize {
                        return Ok(None);
                    }
                    sample.push(node);
                }
                let Some(next) = next else {
                    sample.exhaustive = sample.len as u64 == domain.1;
                    break;
                };
                after = next;
            }
        } else {
            if !budget.spend(2) {
                return Ok(None);
            }
            let max_id = self.sample_domain_max(domain.0, check)?;
            for index in 0..slots {
                let mut after = ((max_id as u128 * index as u128) / slots as u128) as i64;
                loop {
                    let Some((node, next)) =
                        self.sample_node_after(domain.0, after, budget, check)?
                    else {
                        break;
                    };
                    if let Some(node) = node {
                        sample.push(node);
                        break;
                    }
                    let Some(next) = next else { break };
                    after = next;
                }
            }
        }
        Ok((sample.len > 0).then_some(sample))
    }

    fn sample_node_after(
        &self,
        label: Option<LabelId>,
        after: NodeId,
        budget: &mut SampleBudget,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<Option<(Option<NodeId>, Option<NodeId>)>> {
        if !budget.spend(NODE_PAGE_WORK) {
            return Ok(None);
        }
        check()?;
        self.read_checkpoint()?;
        // Each source reads at most one key; tombstones still consume work.
        let page = match label {
            Some(label) => self.scan_label_after(label, after, 1)?,
            None => self.scan_nodes_after(after, 1)?,
        };
        Ok(Some((page.items.first().copied(), page.next_after)))
    }

    fn sample_domain_max(
        &self,
        label: Option<LabelId>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<NodeId> {
        check()?;
        self.read_checkpoint()?;
        let overlay = match label {
            Some(label) => self
                .state
                .overlay
                .labels_by_label
                .range((label, 0)..=(label, i64::MAX))
                .next_back()
                .map(|(key, _)| key.1),
            None => self
                .state
                .overlay
                .nodes
                .last_key_value()
                .map(|(key, _)| *key),
        }
        .unwrap_or(0);
        let Some(checkpoint) = self.state.checkpoint else {
            return Ok(overlay);
        };
        // Both ORDER BY ... LIMIT 1 probes use a complete indexed prefix.
        let base: Option<NodeId> = match label {
            Some(label) => self.connection.query_row(
                "SELECT node_id FROM main._lithograph_cp_labels INDEXED BY _lithograph_cp_labels_by_label WHERE commit_id=?1 AND label_id=?2 ORDER BY node_id DESC LIMIT 1",
                params![checkpoint.as_bytes().as_slice(), label], |row| row.get(0),
            ).optional()?,
            None => self.connection.query_row(
                "SELECT node_id FROM main._lithograph_cp_nodes WHERE commit_id=?1 ORDER BY node_id DESC LIMIT 1",
                [checkpoint.as_bytes().as_slice()], |row| row.get(0),
            ).optional()?,
        };
        Ok(base.unwrap_or(0).max(overlay))
    }

    fn sample_degree(
        &self,
        node: NodeId,
        type_id: Option<RelationshipTypeId>,
        incoming: Option<bool>,
        budget: &mut SampleBudget,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<Option<(u64, bool)>> {
        let mut count = 0;
        let mut cursor = None;
        loop {
            let limit = 8.min(DEGREE_CAP + 1 - count);
            // A directed scan reads CP+overlay. Incident may read both phases,
            // each with its own CP+overlay limit, even when tombstones hide
            // most of the first phase. Charge all four sources in advance.
            let sources = if incoming.is_none() { 4 } else { 2 };
            if !budget.spend(sources * limit + 4) {
                return Ok((count > 0).then_some((count as u64, true)));
            }
            check()?;
            self.read_checkpoint()?;
            let page = match incoming {
                Some(false) => self.scan_outgoing_page(node, type_id, cursor, limit)?,
                Some(true) => self.scan_incoming_page(node, type_id, cursor, limit)?,
                None => self.scan_incident_page(node, type_id, cursor, limit)?,
            };
            count += page.items.len();
            if count > DEGREE_CAP {
                return Ok(Some((count as u64, true)));
            }
            let Some(next) = page.next_cursor else {
                return Ok(Some((count as u64, false)));
            };
            cursor = Some(next);
        }
    }
}

fn scaled_count(observed: u64, domain: u64, rows: usize) -> u64 {
    let scaled = (u128::from(observed) * u128::from(domain) + (rows / 2) as u128) / rows as u128;
    scaled.min(u64::MAX as u128) as u64
}

fn degree_bucket(degree: u64) -> usize {
    match degree {
        0 => 0,
        1 => 1,
        2..=4 => 2,
        5..=16 => 3,
        _ => 4,
    }
}
