use manifold_layout::{
    InputBehavior, LayoutAxis, LayoutChildSlot, LayoutFragment, LayoutItemId, LayoutItemKind,
    LayoutLeaseId, LayoutLeaseToken, LayoutRect, LayoutSize, LayoutState, LocalPoint,
    LocalTransform, ResolvedLayout, ResolvedLayoutItem,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutSource {
    NativeFallback,
    Actor,
}

pub fn convert_actor_layout(layout: manifold_wasm::ResolvedLayout) -> ResolvedLayout {
    ResolvedLayout {
        items: layout
            .items
            .into_iter()
            .map(|item| ResolvedLayoutItem {
                id: LayoutItemId(item.id.0),
                kind: match item.kind {
                    manifold_wasm::LayoutItemKind::Pane { pane } => LayoutItemKind::Pane { pane },
                    manifold_wasm::LayoutItemKind::Placeholder => LayoutItemKind::Placeholder,
                    manifold_wasm::LayoutItemKind::Handle => LayoutItemKind::Handle,
                    manifold_wasm::LayoutItemKind::Chrome => LayoutItemKind::Chrome,
                    manifold_wasm::LayoutItemKind::Overlay => LayoutItemKind::Overlay,
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
                    manifold_wasm::InputBehavior::None => InputBehavior::None,
                    manifold_wasm::InputBehavior::Hit => InputBehavior::Hit,
                    manifold_wasm::InputBehavior::Drag => InputBehavior::Drag,
                    manifold_wasm::InputBehavior::Pane => InputBehavior::Pane,
                },
            })
            .collect(),
    }
}

pub fn convert_actor_layout_fragment(
    layout: manifold_wasm::ResolvedLayout,
    rest: Option<manifold_wasm::LayoutChildSlot>,
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

pub const fn convert_actor_layout_lease(
    lease: manifold_wasm::LayoutLeaseToken,
) -> LayoutLeaseToken {
    LayoutLeaseToken {
        id: LayoutLeaseId(lease.id),
        generation: lease.generation,
    }
}

pub fn native_flat_layout(panes: &[u64], extent: LayoutSize) -> ResolvedLayout {
    if panes.is_empty() {
        return ResolvedLayout::empty();
    }

    let mut tree = LayoutState::default();
    for _ in 1..panes.len() {
        let active_rect = tree.active_rect(extent).unwrap_or(LayoutRect {
            x: 0,
            y: 0,
            w: extent.w,
            h: extent.h,
        });
        let axis = if active_rect.w >= active_rect.h {
            LayoutAxis::Horizontal
        } else {
            LayoutAxis::Vertical
        };
        let Some(next_leaf) = tree.split_active(axis) else {
            break;
        };
        tree.set_active_leaf(next_leaf);
    }

    ResolvedLayout {
        items: panes
            .iter()
            .copied()
            .zip(tree.resolve(extent))
            .enumerate()
            .map(|(index, (pane, region))| ResolvedLayoutItem {
                id: LayoutItemId(pane),
                kind: LayoutItemKind::Pane { pane },
                z: index as f64,
                transform: LocalTransform::from_pixel_rect(region.rect, extent),
                input: InputBehavior::Pane,
            })
            .collect(),
    }
}
