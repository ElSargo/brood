//! Portable identities and presentation modes for synchronized resize work.
//!
//! These types intentionally carry no renderer or actor-runtime state. The
//! presentation client creates epochs after validating an actor request and
//! uses the same value when notifying panes and accepting tagged content.

use serde::{Deserialize, Serialize};

use crate::resolved_layout::{LayoutChildSlot, LayoutLeaseToken, ResolvedLayout};

/// Stable identity for one captured pointer interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InteractionId(pub u64);

/// One host-accepted target geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeEpoch {
    pub id: u64,
    pub lease: u64,
    pub lease_generation: u64,
    pub base_layout_revision: u64,
    pub target_layout_revision: u64,
}

/// Presentation behavior while content for an accepted resize is prepared.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PresentationMode {
    Independent,
    HoldThenAtomic,
    #[default]
    ScaleThenAtomic,
}

/// One lease-scoped layout fragment proposed by a layout actor.
///
/// Interaction zones and divider chrome are carried by [`ResolvedLayout`].
/// The optional `rest` allocation is kept beside it because it delegates the
/// child lease viewport rather than painting an ordinary layout item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynchronizedLayoutFragment {
    pub layout: ResolvedLayout,
    #[serde(default)]
    pub rest: Option<LayoutChildSlot>,
}

/// One actor-owned proposal for a synchronized geometry and paint update.
///
/// The host creates a [`ResizeEpoch`] only after validating the complete
/// request, including the fixed wait set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubmitSynchronizedLayout {
    pub request_id: u64,
    #[serde(default)]
    pub interaction: Option<InteractionId>,
    #[serde(default)]
    pub event_sequence: Option<u64>,
    /// Host-issued identity for a settled native-window resize.
    ///
    /// Pointer-driven requests leave this unset. A layout actor echoes the
    /// current value from its lease-scoped layout state so the host can reject
    /// a late response from an older native viewport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_resize_revision: Option<u64>,
    pub lease: LayoutLeaseToken,
    pub inventory_revision: u64,
    pub base_layout_revision: u64,
    pub fragment: SynchronizedLayoutFragment,
    /// Focus to promote in the same presented frame as the target fragment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<u64>,
    pub wait_for_panes: Vec<u64>,
    #[serde(default)]
    pub presentation: PresentationMode,
}

/// Mark one pane ready for an epoch by retaining its current visible content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReusePaneContent {
    pub pane: u64,
    pub resize_epoch: u64,
}

/// Stable rejection classes which actors can handle without parsing prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SynchronizedLayoutRejectionReason {
    InvalidRequest,
    StaleInteraction,
    StaleEventSequence,
    StaleLease,
    StaleInventoryRevision,
    StaleBaseLayoutRevision,
    UnauthorizedPane,
    InvalidRestSlot,
    InvalidInteractionZone,
    DuplicateInteractionZone,
    DuplicateWaitPane,
    WaitPaneOutsideSubtree,
    IneligibleWaitPane,
    LimitExceeded,
    Busy,
}

/// The host accepted the request and fixed the epoch and pane dependencies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynchronizedLayoutAccepted {
    pub request_id: u64,
    pub resize_epoch: ResizeEpoch,
    pub affected_panes: Vec<u64>,
    pub wait_for_panes: Vec<u64>,
}

/// The accepted geometry and required content became visible atomically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynchronizedLayoutCompleted {
    pub request_id: u64,
    pub resize_epoch: ResizeEpoch,
}

/// Validation rejected a request before it changed visible or staged state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynchronizedLayoutRejected {
    pub request_id: u64,
    pub reason: SynchronizedLayoutRejectionReason,
    pub message: String,
}

/// A newer request from the same interaction replaced an accepted request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynchronizedLayoutSuperseded {
    pub request_id: u64,
    pub resize_epoch: ResizeEpoch,
    pub superseding_request_id: u64,
    pub superseding_resize_epoch: ResizeEpoch,
}

/// The host presented bounded recovery content after an epoch deadline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynchronizedLayoutDegraded {
    pub request_id: u64,
    pub resize_epoch: ResizeEpoch,
    pub missing_panes: Vec<u64>,
}

/// Typed body for host-to-layout-actor transaction notifications.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum SynchronizedLayoutNotification {
    Accepted(SynchronizedLayoutAccepted),
    Completed(SynchronizedLayoutCompleted),
    Rejected(SynchronizedLayoutRejected),
    Superseded(SynchronizedLayoutSuperseded),
    Degraded(SynchronizedLayoutDegraded),
}
