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
        Self { items: Vec::new() }
    }
}
