#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanePlacement {
    pub(crate) offset_u: f32,
    pub(crate) offset_v: f32,
    pub(crate) size_u: f32,
    pub(crate) size_v: f32,
}

const TARGET_TILE_ASPECT: f32 = 1.35;

pub fn pane_placement(
    index: usize,
    panes: &[u64],
    pane_backends: &[u64],
    viewport_width: i32,
    viewport_height: i32,
) -> PanePlacement {
    if panes.len() <= 1 {
        return PanePlacement {
            offset_u: -1.0,
            offset_v: -1.0,
            size_u: 2.0,
            size_v: 2.0,
        };
    }

    let group = pane_group(index, panes.len(), pane_backends);
    let panes_in_group = group.count;
    let pane_index = group.index;
    let group_width = group.width;
    let group_offset = group.offset;
    let viewport_aspect = viewport_width.max(1) as f32 / viewport_height.max(1) as f32;
    let group_aspect = viewport_aspect * group_width / 2.0;
    let columns = grid_columns(panes_in_group, group_aspect);
    let rows = panes_in_group.div_ceil(columns);
    let row = pane_index / columns;
    let row_start = row * columns;
    let panes_in_row = columns.min(panes_in_group - row_start);
    let column = pane_index - row_start;
    let pane_width = group_width / panes_in_row as f32;
    let pane_height = 2.0 / rows as f32;

    PanePlacement {
        offset_u: group_offset + pane_width * column as f32,
        offset_v: -1.0 + pane_height * row as f32,
        size_u: pane_width,
        size_v: pane_height,
    }
}

fn grid_columns(pane_count: usize, viewport_aspect: f32) -> usize {
    let mut best_columns = 1;
    let mut best_score = f32::MAX;
    let mut columns = 1;
    while columns <= pane_count {
        let rows = pane_count.div_ceil(columns);
        let empty = rows * columns - pane_count;
        let tile_aspect = viewport_aspect * rows as f32 / columns as f32;
        let aspect_error = (tile_aspect - TARGET_TILE_ASPECT).abs();
        let empty_penalty = empty as f32 / pane_count as f32 * 0.25;
        let score = aspect_error + empty_penalty;
        if score < best_score {
            best_score = score;
            best_columns = columns;
        }
        columns += 1;
    }
    best_columns
}

const fn ceil_sqrt(value: usize) -> usize {
    let mut root = 1;
    while root * root < value {
        root += 1;
    }
    root
}

#[derive(Clone, Copy)]
pub struct PaneGroup {
    pub offset: f32,
    pub width: f32,
    pub index: usize,
    pub count: usize,
    #[allow(
        dead_code,
        reason = "the native grid consumer does not own per-backend stack state"
    )]
    pub first: usize,
}

pub fn pane_group(index: usize, pane_count: usize, pane_backends: &[u64]) -> PaneGroup {
    let backend_at = |index: usize| pane_backends.get(index).copied().unwrap_or(0);
    let backend = backend_at(index);
    let panes_in_group = (0..pane_count)
        .filter(|candidate| backend_at(*candidate) == backend)
        .count();
    let pane_index = (0..index)
        .filter(|candidate| backend_at(*candidate) == backend)
        .count();
    let group_weight = ceil_sqrt(panes_in_group);

    let mut total_weight = 0;
    let mut preceding_weight = 0;
    let mut candidate = 0;
    while candidate < pane_count {
        let candidate_backend = backend_at(candidate);
        if !(0..candidate).any(|previous| backend_at(previous) == candidate_backend) {
            let count = (candidate..pane_count)
                .filter(|other| backend_at(*other) == candidate_backend)
                .count();
            let weight = ceil_sqrt(count);
            total_weight += weight;
            if candidate_backend != backend
                && (0..candidate).all(|previous| backend_at(previous) != backend)
            {
                preceding_weight += weight;
            }
        }
        candidate += 1;
    }

    let total_weight = total_weight.max(1) as f32;
    let group_width = 2.0 * group_weight as f32 / total_weight;
    let group_offset = -1.0 + 2.0 * preceding_weight as f32 / total_weight;
    PaneGroup {
        offset: group_offset,
        width: group_width,
        index: pane_index,
        count: panes_in_group,
        first: (0..=index)
            .find(|candidate| backend_at(*candidate) == backend)
            .unwrap_or(index),
    }
}
