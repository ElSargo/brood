#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PanePlacement {
    pub(crate) offset_u: f32,
    pub(crate) offset_v: f32,
    pub(crate) size_u: f32,
    pub(crate) size_v: f32,
}

pub(crate) fn pane_placement(index: usize, panes: &[u64], pane_backends: &[u64]) -> PanePlacement {
    if panes.len() <= 1 {
        return PanePlacement {
            offset_u: -1.0,
            offset_v: -1.0,
            size_u: 2.0,
            size_v: 2.0,
        };
    }

    let backend_at = |index: usize| pane_backends.get(index).copied().unwrap_or(0);
    let mut distinct_backends = 0;
    let mut candidate = 0;
    while candidate < panes.len() {
        let backend = backend_at(candidate);
        if !(0..candidate).any(|previous| backend_at(previous) == backend) {
            distinct_backends += 1;
        }
        candidate += 1;
    }
    if distinct_backends <= 1 {
        if index == 0 {
            return PanePlacement {
                offset_u: -1.0,
                offset_v: -1.0,
                size_u: 1.0,
                size_v: 2.0,
            };
        }
        let each = 2.0 / ((panes.len() - 1) as f32);
        return PanePlacement {
            offset_u: 0.0,
            offset_v: -1.0 + each * ((index - 1) as f32),
            size_u: 1.0,
            size_v: each,
        };
    }

    let backend = backend_at(index);
    let first_group_pane = (0..panes.len())
        .find(|candidate| backend_at(*candidate) == backend)
        .unwrap_or(index);
    let group_index = (0..first_group_pane)
        .filter(|candidate| {
            let candidate_backend = backend_at(*candidate);
            !(0..*candidate).any(|previous| backend_at(previous) == candidate_backend)
        })
        .count();
    let pane_index = (0..index)
        .filter(|candidate| backend_at(*candidate) == backend)
        .count();
    let panes_in_group = (0..panes.len())
        .filter(|candidate| backend_at(*candidate) == backend)
        .count();
    let width = 2.0 / (distinct_backends as f32);
    let height = 2.0 / (panes_in_group as f32);
    PanePlacement {
        offset_u: -1.0 + width * (group_index as f32),
        offset_v: -1.0 + height * (pane_index as f32),
        size_u: width,
        size_v: height,
    }
}
