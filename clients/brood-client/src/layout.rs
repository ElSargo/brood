use brood_layout::{
    InputBehavior, LayoutChildSlot, LayoutFragment, LayoutItemId, LayoutItemKind, LayoutLeaseId,
    LayoutLeaseToken, LayoutRect, LayoutSize, LocalPoint, LocalTransform, ResolvedLayout,
    ResolvedLayoutItem,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutSource {
    NativeFallback,
    Actor,
}

pub fn convert_actor_layout(layout: brood_wasm::ResolvedLayout) -> ResolvedLayout {
    ResolvedLayout {
        items: layout
            .items
            .into_iter()
            .map(|item| ResolvedLayoutItem {
                id: LayoutItemId(item.id.0),
                kind: match item.kind {
                    brood_wasm::LayoutItemKind::Pane { pane } => LayoutItemKind::Pane { pane },
                    brood_wasm::LayoutItemKind::Placeholder => LayoutItemKind::Placeholder,
                    brood_wasm::LayoutItemKind::Handle => LayoutItemKind::Handle,
                    brood_wasm::LayoutItemKind::Chrome => LayoutItemKind::Chrome,
                    brood_wasm::LayoutItemKind::Overlay => LayoutItemKind::Overlay,
                },
                z: item.z,
                transform: LocalTransform {
                    offset: LocalPoint {
                        u: item.transform.offset.u,
                        v: item.transform.offset.v,
                    },
                    size: LocalPoint {
                        u: item.transform.size.u,
                        v: item.transform.size.v,
                    },
                },
                input: match item.input {
                    brood_wasm::InputBehavior::None => InputBehavior::None,
                    brood_wasm::InputBehavior::Hit => InputBehavior::Hit,
                    brood_wasm::InputBehavior::Drag => InputBehavior::Drag,
                    brood_wasm::InputBehavior::Pane => InputBehavior::Pane,
                },
            })
            .collect(),
    }
}

pub fn convert_actor_layout_fragment(
    layout: brood_wasm::ResolvedLayout,
    rest: Option<brood_wasm::LayoutChildSlot>,
) -> LayoutFragment {
    LayoutFragment {
        layout: convert_actor_layout(layout),
        rest: rest.map(|rest| LayoutChildSlot {
            z: rest.z,
            transform: LocalTransform {
                offset: LocalPoint {
                    u: rest.transform.offset.u,
                    v: rest.transform.offset.v,
                },
                size: LocalPoint {
                    u: rest.transform.size.u,
                    v: rest.transform.size.v,
                },
            },
        }),
    }
}

pub const fn convert_actor_layout_lease(lease: brood_wasm::LayoutLeaseToken) -> LayoutLeaseToken {
    LayoutLeaseToken {
        id: LayoutLeaseId(lease.id),
        generation: lease.generation,
    }
}

pub fn native_flat_layout(panes: &[u64], extent: LayoutSize) -> ResolvedLayout {
    if panes.is_empty() {
        return ResolvedLayout::empty();
    }

    let columns = grid_columns(panes.len(), extent);
    let rows = panes.len().div_ceil(columns);
    let width = extent.w.max(1);
    let height = extent.h.max(1);

    ResolvedLayout {
        items: panes
            .iter()
            .copied()
            .enumerate()
            .map(|(index, pane)| {
                let row = index / columns;
                let row_start = row * columns;
                let panes_in_row = columns.min(panes.len() - row_start);
                let column = index - row_start;
                let left = width * i32::try_from(column).unwrap_or(i32::MAX)
                    / i32::try_from(panes_in_row).unwrap_or(i32::MAX);
                let right = width * i32::try_from(column + 1).unwrap_or(i32::MAX)
                    / i32::try_from(panes_in_row).unwrap_or(i32::MAX);
                let top = height * i32::try_from(row).unwrap_or(i32::MAX)
                    / i32::try_from(rows).unwrap_or(i32::MAX);
                let bottom = height * i32::try_from(row + 1).unwrap_or(i32::MAX)
                    / i32::try_from(rows).unwrap_or(i32::MAX);
                let rect = LayoutRect {
                    x: left,
                    y: top,
                    w: right - left,
                    h: bottom - top,
                };
                ResolvedLayoutItem {
                    id: LayoutItemId(pane),
                    kind: LayoutItemKind::Pane { pane },
                    z: index as f64,
                    transform: LocalTransform::from_pixel_rect(rect, extent),
                    input: InputBehavior::Pane,
                }
            })
            .collect(),
    }
}

fn grid_columns(pane_count: usize, extent: LayoutSize) -> usize {
    const TARGET_TILE_ASPECT: f64 = 1.35;

    let viewport_aspect = f64::from(extent.w.max(1)) / f64::from(extent.h.max(1));
    let mut best_columns = 1;
    let mut best_score = f64::MAX;
    for columns in 1..=pane_count {
        let rows = pane_count.div_ceil(columns);
        let empty = rows * columns - pane_count;
        let tile_aspect = viewport_aspect * rows as f64 / columns as f64;
        let score = (empty as f64 / pane_count as f64)
            .mul_add(0.25, (tile_aspect - TARGET_TILE_ASPECT).abs());
        if score < best_score {
            best_score = score;
            best_columns = columns;
        }
    }
    best_columns
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_fallback_keeps_many_panes_large_enough_to_use() {
        let extent = LayoutSize { w: 1600, h: 900 };
        let panes = (1..=12).collect::<Vec<_>>();
        let layout = native_flat_layout(&panes, extent);

        assert_eq!(layout.items.len(), panes.len());
        for item in layout.items {
            let rect = item.pixel_rect(extent);
            assert!(rect.w >= 390, "pane width was only {}", rect.w);
            assert!(rect.h >= 290, "pane height was only {}", rect.h);
        }
    }

    #[test]
    fn balanced_fallback_adapts_to_portrait_viewports() {
        let extent = LayoutSize { w: 600, h: 1200 };
        let layout = native_flat_layout(&[1, 2, 3], extent);
        let rects = layout
            .items
            .iter()
            .map(|item| item.pixel_rect(extent))
            .collect::<Vec<_>>();

        assert!(rects.iter().all(|rect| rect.w == 600));
        assert!(rects.iter().all(|rect| rect.h == 400));
    }
}
