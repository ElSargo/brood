//! Wire types for actor-submitted presentation layouts.
//!
//! Layout actors send these values to the host in a normal `SetResolvedLayout`
//! mailbox message. Presentation hosts convert them into `brood-layout`
//! values, validate them against their current pane inventory, and only then
//! commit them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResolvedLayout {
    pub items: Vec<ResolvedLayoutItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub zones: Vec<InteractionZone>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dividers: Vec<LayoutDivider>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutLeaseToken {
    pub id: u64,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct LayoutChildSlot {
    pub z: f64,
    pub transform: LocalTransform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LayoutItemId(pub u64);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PointerEventMask(pub u8);

impl PointerEventMask {
    pub const BUTTON: Self = Self(1 << 0);
    pub const BUTTON_PRESS: Self = Self::BUTTON;
    pub const MOTION: Self = Self(1 << 1);
    pub const RELEASE: Self = Self(1 << 2);
    pub const CANCEL: Self = Self(1 << 3);
    pub const SUPPORTED: Self =
        Self(Self::BUTTON.0 | Self::MOTION.0 | Self::RELEASE.0 | Self::CANCEL.0);

    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn from_bits(bits: u8) -> Option<Self> {
        if bits & !Self::SUPPORTED.0 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl std::ops::BitOr for PointerEventMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for PointerEventMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CursorRole {
    Default,
    Pointer,
    ColResize,
    RowResize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ZoneInputBehavior {
    Hit,
    Drag,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct InteractionZone {
    pub id: LayoutItemId,
    pub transform: LocalTransform,
    pub z: f64,
    pub event_mask: PointerEventMask,
    pub cursor: CursorRole,
    pub input: ZoneInputBehavior,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DividerAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DividerRole {
    Split,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct LayoutDivider {
    pub id: LayoutItemId,
    pub zone: LayoutItemId,
    pub z: f64,
    pub transform: LocalTransform,
    pub axis: DividerAxis,
    pub role: DividerRole,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct ResolvedLayoutItem {
    pub id: LayoutItemId,
    pub kind: LayoutItemKind,
    pub z: f64,
    pub transform: LocalTransform,
    pub input: InputBehavior,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LayoutItemKind {
    Pane { pane: u64 },
    Placeholder,
    Handle,
    Chrome,
    Overlay,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InputBehavior {
    None,
    Hit,
    Drag,
    Pane,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct LocalPoint {
    pub u: f64,
    pub v: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct LocalTransform {
    pub offset: LocalPoint,
    pub size: LocalPoint,
}

impl ResolvedLayout {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            items: Vec::new(),
            zones: Vec::new(),
            dividers: Vec::new(),
        }
    }
}
