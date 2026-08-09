use brood_layout::{
    ComposedDivider, ComposedDividerId, ComposedInteractionZone, ComposedZoneId, CursorRole,
    DividerAxis, DividerRole, InputBehavior, InteractionZone, LayoutChildSlot, LayoutDivider,
    LayoutFragment, LayoutItemId, LayoutItemKind, LayoutLeaseId, LayoutLeaseToken, LayoutRect,
    LayoutSize, LocalPoint, LocalTransform, PointerEventMask, ResolvedLayout, ResolvedLayoutItem,
    ZoneInputBehavior,
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
                transform: convert_actor_transform(item.transform),
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
    let zones = layout
        .zones
        .iter()
        .map(|zone| InteractionZone {
            id: LayoutItemId(zone.id.0),
            transform: convert_actor_transform(zone.transform),
            z: zone.z,
            event_mask: PointerEventMask::from_bits_retain(zone.event_mask.bits()),
            cursor: match zone.cursor {
                brood_wasm::CursorRole::Default => CursorRole::Default,
                brood_wasm::CursorRole::Pointer => CursorRole::Pointer,
                brood_wasm::CursorRole::ColResize => CursorRole::ColResize,
                brood_wasm::CursorRole::RowResize => CursorRole::RowResize,
            },
            input: match zone.input {
                brood_wasm::ZoneInputBehavior::Hit => ZoneInputBehavior::Hit,
                brood_wasm::ZoneInputBehavior::Drag => ZoneInputBehavior::Drag,
            },
        })
        .collect();
    let dividers = layout
        .dividers
        .iter()
        .map(|divider| LayoutDivider {
            id: LayoutItemId(divider.id.0),
            zone: LayoutItemId(divider.zone.0),
            z: divider.z,
            transform: convert_actor_transform(divider.transform),
            axis: match divider.axis {
                brood_wasm::DividerAxis::Horizontal => DividerAxis::Horizontal,
                brood_wasm::DividerAxis::Vertical => DividerAxis::Vertical,
            },
            role: match divider.role {
                brood_wasm::DividerRole::Split => DividerRole::Split,
            },
        })
        .collect();
    LayoutFragment {
        layout: convert_actor_layout(layout),
        zones,
        dividers,
        rest: rest.map(|rest| LayoutChildSlot {
            z: rest.z,
            transform: convert_actor_transform(rest.transform),
        }),
    }
}

const fn convert_actor_transform(transform: brood_wasm::LocalTransform) -> LocalTransform {
    LocalTransform {
        offset: LocalPoint {
            u: transform.offset.u,
            v: transform.offset.v,
        },
        size: LocalPoint {
            u: transform.size.u,
            v: transform.size.v,
        },
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

/// Shared one-segment chrome for every exact adjacency in the native fallback.
/// The fallback has no actor policy, so the synthetic zone identity is used
/// only to keep divider hover/capture state structurally compatible.
pub fn native_flat_dividers(layout: &ResolvedLayout, extent: LayoutSize) -> Vec<ComposedDivider> {
    let rects = layout
        .items
        .iter()
        .filter_map(|item| match item.kind {
            LayoutItemKind::Pane { .. } => Some(item.pixel_rect(extent)),
            LayoutItemKind::Placeholder
            | LayoutItemKind::Handle
            | LayoutItemKind::Chrome
            | LayoutItemKind::Overlay => None,
        })
        .collect::<Vec<_>>();
    let mut segments = std::collections::BTreeSet::new();
    for (index, left) in rects.iter().enumerate() {
        for right in rects.iter().skip(index + 1) {
            let left_right = left.x.saturating_add(left.w);
            let right_right = right.x.saturating_add(right.w);
            if left_right == right.x || right_right == left.x {
                let x = if left_right == right.x {
                    right.x
                } else {
                    left.x
                };
                let top = left.y.max(right.y);
                let bottom = left
                    .y
                    .saturating_add(left.h)
                    .min(right.y.saturating_add(right.h));
                if bottom > top {
                    segments.insert((0_u8, x, top, bottom - top));
                }
            }
            let left_bottom = left.y.saturating_add(left.h);
            let right_bottom = right.y.saturating_add(right.h);
            if left_bottom == right.y || right_bottom == left.y {
                let y = if left_bottom == right.y {
                    right.y
                } else {
                    left.y
                };
                let start = left.x.max(right.x);
                let end = left
                    .x
                    .saturating_add(left.w)
                    .min(right.x.saturating_add(right.w));
                if end > start {
                    segments.insert((1_u8, y, start, end - start));
                }
            }
        }
    }
    segments
        .into_iter()
        .enumerate()
        .map(|(index, (axis, fixed, start, length))| {
            let identity = u64::MAX.saturating_sub(index as u64 * 2 + 1);
            let zone = ComposedZoneId {
                lease: LayoutLeaseId(1),
                lease_generation: 0,
                local_item: LayoutItemId(identity),
            };
            // Divider transforms describe a line centred on the shared pane
            // boundary. Keeping that convention in the fallback makes native
            // device-pixel snapping agree with actor-supplied dividers even
            // when the split falls between physical pixel centres.
            let extent_w = f64::from(extent.w.max(1));
            let extent_h = f64::from(extent.h.max(1));
            let transform = if axis == 0 {
                LocalTransform {
                    offset: LocalPoint {
                        u: (f64::from(fixed) / extent_w).mul_add(2.0, -1.0) - 1.0 / extent_w,
                        v: (f64::from(start) / extent_h).mul_add(2.0, -1.0),
                    },
                    size: LocalPoint {
                        u: 2.0 / extent_w,
                        v: (f64::from(length) / extent_h) * 2.0,
                    },
                }
            } else {
                LocalTransform {
                    offset: LocalPoint {
                        u: (f64::from(start) / extent_w).mul_add(2.0, -1.0),
                        v: (f64::from(fixed) / extent_h).mul_add(2.0, -1.0) - 1.0 / extent_h,
                    },
                    size: LocalPoint {
                        u: (f64::from(length) / extent_w) * 2.0,
                        v: 2.0 / extent_h,
                    },
                }
            };
            ComposedDivider {
                id: ComposedDividerId {
                    lease: zone.lease,
                    lease_generation: zone.lease_generation,
                    local_item: LayoutItemId(identity.saturating_sub(1)),
                },
                zone,
                z: layout.items.len() as f64 + index as f64,
                transform,
                axis: if axis == 0 {
                    DividerAxis::Vertical
                } else {
                    DividerAxis::Horizontal
                },
                role: DividerRole::Split,
            }
        })
        .collect()
}

/// Invisible hit regions centred on native fallback dividers.
///
/// The visual line remains one device pixel after snapping, while the hit
/// region is eight physical pixels wide so hover feedback does not require
/// pixel-perfect pointing.
pub fn native_flat_interaction_zones(
    dividers: &[ComposedDivider],
    extent: LayoutSize,
) -> Vec<ComposedInteractionZone> {
    let extent_w = f64::from(extent.w.max(1));
    let extent_h = f64::from(extent.h.max(1));
    dividers
        .iter()
        .map(|divider| {
            let mut transform = divider.transform;
            match divider.axis {
                DividerAxis::Vertical => {
                    let centre = transform.offset.u + transform.size.u / 2.0;
                    transform.size.u = 16.0 / extent_w;
                    transform.offset.u = centre - transform.size.u / 2.0;
                }
                DividerAxis::Horizontal => {
                    let centre = transform.offset.v + transform.size.v / 2.0;
                    transform.size.v = 16.0 / extent_h;
                    transform.offset.v = centre - transform.size.v / 2.0;
                }
            }
            ComposedInteractionZone {
                id: divider.zone,
                transform,
                lease_viewport: LocalTransform {
                    offset: LocalPoint { u: -1.0, v: -1.0 },
                    size: LocalPoint { u: 2.0, v: 2.0 },
                },
                z: divider.z,
                event_mask: PointerEventMask::SUPPORTED,
                cursor: match divider.axis {
                    DividerAxis::Vertical => CursorRole::ColResize,
                    DividerAxis::Horizontal => CursorRole::RowResize,
                },
                input: ZoneInputBehavior::Drag,
            }
        })
        .collect()
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

    const fn wire_transform(u: f64, v: f64, width: f64, height: f64) -> brood_wasm::LocalTransform {
        brood_wasm::LocalTransform {
            offset: brood_wasm::LocalPoint { u, v },
            size: brood_wasm::LocalPoint {
                u: width,
                v: height,
            },
        }
    }

    #[test]
    fn legacy_actor_layout_json_defaults_new_fragment_surfaces() {
        let parsed = serde_json::from_str::<brood_wasm::ResolvedLayout>(r#"{"items":[]}"#);
        assert!(parsed.is_ok_and(|layout| {
            layout.items.is_empty() && layout.zones.is_empty() && layout.dividers.is_empty()
        }));
    }

    #[test]
    fn actor_fragment_conversion_preserves_zones_dividers_and_rest() {
        let wire_layout = brood_wasm::ResolvedLayout {
            items: vec![brood_wasm::ResolvedLayoutItem {
                id: brood_wasm::LayoutItemId(1),
                kind: brood_wasm::LayoutItemKind::Pane { pane: 41 },
                z: 0.0,
                transform: wire_transform(-1.0, -1.0, 2.0, 2.0),
                input: brood_wasm::InputBehavior::Pane,
            }],
            zones: vec![brood_wasm::InteractionZone {
                id: brood_wasm::LayoutItemId(2),
                transform: wire_transform(-0.05, -1.0, 0.1, 2.0),
                z: 4.0,
                event_mask: brood_wasm::PointerEventMask::BUTTON
                    | brood_wasm::PointerEventMask::MOTION
                    | brood_wasm::PointerEventMask::RELEASE
                    | brood_wasm::PointerEventMask::CANCEL,
                cursor: brood_wasm::CursorRole::ColResize,
                input: brood_wasm::ZoneInputBehavior::Drag,
            }],
            dividers: vec![brood_wasm::LayoutDivider {
                id: brood_wasm::LayoutItemId(3),
                zone: brood_wasm::LayoutItemId(2),
                z: 5.0,
                transform: wire_transform(-0.005, -1.0, 0.01, 2.0),
                axis: brood_wasm::DividerAxis::Vertical,
                role: brood_wasm::DividerRole::Split,
            }],
        };
        let fragment = convert_actor_layout_fragment(
            wire_layout,
            Some(brood_wasm::LayoutChildSlot {
                z: 6.0,
                transform: wire_transform(0.0, -1.0, 1.0, 2.0),
            }),
        );

        assert_eq!(fragment.layout.items.len(), 1);
        assert_eq!(
            fragment.zones,
            vec![InteractionZone {
                id: LayoutItemId(2),
                transform: LocalTransform {
                    offset: LocalPoint { u: -0.05, v: -1.0 },
                    size: LocalPoint { u: 0.1, v: 2.0 },
                },
                z: 4.0,
                event_mask: PointerEventMask::SUPPORTED,
                cursor: CursorRole::ColResize,
                input: ZoneInputBehavior::Drag,
            }]
        );
        assert_eq!(
            fragment.dividers,
            vec![LayoutDivider {
                id: LayoutItemId(3),
                zone: LayoutItemId(2),
                z: 5.0,
                transform: LocalTransform {
                    offset: LocalPoint { u: -0.005, v: -1.0 },
                    size: LocalPoint { u: 0.01, v: 2.0 },
                },
                axis: DividerAxis::Vertical,
                role: DividerRole::Split,
            }]
        );
        assert_eq!(
            fragment.rest,
            Some(LayoutChildSlot {
                z: 6.0,
                transform: LocalTransform {
                    offset: LocalPoint { u: 0.0, v: -1.0 },
                    size: LocalPoint { u: 1.0, v: 2.0 },
                },
            })
        );
    }

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

    #[test]
    fn fallback_draws_each_shared_boundary_once() {
        let extent = LayoutSize { w: 1000, h: 600 };
        let layout = native_flat_layout(&[1, 2], extent);
        let dividers = native_flat_dividers(&layout, extent);

        assert_eq!(dividers.len(), 1);
        let Some(divider) = dividers.first().copied() else {
            panic!("native fallback divider is missing");
        };
        assert_eq!(divider.axis, DividerAxis::Vertical);
        assert_eq!(divider.pixel_rect(extent).x, 500);
        assert_eq!(divider.pixel_rect(extent).h, 600);
        let zones = native_flat_interaction_zones(&dividers, extent);
        assert_eq!(zones.len(), 1);
        let Some(zone) = zones.first() else {
            panic!("native fallback interaction zone is missing");
        };
        assert_eq!(zone.transform.pixel_rect(extent).x, 496);
        assert_eq!(zone.transform.pixel_rect(extent).w, 8);
        assert_eq!(zone.cursor, CursorRole::ColResize);
        assert_eq!(zone.input, ZoneInputBehavior::Drag);
    }
}
