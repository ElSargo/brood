use crate::{backend_layout::pane_group, LayoutTransform, MAX_PANES};

pub const STACK_LIMIT: usize = 5;
const STRIP_PIXELS: f32 = 32.0;

#[derive(Clone, Copy)]
struct Stack {
    master: u64,
    raised: u64,
    first: usize,
    scroll_pixels: f64,
}

impl Stack {
    const EMPTY: Self = Self {
        master: 0,
        raised: 0,
        first: 0,
        scroll_pixels: 0.0,
    };
}

#[derive(Clone, Copy)]
pub struct MasterStack {
    stacks: [Stack; MAX_PANES],
}

impl MasterStack {
    pub const EMPTY: Self = Self {
        stacks: [Stack::EMPTY; MAX_PANES],
    };

    pub fn sync(&mut self, panes: &[u64], backends: &[u64], focus: Option<u64>, reveal: bool) {
        let previous = *self;
        self.stacks = [Stack::EMPTY; MAX_PANES];
        for index in 0..panes.len() {
            let group = pane_group(index, panes.len(), backends);
            if group.index != 0 || group.count < 2 {
                continue;
            }
            let mut stack = previous
                .stacks
                .iter()
                .copied()
                .find(|stack| stack.master == panes[index])
                .unwrap_or(Stack {
                    master: panes[index],
                    ..Stack::EMPTY
                });
            let members =
                |candidate: usize| pane_group(candidate, panes.len(), backends).first == index;
            if !(0..panes.len()).any(|candidate| {
                members(candidate) && panes[candidate] == stack.raised && candidate != index
            }) {
                stack.raised = panes[(index + 1..panes.len())
                    .find(|candidate| members(*candidate))
                    .unwrap()];
            }
            stack.first = stack
                .first
                .min((group.count - 1).saturating_sub(STACK_LIMIT));
            if let Some(focused) = focus.and_then(|pane| {
                (0..panes.len()).find(|candidate| {
                    panes[*candidate] == pane && members(*candidate) && *candidate != index
                })
            }) {
                stack.raised = panes[focused];
                if reveal {
                    let position = pane_group(focused, panes.len(), backends).index - 1;
                    stack.first = stack
                        .first
                        .min(position)
                        .max(position.saturating_sub(STACK_LIMIT - 1));
                }
            }
            self.stacks[index] = stack;
        }
    }

    pub fn scroll(&mut self, first: usize, count: usize, pixels: f64) {
        let stack = &mut self.stacks[first];
        stack.scroll_pixels += pixels;
        let steps = (stack.scroll_pixels.abs() / 40.0) as usize;
        if steps == 0 {
            return;
        }
        let forward = stack.scroll_pixels < 0.0;
        stack.scroll_pixels %= 40.0;
        stack.first = if forward {
            stack.first.saturating_add(steps)
        } else {
            stack.first.saturating_sub(steps)
        }
        .min(count.saturating_sub(1 + STACK_LIMIT));
    }

    pub fn visible(&self, index: usize, panes: &[u64], backends: &[u64]) -> bool {
        let group = pane_group(index, panes.len(), backends);
        group.index == 0
            || (group.index > self.stacks[group.first].first
                && group.index - 1 < self.stacks[group.first].first + STACK_LIMIT)
    }

    pub fn z(&self, index: usize, panes: &[u64], backends: &[u64]) -> f32 {
        let group = pane_group(index, panes.len(), backends);
        let stack = self.stacks[group.first];
        if panes[index] == stack.raised {
            return MAX_PANES as f32;
        }
        let raised_index = panes
            .iter()
            .position(|pane| *pane == stack.raised)
            .unwrap_or(index);
        // Earlier windows expose their tops; later windows expose their bottoms.
        if index < raised_index {
            index as f32
        } else {
            (MAX_PANES - 1 - index) as f32
        }
    }

    pub fn placement(
        &self,
        index: usize,
        panes: &[u64],
        backends: &[u64],
        height: i32,
    ) -> LayoutTransform {
        let group = pane_group(index, panes.len(), backends);
        if group.count == 1 {
            return LayoutTransform {
                offset_u: group.offset,
                offset_v: -1.0,
                size_u: group.width,
                size_v: 2.0,
            };
        }
        let master_width = group.width * 0.55;
        if group.index == 0 {
            return LayoutTransform {
                offset_u: group.offset,
                offset_v: -1.0,
                size_u: master_width,
                size_v: 2.0,
            };
        }
        let visible_count = (group.count - 1).min(STACK_LIMIT);
        let slot = (group.index - 1)
            .saturating_sub(self.stacks[group.first].first)
            .min(visible_count - 1);
        let stagger = (2.0 * STRIP_PIXELS / height.max(1) as f32).min(0.15);
        LayoutTransform {
            offset_u: group.offset + master_width,
            offset_v: -1.0 + slot as f32 * stagger,
            size_u: group.width - master_width,
            size_v: 2.0 - (visible_count - 1) as f32 * stagger,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_unfocused_window_has_an_exposed_strip() {
        for count in 2..=6 {
            let panes = core::array::from_fn::<_, 6, _>(|index| index as u64 + 1);
            let panes = &panes[..count];
            for focus in panes {
                let mut layout = MasterStack::EMPTY;
                layout.sync(panes, &[], Some(*focus), true);
                for index in 1..count {
                    let bounds = layout.placement(index, panes, &[], 720);
                    let top = bounds.offset_v + 0.001;
                    let bottom = bounds.offset_v + bounds.size_v - 0.001;
                    let exposed = |v| {
                        (1..count)
                            .filter(|other| {
                                let rect = layout.placement(*other, panes, &[], 720);
                                v >= rect.offset_v && v < rect.offset_v + rect.size_v
                            })
                            .max_by(|a, b| {
                                layout
                                    .z(*a, panes, &[])
                                    .total_cmp(&layout.z(*b, panes, &[]))
                            })
                            == Some(index)
                    };
                    assert!(
                        exposed(top) || exposed(bottom),
                        "count={count} focus={focus} index={index}"
                    );
                }
            }
        }
    }

    #[test]
    fn scrolling_and_focus_reveal_are_bounded_and_backend_local() {
        let panes = core::array::from_fn::<_, MAX_PANES, _>(|index| index as u64 + 1);
        let backends = core::array::from_fn::<_, MAX_PANES, _>(|index| (index % 2) as u64);
        let mut layout = MasterStack::EMPTY;
        layout.sync(&panes, &backends, Some(1), true);
        layout.scroll(0, 32, -10_000.0);
        assert!(layout.visible(62, &panes, &backends));
        assert!(!layout.visible(2, &panes, &backends));
        assert!(layout.visible(3, &panes, &backends));
        for pane in panes {
            layout.sync(&panes, &backends, Some(pane), true);
            assert!(layout.visible(pane as usize - 1, &panes, &backends));
            assert_eq!(
                (0..MAX_PANES)
                    .filter(|index| layout.visible(*index, &panes, &backends))
                    .count(),
                12
            );
        }
    }
}
