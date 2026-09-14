use super::*;

pub(super) struct ConflictPage {
    pub(super) conflicts: Vec<MergeConflict>,
    pub(super) has_more: bool,
}

pub(super) struct ConflictPageBuilder {
    offset: usize,
    limit: usize,
    seen: usize,
    conflicts: Vec<MergeConflict>,
    has_more: bool,
}

impl ConflictPageBuilder {
    pub(super) fn new(offset: usize, limit: usize) -> Self {
        Self {
            offset,
            limit,
            seen: 0,
            conflicts: Vec::with_capacity(limit),
            has_more: false,
        }
    }

    pub(super) fn push(&mut self, conflict: MergeConflict) -> bool {
        if self.seen < self.offset {
            self.seen += 1;
            return false;
        }
        if self.conflicts.len() < self.limit {
            self.conflicts.push(conflict);
            self.seen += 1;
            return false;
        }
        self.seen += 1;
        self.has_more = true;
        true
    }

    pub(super) fn finish(self) -> QueryResult<ConflictPage> {
        if self.seen < self.offset {
            return Err(QueryError::invalid_argument(
                "merge conflict cursor is out of range",
            ));
        }
        Ok(ConflictPage {
            conflicts: self.conflicts,
            has_more: self.has_more,
        })
    }
}
