//! Stable, toolkit-independent managed UI protocol.
//!
//! These types deliberately contain no host UI framework objects. A plugin
//! publishes complete documents or atomic patches; a presentation adapter
//! validates and reconciles them into its native entity tree.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// First managed-UI wire version.
pub const MANAGED_UI_PROTOCOL_VERSION: u32 = 1;

/// Optional integration nodes negotiated independently of the core semantic vocabulary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiCapability {
    Canvas,
    ExternalSurface,
    TerminalView,
    TextEditorBridge,
    DragAndDrop,
}

/// Host offer returned to an actor before its first publication.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UiCapabilities {
    pub protocol_versions: Vec<u32>,
    pub optional_nodes: BTreeSet<UiCapability>,
    pub limits: UiLimits,
}

impl UiCapabilities {
    #[must_use]
    pub fn gpui_host(limits: UiLimits) -> Self {
        Self {
            protocol_versions: vec![MANAGED_UI_PROTOCOL_VERSION],
            optional_nodes: BTreeSet::from([
                UiCapability::Canvas,
                UiCapability::ExternalSurface,
                UiCapability::TerminalView,
                UiCapability::TextEditorBridge,
                UiCapability::DragAndDrop,
            ]),
            limits,
        }
    }
}

/// Stable node identity within one actor generation.
pub type UiNodeId = u64;

/// Stable pane identity within one presentation client.
pub type UiPaneId = u64;

/// Host-advertised resource and reconciliation bounds.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UiLimits {
    pub max_nodes: u32,
    pub max_depth: u16,
    pub max_text_bytes: u32,
    pub max_patch_operations: u32,
    pub max_children_per_node: u32,
    pub max_retained_resource_bytes: u64,
    pub max_outstanding_actions: u32,
}

impl Default for UiLimits {
    fn default() -> Self {
        Self {
            max_nodes: 16_384,
            max_depth: 128,
            max_text_bytes: 4 * 1024 * 1024,
            max_patch_operations: 4_096,
            max_children_per_node: 4_096,
            max_retained_resource_bytes: 128 * 1024 * 1024,
            max_outstanding_actions: 1_024,
        }
    }
}

/// Which side is authoritative for a mutable interactive value.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiStateMode {
    #[default]
    Controlled,
    HostTransient,
    Hybrid,
}

/// Host theme token. Plugins may select semantics, not implementation colors.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiColorToken {
    #[default]
    Foreground,
    MutedForeground,
    Background,
    Surface,
    Border,
    Accent,
    Danger,
    Success,
    Warning,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiTextRole {
    Caption,
    #[default]
    Body,
    Label,
    Heading,
    Monospace,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiAxis {
    #[default]
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiAlignment {
    Start,
    Center,
    End,
    #[default]
    Stretch,
    SpaceBetween,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct UiInsets {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Default for UiInsets {
    fn default() -> Self {
        Self {
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        }
    }
}

/// Framework-neutral layout and visual properties shared by all nodes.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UiStyle {
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub min_width: Option<f32>,
    pub min_height: Option<f32>,
    pub max_width: Option<f32>,
    pub max_height: Option<f32>,
    pub grow: f32,
    pub shrink: f32,
    pub gap: f32,
    pub padding: UiInsets,
    pub align: UiAlignment,
    pub background: Option<UiColorToken>,
    pub foreground: Option<UiColorToken>,
    pub radius: f32,
    pub hidden: bool,
}

/// Accessibility information projected by the host.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UiAccessibility {
    pub role: UiAccessibilityRole,
    pub label: Option<String>,
    pub description: Option<String>,
    pub value: Option<String>,
    pub disabled: bool,
    pub expanded: Option<bool>,
    pub selected: Option<bool>,
    #[serde(default)]
    pub labelled_by: Vec<UiNodeId>,
    #[serde(default)]
    pub described_by: Vec<UiNodeId>,
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiAccessibilityRole {
    #[default]
    Generic,
    Button,
    Checkbox,
    RadioGroup,
    TextField,
    SearchBox,
    List,
    ListItem,
    Link,
    Dialog,
    ProgressIndicator,
    Terminal,
    Image,
}

/// Finite component vocabulary understood by a version-1 host.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum UiNodeKind {
    Row,
    Column,
    Stack,
    Grid {
        columns: u16,
    },
    Spacer,
    ScrollRegion {
        axis: UiAxis,
    },
    Divider {
        axis: UiAxis,
    },
    VirtualList {
        item_count: u64,
        estimated_item_extent: f32,
    },
    Text {
        text: String,
        role: UiTextRole,
    },
    Icon {
        name: String,
    },
    Image {
        resource: u64,
        alt: String,
    },
    Badge {
        text: String,
    },
    Progress {
        value: Option<f32>,
    },
    Separator {
        axis: UiAxis,
    },
    Button {
        label: String,
    },
    Toggle {
        label: String,
        value: bool,
        state_mode: UiStateMode,
    },
    RadioGroup {
        value: Option<String>,
        state_mode: UiStateMode,
    },
    Menu {
        label: String,
    },
    SelectableRow {
        selected: bool,
    },
    Link {
        label: String,
        target: String,
    },
    CommandTarget {
        command: String,
    },
    TextField {
        value: String,
        placeholder: String,
        state_mode: UiStateMode,
    },
    MultilineEditor {
        value: String,
        state_mode: UiStateMode,
    },
    SearchField {
        value: String,
        placeholder: String,
        state_mode: UiStateMode,
    },
    TextEditorBridge {
        editor: u64,
    },
    Tooltip {
        text: String,
    },
    Popover,
    Dialog {
        title: String,
    },
    ContextMenu,
    DragSource {
        payload_type: String,
    },
    DropTarget {
        accepted_types: Vec<String>,
    },
    Canvas {
        target: u64,
    },
    ExternalSurface {
        surface: u64,
    },
    TerminalView {
        terminal: u64,
    },
    #[serde(other)]
    Unsupported,
}

impl UiNodeKind {
    fn text_bytes(&self) -> usize {
        match self {
            Self::Text { text, .. } | Self::Badge { text } | Self::Tooltip { text } => text.len(),
            Self::Icon { name } => name.len(),
            Self::Image { alt, .. } => alt.len(),
            Self::Button { label } | Self::Menu { label } | Self::Toggle { label, .. } => {
                label.len()
            }
            Self::Link { label, target } => label.len().saturating_add(target.len()),
            Self::CommandTarget { command } => command.len(),
            Self::RadioGroup { value, .. } => value.as_ref().map_or(0, String::len),
            Self::TextField {
                value, placeholder, ..
            }
            | Self::SearchField {
                value, placeholder, ..
            } => value.len().saturating_add(placeholder.len()),
            Self::MultilineEditor { value, .. } => value.len(),
            Self::Dialog { title } => title.len(),
            Self::DragSource { payload_type } => payload_type.len(),
            Self::DropTarget { accepted_types } => accepted_types.iter().map(String::len).sum(),
            Self::Row
            | Self::Unsupported
            | Self::Column
            | Self::Stack
            | Self::Grid { .. }
            | Self::Spacer
            | Self::ScrollRegion { .. }
            | Self::Divider { .. }
            | Self::VirtualList { .. }
            | Self::Progress { .. }
            | Self::Separator { .. }
            | Self::SelectableRow { .. }
            | Self::TextEditorBridge { .. }
            | Self::Popover
            | Self::ContextMenu
            | Self::Canvas { .. }
            | Self::ExternalSurface { .. }
            | Self::TerminalView { .. } => 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UiNode {
    pub id: UiNodeId,
    pub kind: UiNodeKind,
    #[serde(default)]
    pub style: UiStyle,
    #[serde(default)]
    pub accessibility: UiAccessibility,
    #[serde(default)]
    pub optional: bool,
    /// Actor acknowledgement for host-side changes to a hybrid value.
    #[serde(default)]
    pub interaction_revision: u64,
    #[serde(default)]
    pub children: Vec<Self>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UiDocument {
    pub protocol_version: u32,
    pub pane: UiPaneId,
    pub document_revision: u64,
    pub root: UiNode,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "kebab-case")]
pub enum UiOperation {
    ReplaceNode {
        node: UiNodeId,
        replacement: UiNode,
    },
    InsertChild {
        parent: UiNodeId,
        index: u32,
        child: UiNode,
    },
    RemoveNode {
        node: UiNodeId,
    },
    MoveChild {
        node: UiNodeId,
        parent: UiNodeId,
        index: u32,
    },
    SetKind {
        node: UiNodeId,
        kind: UiNodeKind,
    },
    SetStyle {
        node: UiNodeId,
        style: UiStyle,
    },
    SetAccessibility {
        node: UiNodeId,
        accessibility: UiAccessibility,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UiPatch {
    pub protocol_version: u32,
    pub pane: UiPaneId,
    pub base_revision: u64,
    pub document_revision: u64,
    pub operations: Vec<UiOperation>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "publication", content = "value", rename_all = "kebab-case")]
pub enum UiPublication {
    Snapshot(Box<UiDocument>),
    Patch(UiPatch),
    RemovePane { pane: UiPaneId },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "value", content = "payload", rename_all = "kebab-case")]
pub enum UiActionValue {
    Activate,
    Toggle(bool),
    Select(String),
    Text(String),
    Submit,
    Dismiss,
    Scroll {
        x: f32,
        y: f32,
    },
    Resize {
        width: f32,
        height: f32,
    },
    Drag {
        payload_type: String,
        bytes: Vec<u8>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UiAction {
    pub pane: UiPaneId,
    pub actor_generation: u64,
    pub document_revision: u64,
    pub node: UiNodeId,
    pub interaction_revision: u64,
    pub value: UiActionValue,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiDiagnosticCode {
    UnsupportedVersion,
    InvalidRevision,
    StalePublication,
    DuplicateNode,
    UnknownNode,
    InvalidTree,
    LimitExceeded,
    InvalidValue,
}

#[derive(Clone, Debug, Deserialize, Eq, Error, PartialEq, Serialize)]
#[error("managed UI publication rejected ({code:?}): {message}")]
pub struct UiProtocolError {
    pub code: UiDiagnosticCode,
    pub message: String,
}

impl UiProtocolError {
    fn new(code: UiDiagnosticCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Complete accepted state for one actor generation and pane.
#[derive(Clone, Debug)]
pub struct AcceptedUiDocument {
    document: UiDocument,
    actor_generation: u64,
    interactions: BTreeMap<UiNodeId, u64>,
}

impl AcceptedUiDocument {
    pub fn accept_snapshot(
        document: UiDocument,
        actor_generation: u64,
        limits: UiLimits,
    ) -> Result<Self, UiProtocolError> {
        validate_document(&document, limits)?;
        if actor_generation == 0 {
            return Err(UiProtocolError::new(
                UiDiagnosticCode::InvalidRevision,
                "actor generation must be non-zero",
            ));
        }
        Ok(Self {
            document,
            actor_generation,
            interactions: BTreeMap::new(),
        })
    }

    #[must_use]
    pub const fn document(&self) -> &UiDocument {
        &self.document
    }

    #[must_use]
    pub const fn actor_generation(&self) -> u64 {
        self.actor_generation
    }

    /// Applies a patch to a private candidate and commits only after complete validation.
    pub fn apply_patch(&mut self, patch: UiPatch, limits: UiLimits) -> Result<(), UiProtocolError> {
        validate_patch_header(&self.document, &patch, limits)?;
        let mut candidate = self.document.clone();
        for operation in patch.operations {
            apply_operation(&mut candidate.root, operation)?;
        }
        candidate.document_revision = patch.document_revision;
        validate_document(&candidate, limits)?;
        self.document = candidate;
        let ids = collect_ids(&self.document.root);
        self.interactions.retain(|node, _| ids.contains(node));
        Ok(())
    }

    /// Creates a generation- and revision-bound action and advances its interaction revision.
    pub fn action(
        &mut self,
        node: UiNodeId,
        value: UiActionValue,
    ) -> Result<UiAction, UiProtocolError> {
        if find_node(&self.document.root, node).is_none() {
            return Err(UiProtocolError::new(
                UiDiagnosticCode::UnknownNode,
                "action node is absent",
            ));
        }
        let revision = self.interactions.entry(node).or_default();
        *revision = revision.checked_add(1).ok_or_else(|| {
            UiProtocolError::new(
                UiDiagnosticCode::InvalidRevision,
                "interaction revision overflow",
            )
        })?;
        Ok(UiAction {
            pane: self.document.pane,
            actor_generation: self.actor_generation,
            document_revision: self.document.document_revision,
            node,
            interaction_revision: *revision,
            value,
        })
    }
}

fn validate_patch_header(
    document: &UiDocument,
    patch: &UiPatch,
    limits: UiLimits,
) -> Result<(), UiProtocolError> {
    if patch.protocol_version != MANAGED_UI_PROTOCOL_VERSION {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::UnsupportedVersion,
            "unsupported patch version",
        ));
    }
    if patch.pane != document.pane {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::StalePublication,
            "patch pane does not match accepted document",
        ));
    }
    if patch.base_revision != document.document_revision {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::InvalidRevision,
            "patch base is not the accepted revision",
        ));
    }
    if patch.document_revision <= patch.base_revision {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::InvalidRevision,
            "patch revision is not the next accepted state",
        ));
    }
    if patch.operations.len() > limits.max_patch_operations as usize {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::LimitExceeded,
            "patch operation limit exceeded",
        ));
    }
    Ok(())
}

fn validate_document(document: &UiDocument, limits: UiLimits) -> Result<(), UiProtocolError> {
    if document.protocol_version != MANAGED_UI_PROTOCOL_VERSION {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::UnsupportedVersion,
            "unsupported document version",
        ));
    }
    if document.pane == 0 || document.document_revision == 0 {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::InvalidRevision,
            "pane and document revision must be non-zero",
        ));
    }
    let mut seen = BTreeSet::new();
    let mut nodes = 0_u32;
    let mut text_bytes = 0_usize;
    validate_node(
        &document.root,
        1,
        limits,
        &mut seen,
        &mut nodes,
        &mut text_bytes,
    )?;
    validate_accessibility_relationships(&document.root, &seen)
}

fn validate_node(
    node: &UiNode,
    depth: u16,
    limits: UiLimits,
    seen: &mut BTreeSet<UiNodeId>,
    nodes: &mut u32,
    text_bytes: &mut usize,
) -> Result<(), UiProtocolError> {
    if node.id == 0 || !seen.insert(node.id) {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::DuplicateNode,
            "node identities must be unique and non-zero",
        ));
    }
    *nodes = nodes.saturating_add(1);
    *text_bytes = text_bytes.saturating_add(node.kind.text_bytes());
    if *nodes > limits.max_nodes
        || depth > limits.max_depth
        || node.children.len() > limits.max_children_per_node as usize
        || *text_bytes > limits.max_text_bytes as usize
    {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::LimitExceeded,
            "document bounds exceeded",
        ));
    }
    validate_style(&node.style)?;
    if matches!(node.kind, UiNodeKind::Unsupported) && !node.optional {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::UnsupportedVersion,
            "unknown required managed-UI node",
        ));
    }
    validate_kind(&node.kind, node.interaction_revision)?;
    validate_accessibility(&node.accessibility, text_bytes, limits)?;
    for child in &node.children {
        validate_node(
            child,
            depth.saturating_add(1),
            limits,
            seen,
            nodes,
            text_bytes,
        )?;
    }
    Ok(())
}

fn validate_kind(kind: &UiNodeKind, interaction_revision: u64) -> Result<(), UiProtocolError> {
    match kind {
        UiNodeKind::Grid { columns: 0 } => {
            return Err(UiProtocolError::new(
                UiDiagnosticCode::InvalidValue,
                "grid must have at least one column",
            ));
        }
        UiNodeKind::VirtualList {
            estimated_item_extent,
            ..
        } if !estimated_item_extent.is_finite() || *estimated_item_extent <= 0.0 => {
            return Err(UiProtocolError::new(
                UiDiagnosticCode::InvalidValue,
                "virtual-list extent must be finite and positive",
            ));
        }
        UiNodeKind::Progress { value: Some(value) }
            if !value.is_finite() || !(0.0..=1.0).contains(value) =>
        {
            return Err(UiProtocolError::new(
                UiDiagnosticCode::InvalidValue,
                "progress value must be between zero and one",
            ));
        }
        _ => {}
    }
    if !kind_is_hybrid(kind) && interaction_revision != 0 {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::InvalidRevision,
            "only hybrid values may acknowledge an interaction revision",
        ));
    }
    Ok(())
}

const fn kind_is_hybrid(kind: &UiNodeKind) -> bool {
    matches!(
        kind,
        UiNodeKind::Toggle {
            state_mode: UiStateMode::Hybrid,
            ..
        } | UiNodeKind::RadioGroup {
            state_mode: UiStateMode::Hybrid,
            ..
        } | UiNodeKind::TextField {
            state_mode: UiStateMode::Hybrid,
            ..
        } | UiNodeKind::MultilineEditor {
            state_mode: UiStateMode::Hybrid,
            ..
        } | UiNodeKind::SearchField {
            state_mode: UiStateMode::Hybrid,
            ..
        }
    )
}

fn validate_accessibility(
    accessibility: &UiAccessibility,
    text_bytes: &mut usize,
    limits: UiLimits,
) -> Result<(), UiProtocolError> {
    *text_bytes = text_bytes.saturating_add(
        [
            accessibility.label.as_ref(),
            accessibility.description.as_ref(),
            accessibility.value.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(String::len)
        .sum::<usize>(),
    );
    *text_bytes = text_bytes.saturating_add(
        accessibility
            .commands
            .iter()
            .map(String::len)
            .sum::<usize>(),
    );
    if accessibility
        .labelled_by
        .len()
        .saturating_add(accessibility.described_by.len())
        > limits.max_children_per_node as usize
        || accessibility.commands.len() > limits.max_children_per_node as usize
    {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::LimitExceeded,
            "accessibility relationship bound exceeded",
        ));
    }
    if *text_bytes > limits.max_text_bytes as usize {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::LimitExceeded,
            "document text bound exceeded",
        ));
    }
    Ok(())
}

fn validate_accessibility_relationships(
    node: &UiNode,
    ids: &BTreeSet<UiNodeId>,
) -> Result<(), UiProtocolError> {
    if node
        .accessibility
        .labelled_by
        .iter()
        .chain(&node.accessibility.described_by)
        .any(|id| !ids.contains(id))
    {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::UnknownNode,
            "accessibility relationship references an absent node",
        ));
    }
    for child in &node.children {
        validate_accessibility_relationships(child, ids)?;
    }
    Ok(())
}

fn validate_style(style: &UiStyle) -> Result<(), UiProtocolError> {
    let finite_nonnegative = |value: f32| value.is_finite() && value >= 0.0;
    let optional_valid = |value: Option<f32>| value.is_none_or(finite_nonnegative);
    if !optional_valid(style.width)
        || !optional_valid(style.height)
        || !optional_valid(style.min_width)
        || !optional_valid(style.min_height)
        || !optional_valid(style.max_width)
        || !optional_valid(style.max_height)
        || !finite_nonnegative(style.grow)
        || !finite_nonnegative(style.shrink)
        || !finite_nonnegative(style.gap)
        || !finite_nonnegative(style.radius)
        || !finite_nonnegative(style.padding.top)
        || !finite_nonnegative(style.padding.right)
        || !finite_nonnegative(style.padding.bottom)
        || !finite_nonnegative(style.padding.left)
    {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::InvalidValue,
            "style dimensions must be finite and non-negative",
        ));
    }
    Ok(())
}

fn collect_ids(root: &UiNode) -> BTreeSet<UiNodeId> {
    let mut ids = BTreeSet::new();
    collect_ids_into(root, &mut ids);
    ids
}

fn collect_ids_into(node: &UiNode, ids: &mut BTreeSet<UiNodeId>) {
    ids.insert(node.id);
    for child in &node.children {
        collect_ids_into(child, ids);
    }
}

fn find_node(node: &UiNode, id: UiNodeId) -> Option<&UiNode> {
    if node.id == id {
        return Some(node);
    }
    node.children.iter().find_map(|child| find_node(child, id))
}

fn find_node_mut(node: &mut UiNode, id: UiNodeId) -> Option<&mut UiNode> {
    if node.id == id {
        return Some(node);
    }
    node.children
        .iter_mut()
        .find_map(|child| find_node_mut(child, id))
}

fn remove_node(node: &mut UiNode, id: UiNodeId) -> Option<UiNode> {
    if let Some(position) = node.children.iter().position(|child| child.id == id) {
        return Some(node.children.remove(position));
    }
    node.children
        .iter_mut()
        .find_map(|child| remove_node(child, id))
}

fn replace_node(node: &mut UiNode, id: UiNodeId, replacement: UiNode) -> bool {
    if let Some(position) = node.children.iter().position(|child| child.id == id)
        && let Some(child) = node.children.get_mut(position)
    {
        *child = replacement;
        return true;
    }
    node.children
        .iter_mut()
        .any(|child| replace_node(child, id, replacement.clone()))
}

fn insert_child(
    root: &mut UiNode,
    parent: UiNodeId,
    index: u32,
    child: UiNode,
) -> Result<(), UiProtocolError> {
    let parent = find_node_mut(root, parent).ok_or_else(|| {
        UiProtocolError::new(UiDiagnosticCode::UnknownNode, "parent node is absent")
    })?;
    let index = usize::try_from(index).map_err(|_error| {
        UiProtocolError::new(
            UiDiagnosticCode::InvalidTree,
            "child index cannot be represented",
        )
    })?;
    if index > parent.children.len() {
        return Err(UiProtocolError::new(
            UiDiagnosticCode::InvalidTree,
            "child index is outside the parent",
        ));
    }
    parent.children.insert(index, child);
    Ok(())
}

fn apply_operation(root: &mut UiNode, operation: UiOperation) -> Result<(), UiProtocolError> {
    match operation {
        UiOperation::ReplaceNode { node, replacement } => {
            if root.id == node {
                *root = replacement;
                Ok(())
            } else if replace_node(root, node, replacement) {
                Ok(())
            } else {
                Err(UiProtocolError::new(
                    UiDiagnosticCode::UnknownNode,
                    "replacement node is absent",
                ))
            }
        }
        UiOperation::InsertChild {
            parent,
            index,
            child,
        } => insert_child(root, parent, index, child),
        UiOperation::RemoveNode { node } => {
            if root.id == node {
                return Err(UiProtocolError::new(
                    UiDiagnosticCode::InvalidTree,
                    "root cannot be removed",
                ));
            }
            remove_node(root, node).map_or_else(
                || {
                    Err(UiProtocolError::new(
                        UiDiagnosticCode::UnknownNode,
                        "removed node is absent",
                    ))
                },
                |_| Ok(()),
            )
        }
        UiOperation::MoveChild {
            node,
            parent,
            index,
        } => {
            if node == root.id {
                return Err(UiProtocolError::new(
                    UiDiagnosticCode::InvalidTree,
                    "root cannot be moved",
                ));
            }
            let moved = remove_node(root, node).ok_or_else(|| {
                UiProtocolError::new(UiDiagnosticCode::UnknownNode, "moved node is absent")
            })?;
            insert_child(root, parent, index, moved)
        }
        UiOperation::SetKind { node, kind } => {
            find_node_mut(root, node)
                .ok_or_else(|| {
                    UiProtocolError::new(UiDiagnosticCode::UnknownNode, "updated node is absent")
                })?
                .kind = kind;
            Ok(())
        }
        UiOperation::SetStyle { node, style } => {
            find_node_mut(root, node)
                .ok_or_else(|| {
                    UiProtocolError::new(UiDiagnosticCode::UnknownNode, "updated node is absent")
                })?
                .style = style;
            Ok(())
        }
        UiOperation::SetAccessibility {
            node,
            accessibility,
        } => {
            find_node_mut(root, node)
                .ok_or_else(|| {
                    UiProtocolError::new(UiDiagnosticCode::UnknownNode, "updated node is absent")
                })?
                .accessibility = accessibility;
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "test fixtures fail immediately when their setup is invalid"
)]
mod tests {
    use super::*;

    fn node(id: u64, children: Vec<UiNode>) -> UiNode {
        UiNode {
            id,
            kind: UiNodeKind::Column,
            style: UiStyle::default(),
            accessibility: UiAccessibility::default(),
            optional: false,
            interaction_revision: 0,
            children,
        }
    }

    fn document() -> UiDocument {
        UiDocument {
            protocol_version: MANAGED_UI_PROTOCOL_VERSION,
            pane: 7,
            document_revision: 1,
            root: node(1, vec![node(2, Vec::new())]),
        }
    }

    #[test]
    fn patch_is_atomic_when_a_later_operation_fails() {
        let mut accepted = AcceptedUiDocument::accept_snapshot(document(), 9, UiLimits::default())
            .expect("fixture document must be valid");
        let patch = UiPatch {
            protocol_version: MANAGED_UI_PROTOCOL_VERSION,
            pane: 7,
            base_revision: 1,
            document_revision: 2,
            operations: vec![
                UiOperation::SetKind {
                    node: 2,
                    kind: UiNodeKind::Text {
                        text: "changed".into(),
                        role: UiTextRole::Body,
                    },
                },
                UiOperation::RemoveNode { node: 99 },
            ],
        };
        assert_eq!(
            accepted
                .apply_patch(patch, UiLimits::default())
                .map_err(|error| error.code),
            Err(UiDiagnosticCode::UnknownNode)
        );
        assert_eq!(accepted.document().document_revision, 1);
        assert!(matches!(
            find_node(&accepted.document().root, 2).map(|node| &node.kind),
            Some(UiNodeKind::Column)
        ));
    }

    #[test]
    fn rejects_duplicate_identity_and_stale_patch() {
        let mut duplicate = document();
        duplicate.root.children.push(node(1, Vec::new()));
        assert!(matches!(
            AcceptedUiDocument::accept_snapshot(duplicate, 9, UiLimits::default()),
            Err(UiProtocolError {
                code: UiDiagnosticCode::DuplicateNode,
                ..
            })
        ));

        let mut accepted = AcceptedUiDocument::accept_snapshot(document(), 9, UiLimits::default())
            .expect("fixture document must be valid");
        let stale = UiPatch {
            protocol_version: MANAGED_UI_PROTOCOL_VERSION,
            pane: 7,
            base_revision: 0,
            document_revision: 2,
            operations: Vec::new(),
        };
        assert_eq!(
            accepted
                .apply_patch(stale, UiLimits::default())
                .map_err(|error| error.code),
            Err(UiDiagnosticCode::InvalidRevision)
        );
    }

    #[test]
    fn actions_are_bound_to_document_and_interaction_revisions() {
        let mut accepted = AcceptedUiDocument::accept_snapshot(document(), 9, UiLimits::default())
            .expect("fixture document must be valid");
        let first = accepted
            .action(2, UiActionValue::Activate)
            .expect("known node action");
        let second = accepted
            .action(2, UiActionValue::Activate)
            .expect("known node action");
        assert_eq!(first.document_revision, 1);
        assert_eq!(first.interaction_revision, 1);
        assert_eq!(second.interaction_revision, 2);
        assert_eq!(
            accepted
                .action(88, UiActionValue::Activate)
                .map_err(|error| error.code),
            Err(UiDiagnosticCode::UnknownNode)
        );
    }

    #[test]
    fn unknown_nodes_are_omitted_only_when_declared_optional() {
        let mut value = serde_json::to_value(document()).expect("serialize fixture");
        let root = value
            .get_mut("root")
            .and_then(serde_json::Value::as_object_mut)
            .expect("root");
        root.insert(
            "kind".into(),
            serde_json::json!({ "kind": "future-decoration", "future-value": 7 }),
        );
        root.insert("optional".into(), serde_json::Value::Bool(true));
        let optional: UiDocument =
            serde_json::from_value(value.clone()).expect("decode optional future node");
        assert!(matches!(optional.root.kind, UiNodeKind::Unsupported));
        assert!(AcceptedUiDocument::accept_snapshot(optional, 9, UiLimits::default()).is_ok());

        value
            .get_mut("root")
            .and_then(serde_json::Value::as_object_mut)
            .expect("root")
            .insert("optional".into(), serde_json::Value::Bool(false));
        let required: UiDocument =
            serde_json::from_value(value).expect("decode required future node");
        assert!(matches!(
            AcceptedUiDocument::accept_snapshot(required, 9, UiLimits::default()),
            Err(UiProtocolError {
                code: UiDiagnosticCode::UnsupportedVersion,
                ..
            })
        ));
    }
}
