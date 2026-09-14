//! Typed guest-to-host layout protocol values.
//!
//! Plugins build these values in memory and serialize them only when emitting
//! a host message. Keeping layout inspection typed avoids the fragile pattern
//! of formatting JSON and immediately parsing that JSON back into a value.
//!
//! Incoming `layout-state` messages distinguish `owned_panes` (all lease
//! members, including inactive sessions and panes without a first frame) from
//! `visible_panes` (the current presentation candidates). Delegate ownership
//! using `owned_panes`; selecting only visible panes can strand children when
//! their rendering or session activation finishes after the delegation.

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct AxisValue {
    pub u: f64,
    pub v: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Transform {
    pub offset: AxisValue,
    pub size: AxisValue,
}

impl Transform {
    #[must_use]
    pub const fn new(offset_u: f64, offset_v: f64, size_u: f64, size_v: f64) -> Self {
        Self {
            offset: AxisValue {
                u: offset_u,
                v: offset_v,
            },
            size: AxisValue {
                u: size_u,
                v: size_v,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub enum ItemKind {
    #[serde(rename = "pane")]
    Pane { pane: u64 },
    #[serde(rename = "overlay-pane")]
    OverlayPane { pane: u64 },
}

impl ItemKind {
    #[must_use]
    pub const fn pane(self) -> u64 {
        match self {
            Self::Pane { pane } | Self::OverlayPane { pane } => pane,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LayoutItem {
    pub id: u64,
    pub kind: ItemKind,
    pub z: i32,
    pub transform: Transform,
    pub input: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DividerItem {
    pub id: u64,
    pub zone: u64,
    pub z: i32,
    pub transform: Transform,
    pub axis: &'static str,
    pub role: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InputZone {
    pub id: u64,
    pub transform: Transform,
    pub z: i32,
    pub event_mask: u32,
    pub cursor: &'static str,
    pub input: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ResolvedLayout {
    pub items: Vec<LayoutItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub zones: Vec<InputZone>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dividers: Vec<DividerItem>,
}

impl ResolvedLayout {
    pub fn pane_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.items.iter().map(|item| item.kind.pane())
    }

    #[must_use]
    pub fn pane_extent(&self, pane: u64, axis: Axis, pixels: f64) -> f64 {
        self.items
            .iter()
            .find(|item| item.kind.pane() == pane)
            .map_or(0.0, |item| {
                let fraction = match axis {
                    Axis::Horizontal => item.transform.size.u,
                    Axis::Vertical => item.transform.size.v,
                };
                fraction * pixels / 2.0
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LayoutRest {
    pub z: i32,
    pub transform: Transform,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LayoutFragment {
    pub layout: ResolvedLayout,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rest: Option<LayoutRest>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Lease {
    pub id: u64,
    pub generation: u64,
}

#[derive(Serialize)]
pub struct SetPointerPolicy {
    #[serde(rename = "type")]
    pub message_type: &'static str,
    pub lease: Lease,
    pub provider: &'static str,
}

#[derive(Serialize)]
pub struct DelegateLayoutRest<'a> {
    #[serde(rename = "type")]
    pub message_type: &'static str,
    pub lease: Lease,
    pub revision: u64,
    pub actor: &'static str,
    pub panes: &'a [u64],
}

#[derive(Serialize)]
pub struct SetResolvedLayout<'a> {
    #[serde(rename = "type")]
    pub message_type: &'static str,
    pub lease: Lease,
    pub revision: u64,
    pub layout: &'a ResolvedLayout,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rest: Option<&'a LayoutRest>,
    pub focus: Option<u64>,
}

#[derive(Serialize)]
pub struct SubmitSynchronizedLayout<'a> {
    #[serde(rename = "type")]
    pub message_type: &'static str,
    pub request_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_sequence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interaction: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_resize_revision: Option<u64>,
    pub lease: Lease,
    pub inventory_revision: u64,
    pub base_layout_revision: u64,
    pub fragment: &'a LayoutFragment,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<u64>,
    pub wait_for_panes: &'a [u64],
    pub presentation: &'static str,
}

#[derive(Serialize)]
pub struct SubmitLayoutRegion<'a, E> {
    #[serde(rename = "type")]
    pub message_type: &'static str,
    pub resize_epoch: E,
    pub lease: Lease,
    pub inventory_revision: u64,
    pub fragment: &'a LayoutFragment,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<u64>,
}
