use super::*;

#[derive(Debug)]
struct Node {
    children: Option<[PartitionId; FANOUT]>,
    stats: PartitionStats,
}

#[derive(Debug)]
pub(super) struct Directory {
    nodes: Vec<Node>,
    memory: MemoryReservation,
}

impl Directory {
    pub(super) fn resident_bound() -> u64 {
        (MAX_NODES * std::mem::size_of::<Node>()) as u64
    }

    pub(super) fn resident_bytes(&self) -> u64 {
        self.memory.bytes()
    }

    pub(super) fn new(resources: &QueryResources) -> QueryResult<Self> {
        let mut directory = Self {
            nodes: Vec::new(),
            memory: resources.reserve(0)?,
        };
        directory.reserve(1)?;
        directory.nodes.push(Node {
            children: None,
            stats: PartitionStats::default(),
        });
        Ok(directory)
    }

    fn reserve(&mut self, additional: usize) -> QueryResult<()> {
        let needed = self.nodes.len().checked_add(additional).ok_or_else(|| {
            super::super::super::resources::memory_error("Grace directory capacity overflow")
        })?;
        if needed > MAX_NODES {
            return Err(super::super::super::resources::memory_error(
                "Grace directory limit exceeded",
            ));
        }
        if needed > self.nodes.capacity() {
            let capacity = needed
                .max(self.nodes.capacity().saturating_mul(2))
                .min(MAX_NODES);
            self.memory
                .grow(allocation_bytes::<Node>(capacity - self.nodes.capacity())?)?;
            self.nodes
                .try_reserve_exact(capacity - self.nodes.len())
                .map_err(|_| {
                    super::super::super::resources::memory_error(
                        "unable to allocate Grace directory",
                    )
                })?;
        }
        Ok(())
    }

    fn node(&self, id: PartitionId) -> QueryResult<&Node> {
        self.nodes
            .get(id.0 as usize)
            .ok_or_else(|| QueryError::internal("unknown Grace partition"))
    }

    fn leaf(&self, id: PartitionId) -> QueryResult<&Node> {
        let node = self.node(id)?;
        if node.children.is_some() {
            return Err(QueryError::internal("retired Grace partition"));
        }
        Ok(node)
    }

    pub(super) fn stats(&self, id: PartitionId) -> QueryResult<PartitionStats> {
        Ok(self.leaf(id)?.stats)
    }

    pub(super) fn add(&mut self, id: PartitionId, side: HashSide, bytes: u64) -> QueryResult<()> {
        self.leaf(id)?;
        let stats = &mut self.nodes[id.0 as usize].stats;
        let side = side as usize;
        stats.rows[side] = checked_add(stats.rows[side], 1)?;
        stats.encoded_bytes[side] = checked_add(stats.encoded_bytes[side], bytes)?;
        Ok(())
    }

    pub(super) fn route(&self, digest: &[u8; 32]) -> QueryResult<PartitionId> {
        let mut id = PartitionId::ROOT;
        for _ in 0..=MAX_DEPTH {
            let node = self.node(id)?;
            let Some(children) = node.children else {
                return Ok(id);
            };
            id = children[bucket(digest, node.stats.depth)];
        }
        Err(QueryError::internal(
            "Grace routing depth exceeds its bound",
        ))
    }

    pub(super) fn split(&mut self, id: PartitionId) -> QueryResult<[PartitionId; FANOUT]> {
        let depth = self.stats(id)?.depth;
        if depth >= MAX_DEPTH {
            return Err(QueryError::internal("Grace split exceeds depth limit"));
        }
        self.reserve(FANOUT)?;
        let first = self.nodes.len();
        let children = std::array::from_fn(|index| PartitionId((first + index) as u32));
        for _ in 0..FANOUT {
            self.nodes.push(Node {
                children: None,
                stats: PartitionStats {
                    depth: depth + 1,
                    ..PartitionStats::default()
                },
            });
        }
        self.nodes[id.0 as usize].children = Some(children);
        Ok(children)
    }

    pub(super) fn clear_retired(&mut self, id: PartitionId) {
        self.nodes[id.0 as usize].stats.rows = [0; 2];
        self.nodes[id.0 as usize].stats.encoded_bytes = [0; 2];
    }

    pub(super) fn clear_probe(&mut self, id: PartitionId) {
        self.nodes[id.0 as usize].stats.rows[1] = 0;
        self.nodes[id.0 as usize].stats.encoded_bytes[1] = 0;
    }

    pub(super) fn next_leaf(&self, after: Option<PartitionId>) -> Option<PartitionId> {
        let start = after.map_or(0, |id| id.0 as usize + 1);
        self.nodes
            .iter()
            .enumerate()
            .skip(start)
            .find_map(|(index, node)| node.children.is_none().then_some(PartitionId(index as u32)))
    }
}

pub(super) fn bucket(digest: &[u8; 32], depth: u8) -> usize {
    let offset = usize::from(depth) * 3;
    let combined = u16::from(digest[offset / 8]) | (u16::from(digest[offset / 8 + 1]) << 8);
    ((combined >> (offset % 8)) & 7) as usize
}
