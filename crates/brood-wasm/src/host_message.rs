//! Common actor-to-host commands.
//!
//! These messages leave ordinary authenticated actor outboxes. They are not
//! part of any synchronous policy provider's authority, even when a policy
//! enqueues the actor message that eventually produces one.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum HostMessage {
    LayoutAction {
        action: String,
    },
    FocusPane {
        pane: u64,
    },
    SpawnApp {
        argv: Vec<String>,
    },
    FocusOrSpawnApp {
        app_id: String,
        argv: Vec<String>,
    },
    SwitchActiveSessionNext,
    CreateAndAttachSession,
    BackendSession {
        backend: u64,
        request: u64,
        operation: String,
        session: Option<u64>,
        name: Option<String>,
        directory: Option<String>,
    },
    ConnectRemoteBackend {
        address: String,
    },
    DeploySshBackend {
        target: String,
    },
    DeployContainerBackend {
        image: String,
        connection: Option<String>,
    },
    RequestBackendCatalogue,
    CreateLocalBackend,
    ComplaintCapture {
        action: String,
    },
    ToggleShortcutPassthrough,
    SetShortcutPassthrough {
        enabled: bool,
    },
    SetKeybinding {
        action: String,
        key_code: i32,
        modifiers: i32,
    },
    SetClipboard {
        text: String,
    },
    RequestClipboardPaste {
        pane: u64,
    },
    SetResolvedLayout {
        #[serde(default)]
        lease: Option<crate::resolved_layout::LayoutLeaseToken>,
        #[serde(default)]
        revision: Option<u64>,
        layout: crate::resolved_layout::ResolvedLayout,
        #[serde(default)]
        rest: Option<crate::resolved_layout::LayoutChildSlot>,
        focus: Option<u64>,
    },
    ClaimRootLayout,
    DelegateLayoutRest {
        lease: crate::resolved_layout::LayoutLeaseToken,
        revision: u64,
        actor: String,
        panes: Vec<u64>,
    },
    SetPointerPolicy {
        lease: crate::resolved_layout::LayoutLeaseToken,
        provider: String,
    },
    SubmitSynchronizedLayout(crate::resize::SubmitSynchronizedLayout),
    ReusePaneContent(crate::resize::ReusePaneContent),
    SubmitLayoutRegion(crate::resize::SubmitLayoutRegion),
    ReuseLayoutRegion(crate::resize::ReuseLayoutRegion),
    FailLayoutRegion(crate::resize::FailLayoutRegion),
}
