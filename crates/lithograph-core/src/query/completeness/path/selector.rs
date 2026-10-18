//! Budgeted, cancellable path selection and ordering.

use super::*;

fn compare_paths(
    left: &PathState,
    right: &PathState,
    control: &ExecutionControl<'_>,
) -> QueryResult<std::cmp::Ordering> {
    control.checkpoint(1)?;
    let length = left.relationships.len().cmp(&right.relationships.len());
    if length != std::cmp::Ordering::Equal {
        return Ok(length);
    }
    for (left, right) in left.relationships.iter().zip(&right.relationships) {
        control.checkpoint(1)?;
        let ordering = left.id.cmp(&right.id);
        if ordering != std::cmp::Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(std::cmp::Ordering::Equal)
}

pub(super) fn sort_paths(
    paths: Vec<PathState>,
    control: &ExecutionControl<'_>,
) -> QueryResult<Vec<PathState>> {
    let length = paths.len();
    let _memory = control.reserve(checked_add(
        allocation_bytes::<Option<PathState>>(length.saturating_mul(2))?,
        allocation_bytes::<PathState>(length)?,
    )?)?;
    let mut source = paths.into_iter().map(Some).collect::<Vec<_>>();
    let mut target = (0..length)
        .map(|_| None)
        .collect::<Vec<Option<PathState>>>();
    let mut width = 1_usize;
    while width < length {
        for start in (0..length).step_by(width.saturating_mul(2)) {
            let middle = start.saturating_add(width).min(length);
            let end = middle.saturating_add(width).min(length);
            merge_path_run(&mut source, &mut target, start, middle, end, control)?;
        }
        std::mem::swap(&mut source, &mut target);
        width = width.saturating_mul(2);
    }
    source
        .into_iter()
        .map(|state| state.ok_or_else(|| QueryError::internal("sorted path state is missing")))
        .collect()
}

fn merge_path_run(
    source: &mut [Option<PathState>],
    target: &mut [Option<PathState>],
    start: usize,
    middle: usize,
    end: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let mut left = start;
    let mut right = middle;
    for destination in &mut target[start..end] {
        control.checkpoint(1)?;
        let choose_left = if left >= middle {
            false
        } else if right >= end {
            true
        } else {
            let left_state = source[left]
                .as_ref()
                .ok_or_else(|| QueryError::internal("path sort left state is missing"))?;
            let right_state = source[right]
                .as_ref()
                .ok_or_else(|| QueryError::internal("path sort right state is missing"))?;
            compare_paths(left_state, right_state, control)? != std::cmp::Ordering::Greater
        };
        let index = if choose_left {
            let index = left;
            left += 1;
            index
        } else {
            let index = right;
            right += 1;
            index
        };
        *destination = source[index].take();
    }
    Ok(())
}

pub(super) fn apply_selector(
    states: Vec<PathState>,
    selector: PathSelector,
    control: &ExecutionControl<'_>,
) -> QueryResult<Vec<PathState>> {
    if selector.kind == PathSelectorKind::All {
        return Ok(states);
    }
    let mut partitions = BTreeMap::<(Option<i64>, Option<i64>), Vec<PathState>>::new();
    for state in states {
        control.checkpoint(1)?;
        partitions
            .entry((state.nodes.first().copied(), state.nodes.last().copied()))
            .or_default()
            .push(state);
    }
    let mut output = Vec::new();
    for (_, paths) in partitions {
        control.check()?;
        let paths = sort_paths(paths, control)?;
        match selector.kind {
            PathSelectorKind::Any => output.extend(paths.into_iter().take(selector.count)),
            PathSelectorKind::AnyShortest => output.extend(paths.into_iter().take(1)),
            PathSelectorKind::AllShortest => {
                if let Some(length) = paths.first().map(|path| path.relationships.len()) {
                    output.extend(
                        paths
                            .into_iter()
                            .take_while(|path| path.relationships.len() == length),
                    );
                }
            }
            PathSelectorKind::ShortestPaths => {
                output.extend(paths.into_iter().take(selector.count));
            }
            PathSelectorKind::ShortestGroups => {
                let mut lengths = BTreeSet::new();
                for path in paths {
                    control.checkpoint(1)?;
                    lengths.insert(path.relationships.len());
                    if lengths.len() > selector.count {
                        break;
                    }
                    output.push(path);
                }
            }
            PathSelectorKind::All => unreachable!(),
        }
    }
    Ok(output)
}
