//! GPUI host adapter for Brood's stable managed-UI protocol.
//!
//! This is the only workspace crate allowed to translate protocol nodes into
//! GPUI elements. Protocol, actor, transport, and session crates remain free
//! of GPUI types.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::collapsible_if,
    clippy::default_trait_access,
    clippy::float_cmp,
    clippy::let_underscore_must_use,
    clippy::needless_ifs,
    clippy::needless_pass_by_ref_mut,
    clippy::semicolon_if_nothing_returned,
    clippy::suboptimal_flops,
    clippy::too_many_arguments,
    clippy::type_complexity,
    reason = "the adapter crosses validated wire numerics and GPUI's f32/pixel callback API"
)]

#[path = "bin/brood-client-gpui.rs"]
pub mod client;

#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::Range,
    sync::{Arc, Mutex},
};

use brood_protocol::{
    InputEvent as BroodInputEvent,
    managed_ui::{
        AcceptedUiDocument, UiAction, UiActionValue, UiAlignment, UiColorToken, UiDocument,
        UiLimits, UiNode, UiNodeKind, UiPaneId, UiPatch, UiProtocolError, UiPublication,
    },
};
use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, GlobalElementId, InspectorElementId,
    InteractiveElement as _, IntoElement, KeyDownEvent, KeyUpEvent, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection, ParentElement as _, Pixels,
    Point, Render, ScrollWheelEvent, SharedString, StatefulInteractiveElement as _, Style,
    Styled as _, UTF16Selection, WeakEntity, Window, div, img, prelude::FluentBuilder as _, px,
    relative, rgb,
};
use thiserror::Error;

#[cfg(target_os = "linux")]
struct AccessibilityActivation {
    tree: Arc<Mutex<accesskit::TreeUpdate>>,
}

#[cfg(target_os = "linux")]
impl accesskit::ActivationHandler for AccessibilityActivation {
    fn request_initial_tree(&mut self) -> Option<accesskit::TreeUpdate> {
        Some(
            self.tree
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
    }
}

#[cfg(target_os = "linux")]
struct AccessibilityActions(std::sync::mpsc::SyncSender<accesskit::ActionRequest>);

#[cfg(target_os = "linux")]
impl accesskit::ActionHandler for AccessibilityActions {
    fn do_action(&mut self, request: accesskit::ActionRequest) {
        if self.0.try_send(request).is_err() {}
    }
}

#[cfg(target_os = "linux")]
struct AccessibilityDeactivation;

#[cfg(target_os = "linux")]
impl accesskit::DeactivationHandler for AccessibilityDeactivation {
    fn deactivate_accessibility(&mut self) {}
}

/// Bounded diagnostics retained for operator inspection.
const MAX_DIAGNOSTICS: usize = 256;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AdapterError {
    #[error(transparent)]
    Protocol(#[from] UiProtocolError),
    #[error("managed UI pane belongs to a different actor generation")]
    StaleActorGeneration,
    #[error("managed UI patch has no accepted base document")]
    MissingDocument,
    #[error("managed UI action queue is full")]
    ActionBacklogFull,
    #[error("unsupported external-surface DRM format {0:#x}")]
    UnsupportedDrmFormat(u32),
    #[error("invalid external-surface descriptor: {0}")]
    InvalidSurfaceDescriptor(&'static str),
}

/// Import metadata carried by Brood's authenticated local transport.
#[cfg(target_os = "linux")]
pub struct LinuxSurfaceDescriptor {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub stride: u32,
    pub offset: u32,
    pub y_inverted: bool,
    /// Source rectangle in buffer pixels.
    pub source: [i32; 4],
}

#[cfg(target_os = "linux")]
impl LinuxSurfaceDescriptor {
    pub fn import(self) -> Result<Arc<gpui::ExternalSurface>, AdapterError> {
        const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;
        const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
        const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241;
        const DRM_FORMAT_XBGR8888: u32 = 0x3432_4258;
        let format = match self.fourcc {
            DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 => gpui::ExternalSurfaceFormat::Bgra8Srgb,
            DRM_FORMAT_ABGR8888 | DRM_FORMAT_XBGR8888 => gpui::ExternalSurfaceFormat::Rgba8Srgb,
            other => return Err(AdapterError::UnsupportedDrmFormat(other)),
        };
        if self.width == 0 || self.height == 0 {
            return Err(AdapterError::InvalidSurfaceDescriptor(
                "buffer extent must be non-zero",
            ));
        }
        let [x, y, width, height] = self.source;
        if x < 0
            || y < 0
            || width <= 0
            || height <= 0
            || i64::from(x) + i64::from(width) > i64::from(self.width)
            || i64::from(y) + i64::from(height) > i64::from(self.height)
        {
            return Err(AdapterError::InvalidSurfaceDescriptor(
                "source rectangle is outside the buffer",
            ));
        }
        let source_uv = [
            x as f32 / self.width as f32,
            y as f32 / self.height as f32,
            width as f32 / self.width as f32,
            height as f32 / self.height as f32,
        ];
        Ok(Arc::new(gpui::ExternalSurface::new(
            self.fd,
            self.width,
            self.height,
            format,
            self.modifier,
            u64::from(self.stride),
            u64::from(self.offset),
            self.y_inverted,
            source_uv,
        )))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterDiagnostic {
    pub pane: UiPaneId,
    pub actor_generation: u64,
    pub message: String,
}

/// Accepted CPU-side semantic state. GPU/device recovery rebuilds from here.
#[derive(Clone)]
pub struct DocumentStore {
    limits: UiLimits,
    documents: BTreeMap<UiPaneId, AcceptedUiDocument>,
    diagnostics: VecDeque<AdapterDiagnostic>,
    interaction_floors: Option<Arc<Mutex<BTreeMap<(UiPaneId, u64, u64), u64>>>>,
}

impl DocumentStore {
    #[must_use]
    pub const fn new(limits: UiLimits) -> Self {
        Self {
            limits,
            documents: BTreeMap::new(),
            diagnostics: VecDeque::new(),
            interaction_floors: None,
        }
    }

    #[must_use]
    pub fn with_actions(mut self, actions: &ActionSink) -> Self {
        self.interaction_floors = Some(Arc::clone(&actions.interactions));
        self
    }

    pub fn publish(
        &mut self,
        actor_generation: u64,
        publication: UiPublication,
    ) -> Result<(), AdapterError> {
        let pane = match &publication {
            UiPublication::Snapshot(document) => document.pane,
            UiPublication::Patch(patch) => patch.pane,
            UiPublication::RemovePane { pane } => *pane,
        };
        let result = match publication {
            UiPublication::Snapshot(document) => self.snapshot(actor_generation, *document),
            UiPublication::Patch(patch) => self.patch(actor_generation, patch),
            UiPublication::RemovePane { pane } => self.remove(pane, actor_generation),
        };
        if let Err(error) = &result {
            self.record_diagnostic(pane, actor_generation, error.to_string());
        }
        result
    }

    fn snapshot(
        &mut self,
        actor_generation: u64,
        document: UiDocument,
    ) -> Result<(), AdapterError> {
        if self.documents.get(&document.pane).is_some_and(|accepted| {
            accepted.actor_generation() == actor_generation
                && accepted.document().document_revision >= document.document_revision
        }) {
            return Err(AdapterError::Protocol(UiProtocolError {
                code: brood_protocol::managed_ui::UiDiagnosticCode::StalePublication,
                message: "snapshot does not advance the accepted revision".into(),
            }));
        }
        let pane = document.pane;
        let accepted =
            AcceptedUiDocument::accept_snapshot(document, actor_generation, self.limits)?;
        self.validate_interaction_floors(&accepted)?;
        self.documents.insert(pane, accepted);
        Ok(())
    }

    fn patch(&mut self, actor_generation: u64, patch: UiPatch) -> Result<(), AdapterError> {
        let pane = patch.pane;
        let accepted = self
            .documents
            .get(&pane)
            .ok_or(AdapterError::MissingDocument)?;
        if accepted.actor_generation() != actor_generation {
            return Err(AdapterError::StaleActorGeneration);
        }
        let mut candidate = accepted.clone();
        candidate.apply_patch(patch, self.limits)?;
        self.validate_interaction_floors(&candidate)?;
        self.documents.insert(pane, candidate);
        Ok(())
    }

    fn remove(&mut self, pane: UiPaneId, actor_generation: u64) -> Result<(), AdapterError> {
        let Some(accepted) = self.documents.get(&pane) else {
            return Ok(());
        };
        if accepted.actor_generation() != actor_generation {
            return Err(AdapterError::StaleActorGeneration);
        }
        self.documents.remove(&pane);
        Ok(())
    }

    fn record_diagnostic(&mut self, pane: UiPaneId, actor_generation: u64, message: String) {
        if self.diagnostics.len() == MAX_DIAGNOSTICS {
            self.diagnostics.pop_front();
        }
        self.diagnostics.push_back(AdapterDiagnostic {
            pane,
            actor_generation,
            message,
        });
    }

    fn validate_interaction_floors(
        &self,
        accepted: &AcceptedUiDocument,
    ) -> Result<(), AdapterError> {
        let Some(floors) = self.interaction_floors.as_ref() else {
            return Ok(());
        };
        let floors = floors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        validate_node_interaction_floors(
            &accepted.document().root,
            accepted.document().pane,
            accepted.actor_generation(),
            &floors,
        )
    }

    #[must_use]
    pub fn document(&self, pane: UiPaneId) -> Option<&UiDocument> {
        self.documents.get(&pane).map(AcceptedUiDocument::document)
    }

    pub fn documents(&self) -> impl Iterator<Item = &UiDocument> {
        self.documents.values().map(AcceptedUiDocument::document)
    }

    #[must_use]
    pub fn managed_documents(&self) -> Vec<ManagedDocument> {
        self.documents
            .values()
            .map(|accepted| ManagedDocument {
                actor_generation: accepted.actor_generation(),
                document: accepted.document().clone(),
            })
            .collect()
    }

    pub fn diagnostics(&self) -> impl Iterator<Item = &AdapterDiagnostic> {
        self.diagnostics.iter()
    }
}

/// Bounded messages crossing from Brood services into GPUI's foreground.
pub enum PresentationCommand {
    Publish {
        actor_generation: u64,
        publication: UiPublication,
    },
    CommitGeometry {
        revision: u64,
        panes: BTreeMap<UiPaneId, PaneGeometry>,
    },
    /// Commit actor-authored normalized pane geometry. The GPUI foreground
    /// projects it against the current native window on every frame.
    CommitNormalizedGeometry {
        revision: u64,
        panes: BTreeMap<UiPaneId, brood_wasm::LocalTransform>,
    },
    ReplaceCanvas {
        actor_generation: u64,
        document: Option<Box<UiDocument>>,
        target: u64,
        packet: Arc<brood_graphics::Packet>,
    },
    /// Prepare resize-sensitive canvas output without changing the visible tree.
    StageCanvas {
        actor_generation: u64,
        document: Option<Box<UiDocument>>,
        epoch: u64,
        target: u64,
        packet: Arc<brood_graphics::Packet>,
    },
    /// Promote geometry and all canvas output prepared for this resize epoch in
    /// one foreground mutation. Missing canvas entries deliberately reuse the
    /// previously accepted CPU-side packet.
    CommitResizeCandidate {
        epoch: u64,
        revision: u64,
        panes: BTreeMap<UiPaneId, brood_wasm::LocalTransform>,
    },
    ReplaceCanvasText {
        resource: u64,
        layout: CanvasTextLayout,
    },
    MeasureCanvasText {
        resource: u64,
        layout: CanvasTextLayout,
        response: std::sync::mpsc::SyncSender<
            Result<brood_wasm::TextLayoutResult, brood_wasm::PresentationError>,
        >,
    },
    ReplaceCanvasImage {
        resource: u64,
        image: Arc<gpui::RenderImage>,
    },
    RemoveCanvasResource {
        resource: u64,
        response: std::sync::mpsc::SyncSender<bool>,
    },
    RemoveCanvas {
        target: u64,
    },
    SetClipboard {
        text: String,
    },
    RequestClipboard {
        actor_generation: u64,
        pane: u64,
        response: std::sync::mpsc::SyncSender<(u64, u64, String)>,
    },
    #[cfg(target_os = "linux")]
    ReplaceExternalSurface {
        resource: u64,
        surface: Arc<gpui::ExternalSurface>,
    },
    #[cfg(target_os = "linux")]
    CommitExternalScene {
        revision: u64,
        sequence: u64,
        panes: Vec<ExternalPane>,
    },
    #[cfg(target_os = "linux")]
    StageExternalScene {
        epoch: u64,
        sequence: u64,
        panes: Vec<ExternalPane>,
    },
    CommitCopiedScene {
        revision: u64,
        sequence: u64,
        panes: Vec<CopiedPane>,
    },
    StageCopiedScene {
        epoch: u64,
        sequence: u64,
        panes: Vec<CopiedPane>,
    },
    RemoveSceneDomain {
        domain: u64,
    },
    RemoveExternalSurface {
        resource: u64,
    },
    SetCursor(brood_wasm::CursorRole),
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

const fn pane_scene_domain(pane: UiPaneId) -> u64 {
    // Remote panes reserve bit 62 and carry their backend in bits 48..62.
    // Local session panes and plugin-owned panes occupy distinct namespaces.
    if pane & (3_u64 << 62) == 1_u64 << 62 {
        (pane >> 48) & 0x3fff
    } else {
        0
    }
}

#[derive(Clone, Debug)]
pub struct CanvasTextLayout {
    pub text: String,
    pub options: brood_wasm::TextLayoutOptions,
}

/// Reject canvas features that GPUI's public scene API cannot lower exactly.
///
/// Keeping the prior accepted packet visible is preferable to silently
/// replacing path clips, isolation groups, or blend equations with a visually
/// different operation.
#[must_use]
pub fn canvas_packet_is_exact(packet: &brood_graphics::Packet) -> bool {
    use brood_graphics::{BlendMode, Command, Transform};
    let mut transform = Transform::IDENTITY;
    let mut stack = Vec::new();
    for command in &packet.commands {
        match command {
            Command::Save => stack.push(transform),
            Command::Restore => {
                let Some(saved) = stack.pop() else {
                    return false;
                };
                transform = saved;
            }
            Command::SetTransform(next) => transform = *next,
            Command::SetBlendMode(mode) if *mode != BlendMode::SourceOver => return false,
            Command::ClipPath(_) | Command::BeginLayer | Command::EndLayer => return false,
            Command::ClipRect(_) | Command::FillRect { .. }
                if !axis_aligned_transform(transform) =>
            {
                return false;
            }
            Command::StrokeRect { .. }
            | Command::FillRoundedRect { .. }
            | Command::StrokeRoundedRect { .. }
                if !axis_aligned_transform(transform)
                    || transform_uniform_scale(transform).is_none() =>
            {
                return false;
            }
            Command::DrawImage { .. }
                if !axis_aligned_transform(transform)
                    || transform.sx < 0.0
                    || transform.sy < 0.0 =>
            {
                return false;
            }
            Command::StrokePath { .. } if transform_uniform_scale(transform).is_none() => {
                return false;
            }
            Command::DrawTextLayout { .. }
                if transform != Transform::IDENTITY
                    && !(transform.sx == 1.0
                        && transform.sy == 1.0
                        && transform.kx == 0.0
                        && transform.ky == 0.0) =>
            {
                return false;
            }
            _ => {}
        }
    }
    stack.is_empty()
}

const fn axis_aligned_transform(transform: brood_graphics::Transform) -> bool {
    transform.kx == 0.0 && transform.ky == 0.0
}

fn transform_uniform_scale(transform: brood_graphics::Transform) -> Option<f32> {
    let first = transform
        .sx
        .mul_add(transform.sx, transform.ky * transform.ky);
    let second = transform
        .kx
        .mul_add(transform.kx, transform.sy * transform.sy);
    let dot = transform
        .sx
        .mul_add(transform.kx, transform.ky * transform.sy);
    let tolerance = 1.0e-5 * first.max(second).max(1.0);
    ((first - second).abs() <= tolerance && dot.abs() <= tolerance).then(|| first.sqrt())
}

pub struct CopiedPane {
    pub pane: UiPaneId,
    pub resource: u64,
    pub geometry: PaneGeometry,
    pub image: Arc<gpui::RenderImage>,
}

/// Native input details used by plugin canvas/terminal bridges.
///
/// These stay inside the presentation process; the session compositor
/// continues to receive the stable transport-level [`BroodInputEvent`] stream.
#[derive(Clone, Debug)]
pub enum ActorInputEvent {
    Key {
        keycode: u32,
        logical_key: String,
        pressed: bool,
        modifiers: u32,
    },
    Text(String),
    Paste(String),
    PointerMotion {
        x: f32,
        y: f32,
        modifiers: u32,
    },
    PointerButton {
        x: f32,
        y: f32,
        button: u32,
        pressed: bool,
        modifiers: u32,
    },
    PointerScroll {
        x: f32,
        y: f32,
        horizontal: f32,
        vertical: f32,
        modifiers: u32,
    },
}

#[cfg(target_os = "linux")]
pub struct ExternalPane {
    pub pane: UiPaneId,
    pub resource: u64,
    pub geometry: PaneGeometry,
    pub surface: Arc<gpui::ExternalSurface>,
}

/// Cloneable, bounded ingress for the sole presentation lifecycle.
#[derive(Clone, Debug)]
pub struct PresentationHandle {
    sender: async_channel::Sender<PresentationCommand>,
}

impl PresentationHandle {
    pub fn try_send(&self, command: PresentationCommand) -> Result<(), PresentationCommand> {
        self.sender
            .try_send(command)
            .map_err(async_channel::TrySendError::into_inner)
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.sender.len()
    }
}

#[must_use]
pub fn presentation_channel(
    capacity: usize,
) -> (
    PresentationHandle,
    async_channel::Receiver<PresentationCommand>,
) {
    let (sender, receiver) = async_channel::bounded(capacity.max(1));
    (PresentationHandle { sender }, receiver)
}

/// Owns the GPUI foreground mutation loop. Background services only submit
/// bounded messages and never borrow a framework entity directly.
#[allow(
    clippy::future_not_send,
    reason = "GPUI foreground contexts are deliberately thread-affine"
)]
pub async fn drive_presentation(
    receiver: async_channel::Receiver<PresentationCommand>,
    root: gpui::Entity<ManagedUiRoot>,
    actions: ActionSink,
    mut cx: gpui::AsyncApp,
) {
    let mut store = DocumentStore::new(UiLimits::default()).with_actions(&actions);
    let mut raw_geometry_revision = 0_u64;
    let mut scene_revisions = BTreeMap::<u64, u64>::new();
    let mut normalized_geometry_revision = 0_u64;
    let mut staged_canvas = BTreeMap::<u64, BTreeMap<u64, Arc<brood_graphics::Packet>>>::new();
    let mut staged_canvas_documents = BTreeMap::<u64, BTreeMap<u64, (u64, Box<UiDocument>)>>::new();
    let mut retained_canvas = BTreeMap::<u64, Arc<brood_graphics::Packet>>::new();
    #[cfg(target_os = "linux")]
    let mut staged_external = BTreeMap::<u64, (u64, Vec<ExternalPane>)>::new();
    #[cfg(target_os = "linux")]
    let mut external_resources = BTreeMap::<u64, BTreeSet<u64>>::new();
    let mut staged_copied = BTreeMap::<u64, (u64, Vec<CopiedPane>)>::new();
    #[cfg(target_os = "linux")]
    let mut scene_panes = BTreeMap::<u64, BTreeSet<u64>>::new();
    let mut copied_resources = BTreeMap::<u64, BTreeSet<u64>>::new();
    let mut command_count = 0_u64;
    while let Ok(command) = receiver.recv().await {
        command_count = command_count.saturating_add(1);
        let keep_running = match command {
            PresentationCommand::Publish {
                actor_generation,
                publication,
            } => {
                if let Err(error) = store.publish(actor_generation, publication) {
                    eprintln!("managed UI publication rejected: {error}");
                } else if cx
                    .update_entity(&root, |view, cx| {
                        view.replace_documents(store.managed_documents(), cx);
                    })
                    .is_err()
                {
                    break;
                }
                true
            }
            PresentationCommand::CommitGeometry { revision, panes } => {
                if revision > raw_geometry_revision {
                    if cx
                        .update_entity(&root, |view, cx| view.replace_geometry(revision, panes, cx))
                        .is_err()
                    {
                        break;
                    }
                    raw_geometry_revision = revision;
                }
                true
            }
            PresentationCommand::CommitNormalizedGeometry { revision, panes } => {
                if revision > normalized_geometry_revision {
                    if cx
                        .update_entity(&root, |view, cx| {
                            view.replace_normalized_geometry(revision, panes, cx);
                        })
                        .is_err()
                    {
                        break;
                    }
                    normalized_geometry_revision = revision;
                }
                true
            }
            PresentationCommand::ReplaceCanvas {
                actor_generation,
                document,
                target,
                packet,
            } => {
                if let Some(document) = document
                    && let Err(error) =
                        store.publish(actor_generation, UiPublication::Snapshot(document))
                {
                    eprintln!("canvas document rejected: {error}");
                    continue;
                }
                retained_canvas.insert(target, Arc::clone(&packet));
                let documents = store.managed_documents();
                if cx
                    .update_entity(&root, |view, cx| {
                        view.canvas_packets.insert(target, packet);
                        view.documents = documents;
                        view.reconcile_focus_handles(cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                true
            }
            PresentationCommand::StageCanvas {
                actor_generation,
                document,
                epoch,
                target,
                packet,
            } => {
                // Keep staging bounded and latest-only per target. Epochs older
                // than the currently visible geometry can never be promoted.
                staged_canvas.retain(|candidate, _| *candidate >= normalized_geometry_revision);
                staged_canvas_documents
                    .retain(|candidate, _| *candidate >= normalized_geometry_revision);
                if staged_canvas.len() >= 8 && !staged_canvas.contains_key(&epoch) {
                    if let Some(oldest) = staged_canvas.keys().next().copied() {
                        staged_canvas.remove(&oldest);
                        staged_canvas_documents.remove(&oldest);
                    }
                }
                staged_canvas
                    .entry(epoch)
                    .or_default()
                    .insert(target, packet);
                if let Some(document) = document {
                    staged_canvas_documents
                        .entry(epoch)
                        .or_default()
                        .insert(target, (actor_generation, document));
                }
                true
            }
            PresentationCommand::CommitResizeCandidate {
                epoch,
                revision,
                panes,
            } => {
                if revision > normalized_geometry_revision {
                    let packets = staged_canvas.remove(&epoch).unwrap_or_default();
                    let staged_documents =
                        staged_canvas_documents.remove(&epoch).unwrap_or_default();
                    let mut candidate_store = store.clone();
                    let mut document_error = None;
                    for (_, (actor_generation, document)) in staged_documents {
                        let pane = document.pane;
                        if let Err(error) = candidate_store
                            .publish(actor_generation, UiPublication::Snapshot(document))
                        {
                            document_error = Some((pane, actor_generation, error));
                            break;
                        }
                    }
                    if let Some((pane, actor_generation, error)) = document_error {
                        store.record_diagnostic(pane, actor_generation, error.to_string());
                        eprintln!("staged canvas document rejected: {error}");
                        staged_canvas.retain(|candidate, _| *candidate > epoch);
                        staged_canvas_documents.retain(|candidate, _| *candidate > epoch);
                        #[cfg(target_os = "linux")]
                        staged_external.retain(|candidate, _| *candidate > epoch);
                        staged_copied.retain(|candidate, _| *candidate > epoch);
                        continue;
                    }
                    store = candidate_store;
                    retained_canvas.extend(
                        packets
                            .iter()
                            .map(|(target, packet)| (*target, Arc::clone(packet))),
                    );
                    #[cfg(target_os = "linux")]
                    let external = staged_external.remove(&epoch);
                    let copied = staged_copied.remove(&epoch);
                    #[cfg(target_os = "linux")]
                    if let Some((_, staged)) = external.as_ref() {
                        for pane in staged {
                            let document_revision = store
                                .document(pane.pane)
                                .map_or(1, |document| document.document_revision.saturating_add(1));
                            let _ = store.publish(
                                u64::MAX,
                                UiPublication::Snapshot(Box::new(external_surface_document(
                                    pane.pane,
                                    pane.resource,
                                    document_revision,
                                ))),
                            );
                        }
                    }
                    if let Some((_, staged)) = copied.as_ref() {
                        for pane in staged {
                            let document_revision = store
                                .document(pane.pane)
                                .map_or(1, |document| document.document_revision.saturating_add(1));
                            let _ = store.publish(
                                u64::MAX,
                                UiPublication::Snapshot(Box::new(copied_surface_document(
                                    pane.pane,
                                    pane.resource,
                                    document_revision,
                                ))),
                            );
                        }
                    }
                    let documents = store.managed_documents();
                    #[cfg(target_os = "linux")]
                    let retired_external = external
                        .as_ref()
                        .map(|(_, staged)| {
                            let domain = staged
                                .first()
                                .map_or(0, |pane| pane_scene_domain(pane.pane));
                            let next = staged
                                .iter()
                                .map(|pane| pane.resource)
                                .collect::<BTreeSet<_>>();
                            external_resources
                                .insert(domain, next.clone())
                                .unwrap_or_default()
                                .difference(&next)
                                .copied()
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    if cx
                        .update_entity(&root, |view, cx| {
                            view.normalized_geometry_revision = revision;
                            view.normalized_geometry = panes;
                            view.canvas_packets.extend(packets);
                            view.documents = documents;
                            #[cfg(target_os = "linux")]
                            if let Some((sequence, staged)) = external {
                                view.pending_frame_completion = Some(sequence);
                                for resource in retired_external {
                                    view.external_surfaces.remove(&resource);
                                }
                                for pane in staged {
                                    view.external_surfaces.insert(pane.resource, pane.surface);
                                }
                            }
                            if let Some((sequence, staged)) = copied {
                                view.pending_frame_completion = Some(sequence);
                                for pane in staged {
                                    view.canvas_images.insert(pane.resource, pane.image);
                                }
                            }
                            view.reconcile_focus_handles(cx);
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                    normalized_geometry_revision = revision;
                }
                staged_canvas.retain(|candidate, _| *candidate > epoch);
                staged_canvas_documents.retain(|candidate, _| *candidate > epoch);
                #[cfg(target_os = "linux")]
                staged_external.retain(|candidate, _| *candidate > epoch);
                staged_copied.retain(|candidate, _| *candidate > epoch);
                true
            }
            #[cfg(target_os = "linux")]
            PresentationCommand::StageExternalScene {
                epoch,
                sequence,
                panes,
            } => {
                if staged_external.len() >= 8
                    && !staged_external.contains_key(&epoch)
                    && let Some(oldest) = staged_external.keys().next().copied()
                {
                    staged_external.remove(&oldest);
                }
                staged_external.insert(epoch, (sequence, panes));
                true
            }
            PresentationCommand::StageCopiedScene {
                epoch,
                sequence,
                panes,
            } => {
                if staged_copied.len() >= 8
                    && !staged_copied.contains_key(&epoch)
                    && let Some(oldest) = staged_copied.keys().next().copied()
                {
                    staged_copied.remove(&oldest);
                }
                staged_copied.insert(epoch, (sequence, panes));
                true
            }
            PresentationCommand::RemoveSceneDomain { domain } => {
                let panes = scene_panes.remove(&domain).unwrap_or_default();
                for pane in &panes {
                    let _ = store.publish(u64::MAX, UiPublication::RemovePane { pane: *pane });
                }
                scene_revisions.remove(&domain);
                let resources = copied_resources.remove(&domain).unwrap_or_default();
                #[cfg(target_os = "linux")]
                let retired_external = external_resources.remove(&domain).unwrap_or_default();
                if cx
                    .update_entity(&root, |view, cx| {
                        view.documents = store.managed_documents();
                        for pane in panes {
                            view.pane_geometry.remove(&pane);
                        }
                        for resource in resources {
                            view.canvas_images.remove(&resource);
                        }
                        #[cfg(target_os = "linux")]
                        for resource in retired_external {
                            view.external_surfaces.remove(&resource);
                        }
                        view.reconcile_focus_handles(cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                true
            }
            PresentationCommand::ReplaceCanvasText { resource, layout } => {
                if cx
                    .update_entity(&root, |view, cx| {
                        view.canvas_text.insert(resource, layout);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                true
            }
            PresentationCommand::MeasureCanvasText {
                resource,
                layout,
                response,
            } => {
                if cx
                    .update_entity(&root, |view, cx| {
                        view.pending_text_measurements
                            .push((resource, layout, response));
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                true
            }
            PresentationCommand::ReplaceCanvasImage { resource, image } => {
                if cx
                    .update_entity(&root, |view, cx| {
                        view.canvas_images.insert(resource, image);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                true
            }
            PresentationCommand::RemoveCanvasResource { resource, response } => {
                let in_use = retained_canvas
                    .values()
                    .chain(staged_canvas.values().flat_map(BTreeMap::values))
                    .any(|packet| canvas_packet_uses_resource(packet, resource));
                if in_use {
                    let _ = response.try_send(false);
                    continue;
                }
                if cx
                    .update_entity(&root, |view, cx| {
                        view.canvas_text.remove(&resource);
                        view.canvas_images.remove(&resource);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                let _ = response.try_send(true);
                true
            }
            PresentationCommand::RemoveCanvas { target } => {
                retained_canvas.remove(&target);
                if cx
                    .update_entity(&root, |view, cx| {
                        view.canvas_packets.remove(&target);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                true
            }
            PresentationCommand::SetClipboard { text } => {
                if cx
                    .update(|cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)))
                    .is_err()
                {
                    break;
                }
                true
            }
            PresentationCommand::RequestClipboard {
                actor_generation,
                pane,
                response,
            } => {
                if cx
                    .update(|cx| {
                        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                            drop(response.try_send((actor_generation, pane, text)));
                        }
                    })
                    .is_err()
                {
                    break;
                }
                true
            }
            #[cfg(target_os = "linux")]
            PresentationCommand::ReplaceExternalSurface { resource, surface } => {
                if cx
                    .update_entity(&root, |view, cx| {
                        view.replace_external_surface(resource, surface, cx);
                    })
                    .is_err()
                {
                    break;
                }
                true
            }
            #[cfg(target_os = "linux")]
            PresentationCommand::CommitExternalScene {
                revision,
                sequence,
                panes,
            } => {
                let domain = panes.first().map_or(0, |pane| pane_scene_domain(pane.pane));
                if revision <= scene_revisions.get(&domain).copied().unwrap_or(0) {
                    continue;
                }
                let next_panes = panes.iter().map(|pane| pane.pane).collect::<BTreeSet<_>>();
                let previous_panes = scene_panes.get(&domain).cloned().unwrap_or_default();
                for pane in previous_panes.difference(&next_panes).copied() {
                    if let Err(error) = store.publish(u64::MAX, UiPublication::RemovePane { pane })
                    {
                        eprintln!("could not retire external pane {pane}: {error}");
                    }
                }
                let mut geometry = BTreeMap::new();
                let mut resources = Vec::new();
                for pane in panes {
                    let document = external_surface_document(pane.pane, pane.resource, revision);
                    if let Err(error) =
                        store.publish(u64::MAX, UiPublication::Snapshot(Box::new(document)))
                    {
                        eprintln!("could not accept external pane {}: {error}", pane.pane);
                    }
                    geometry.insert(pane.pane, pane.geometry);
                    resources.push((pane.resource, pane.surface));
                }
                scene_panes.insert(domain, next_panes);
                scene_revisions.insert(domain, revision);
                let next_resources = resources
                    .iter()
                    .map(|(resource, _)| *resource)
                    .collect::<BTreeSet<_>>();
                let previous_resources = external_resources
                    .insert(domain, next_resources.clone())
                    .unwrap_or_default();
                if cx
                    .update_entity(&root, |view, cx| {
                        view.documents = store.managed_documents();
                        view.raw_geometry_revision = revision;
                        view.pending_frame_completion = Some(sequence);
                        for pane in &previous_panes {
                            view.pane_geometry.remove(pane);
                        }
                        view.pane_geometry.extend(geometry);
                        for resource in previous_resources.difference(&next_resources) {
                            view.external_surfaces.remove(resource);
                        }
                        for (resource, surface) in resources {
                            view.external_surfaces.insert(resource, surface);
                        }
                        view.reconcile_focus_handles(cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                true
            }
            PresentationCommand::CommitCopiedScene {
                revision,
                sequence,
                panes,
            } => {
                let domain = panes.first().map_or(0, |pane| pane_scene_domain(pane.pane));
                if revision <= scene_revisions.get(&domain).copied().unwrap_or(0) {
                    continue;
                }
                let next_panes = panes.iter().map(|pane| pane.pane).collect::<BTreeSet<_>>();
                let previous_panes = scene_panes.get(&domain).cloned().unwrap_or_default();
                for pane in previous_panes.difference(&next_panes).copied() {
                    let _ = store.publish(u64::MAX, UiPublication::RemovePane { pane });
                }
                let mut geometry = BTreeMap::new();
                let mut resources = Vec::new();
                for pane in panes {
                    let document = copied_surface_document(pane.pane, pane.resource, revision);
                    if let Err(error) =
                        store.publish(u64::MAX, UiPublication::Snapshot(Box::new(document)))
                    {
                        eprintln!("could not accept copied pane {}: {error}", pane.pane);
                    }
                    geometry.insert(pane.pane, pane.geometry);
                    resources.push((pane.resource, pane.image));
                }
                let next_resources = resources
                    .iter()
                    .map(|(resource, _)| *resource)
                    .collect::<BTreeSet<_>>();
                let previous_resources = copied_resources
                    .insert(domain, next_resources.clone())
                    .unwrap_or_default();
                scene_panes.insert(domain, next_panes);
                scene_revisions.insert(domain, revision);
                if cx
                    .update_entity(&root, |view, cx| {
                        view.documents = store.managed_documents();
                        view.raw_geometry_revision = revision;
                        view.pending_frame_completion = Some(sequence);
                        for pane in &previous_panes {
                            view.pane_geometry.remove(pane);
                        }
                        view.pane_geometry.extend(geometry);
                        for resource in previous_resources.difference(&next_resources) {
                            view.canvas_images.remove(resource);
                        }
                        for (resource, image) in resources {
                            view.canvas_images.insert(resource, image);
                        }
                        view.reconcile_focus_handles(cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                true
            }
            PresentationCommand::RemoveExternalSurface { resource } => {
                #[cfg(target_os = "linux")]
                if cx
                    .update_entity(&root, |view, cx| view.remove_external_surface(resource, cx))
                    .is_err()
                {
                    break;
                }
                #[cfg(not(target_os = "linux"))]
                let _ = resource;
                true
            }
            PresentationCommand::SetCursor(cursor) => {
                if cx
                    .update_entity(&root, |view, cx| {
                        view.cursor = cursor;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                true
            }
            PresentationCommand::Shutdown => false,
        };
        if !keep_running {
            break;
        }
    }
    eprintln!("GPUI presentation driver stopped after {command_count} commands");
    drop(cx.update(|cx| cx.quit()));
}

fn external_surface_document(pane: UiPaneId, resource: u64, revision: u64) -> UiDocument {
    UiDocument {
        protocol_version: brood_protocol::managed_ui::MANAGED_UI_PROTOCOL_VERSION,
        pane,
        document_revision: revision.max(1),
        root: UiNode {
            id: 1,
            kind: UiNodeKind::ExternalSurface { surface: resource },
            style: brood_protocol::managed_ui::UiStyle {
                grow: 1.0,
                ..Default::default()
            },
            accessibility: brood_protocol::managed_ui::UiAccessibility {
                label: Some("Hosted application".to_string()),
                ..Default::default()
            },
            optional: false,
            interaction_revision: 0,
            children: Vec::new(),
        },
    }
}

fn copied_surface_document(pane: UiPaneId, resource: u64, revision: u64) -> UiDocument {
    let mut document = external_surface_document(pane, resource, revision);
    document.root.kind = UiNodeKind::Image {
        resource,
        alt: "Hosted application".to_string(),
    };
    document
}

/// Sender used by GPUI callbacks. `try_send` preserves bounded, non-blocking UI input.
#[derive(Clone)]
pub struct ActionSink {
    sender: std::sync::mpsc::SyncSender<UiAction>,
    interactions: Arc<Mutex<BTreeMap<(UiPaneId, u64, u64), u64>>>,
}

impl ActionSink {
    #[must_use]
    pub fn new(sender: std::sync::mpsc::SyncSender<UiAction>) -> Self {
        Self {
            sender,
            interactions: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn emit(
        &self,
        document: &UiDocument,
        actor_generation: u64,
        node: u64,
        value: UiActionValue,
    ) -> Result<(), AdapterError> {
        let interaction_revision = {
            let mut interactions = self
                .interactions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let revision = interactions
                .entry((document.pane, actor_generation, node))
                .or_default();
            *revision = revision.checked_add(1).ok_or_else(|| {
                AdapterError::Protocol(UiProtocolError {
                    code: brood_protocol::managed_ui::UiDiagnosticCode::InvalidRevision,
                    message: "interaction revision overflow".into(),
                })
            })?;
            let interaction_revision = *revision;
            drop(interactions);
            interaction_revision
        };
        self.sender
            .try_send(UiAction {
                pane: document.pane,
                actor_generation,
                document_revision: document.document_revision,
                node,
                interaction_revision,
                value,
            })
            .map_err(|_error| AdapterError::ActionBacklogFull)
    }

    pub fn clear_generation(&self, pane: UiPaneId) {
        self.interactions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(action_pane, _, _), _| *action_pane != pane);
    }
}

fn validate_node_interaction_floors(
    node: &UiNode,
    pane: UiPaneId,
    generation: u64,
    floors: &BTreeMap<(UiPaneId, u64, u64), u64>,
) -> Result<(), AdapterError> {
    let floor = floors
        .get(&(pane, generation, node.id))
        .copied()
        .unwrap_or_default();
    let hybrid = matches!(
        node.kind,
        UiNodeKind::Toggle {
            state_mode: brood_protocol::managed_ui::UiStateMode::Hybrid,
            ..
        } | UiNodeKind::RadioGroup {
            state_mode: brood_protocol::managed_ui::UiStateMode::Hybrid,
            ..
        } | UiNodeKind::TextField {
            state_mode: brood_protocol::managed_ui::UiStateMode::Hybrid,
            ..
        } | UiNodeKind::MultilineEditor {
            state_mode: brood_protocol::managed_ui::UiStateMode::Hybrid,
            ..
        } | UiNodeKind::SearchField {
            state_mode: brood_protocol::managed_ui::UiStateMode::Hybrid,
            ..
        }
    );
    if hybrid && node.interaction_revision < floor {
        return Err(AdapterError::Protocol(UiProtocolError {
            code: brood_protocol::managed_ui::UiDiagnosticCode::StalePublication,
            message: format!(
                "node {} acknowledges interaction {} before host revision {floor}",
                node.id, node.interaction_revision
            ),
        }));
    }
    for child in &node.children {
        validate_node_interaction_floors(child, pane, generation, floors)?;
    }
    Ok(())
}

/// Foreground-owned GPUI view rebuilt from complete accepted documents.
#[derive(Clone)]
pub struct ManagedDocument {
    pub actor_generation: u64,
    pub document: UiDocument,
}

pub struct ManagedUiRoot {
    documents: Vec<ManagedDocument>,
    actions: ActionSink,
    focus_handles: BTreeMap<(u64, UiPaneId, u64), FocusHandle>,
    text_inputs: BTreeMap<(u64, UiPaneId, u64), Entity<ManagedTextInput>>,
    raw_geometry_revision: u64,
    normalized_geometry_revision: u64,
    pane_geometry: BTreeMap<UiPaneId, PaneGeometry>,
    normalized_geometry: BTreeMap<UiPaneId, brood_wasm::LocalTransform>,
    canvas_packets: BTreeMap<u64, Arc<brood_graphics::Packet>>,
    canvas_text: BTreeMap<u64, CanvasTextLayout>,
    pending_text_measurements: Vec<(
        u64,
        CanvasTextLayout,
        std::sync::mpsc::SyncSender<
            Result<brood_wasm::TextLayoutResult, brood_wasm::PresentationError>,
        >,
    )>,
    canvas_images: BTreeMap<u64, Arc<gpui::RenderImage>>,
    frame_completions: Option<std::sync::mpsc::SyncSender<u64>>,
    pending_frame_completion: Option<u64>,
    input_sink: Option<std::sync::mpsc::SyncSender<BroodInputEvent>>,
    actor_input_sink: Option<std::sync::mpsc::SyncSender<ActorInputEvent>>,
    root_focus: FocusHandle,
    last_presentation_state: Option<(u32, u32, u32, u32, bool)>,
    last_traced_geometry_revision: u64,
    cursor: brood_wasm::CursorRole,
    #[cfg(target_os = "linux")]
    external_surfaces: BTreeMap<u64, Arc<gpui::ExternalSurface>>,
    #[cfg(target_os = "linux")]
    accessibility: accesskit_unix::Adapter,
    #[cfg(target_os = "linux")]
    accessibility_tree: Arc<Mutex<accesskit::TreeUpdate>>,
    #[cfg(target_os = "linux")]
    accessibility_actions: std::sync::mpsc::Receiver<accesskit::ActionRequest>,
    #[cfg(target_os = "linux")]
    accessibility_ids: BTreeMap<(u64, UiPaneId, u64), accesskit::NodeId>,
    #[cfg(target_os = "linux")]
    accessibility_targets: BTreeMap<accesskit::NodeId, (ManagedDocument, UiNode)>,
    #[cfg(target_os = "linux")]
    next_accessibility_id: u64,
}

impl ManagedUiRoot {
    #[must_use]
    pub fn new(
        documents: Vec<ManagedDocument>,
        actions: ActionSink,
        cx: &mut Context<Self>,
    ) -> Self {
        let root_focus = cx.focus_handle();
        #[cfg(target_os = "linux")]
        let (accessibility, accessibility_tree, accessibility_actions) = {
            let root = accesskit::NodeId(0);
            let mut root_node = accesskit::Node::new(accesskit::Role::Window);
            root_node.set_label("Brood");
            let tree = Arc::new(Mutex::new(accesskit::TreeUpdate {
                nodes: vec![(root, root_node)],
                tree: Some(accesskit::TreeInfo {
                    root,
                    toolkit_name: Some("Brood GPUI".to_string()),
                    toolkit_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                }),
                tree_id: accesskit::TreeId::ROOT,
                focus: root,
            }));
            let (sender, receiver) = std::sync::mpsc::sync_channel(256);
            let adapter = accesskit_unix::Adapter::new(
                AccessibilityActivation {
                    tree: Arc::clone(&tree),
                },
                AccessibilityActions(sender),
                AccessibilityDeactivation,
            );
            (adapter, tree, receiver)
        };
        let mut this = Self {
            documents,
            actions,
            focus_handles: BTreeMap::new(),
            text_inputs: BTreeMap::new(),
            raw_geometry_revision: 0,
            normalized_geometry_revision: 0,
            pane_geometry: BTreeMap::new(),
            normalized_geometry: BTreeMap::new(),
            canvas_packets: BTreeMap::new(),
            canvas_text: BTreeMap::new(),
            pending_text_measurements: Vec::new(),
            canvas_images: BTreeMap::new(),
            frame_completions: None,
            pending_frame_completion: None,
            input_sink: None,
            actor_input_sink: None,
            root_focus,
            last_presentation_state: None,
            last_traced_geometry_revision: 0,
            cursor: brood_wasm::CursorRole::Default,
            #[cfg(target_os = "linux")]
            external_surfaces: BTreeMap::new(),
            #[cfg(target_os = "linux")]
            accessibility,
            #[cfg(target_os = "linux")]
            accessibility_tree,
            #[cfg(target_os = "linux")]
            accessibility_actions,
            #[cfg(target_os = "linux")]
            accessibility_ids: BTreeMap::new(),
            #[cfg(target_os = "linux")]
            accessibility_targets: BTreeMap::new(),
            #[cfg(target_os = "linux")]
            next_accessibility_id: 1,
        };
        this.reconcile_focus_handles(cx);
        this
    }

    pub fn replace_documents(&mut self, documents: Vec<ManagedDocument>, cx: &mut Context<Self>) {
        self.documents = documents;
        self.reconcile_focus_handles(cx);
        cx.notify();
    }

    pub fn replace_geometry(
        &mut self,
        revision: u64,
        geometry: BTreeMap<UiPaneId, PaneGeometry>,
        cx: &mut Context<Self>,
    ) {
        if revision <= self.raw_geometry_revision {
            return;
        }
        self.raw_geometry_revision = revision;
        self.pane_geometry = geometry;
        cx.notify();
    }

    pub fn replace_normalized_geometry(
        &mut self,
        revision: u64,
        geometry: BTreeMap<UiPaneId, brood_wasm::LocalTransform>,
        cx: &mut Context<Self>,
    ) {
        if revision <= self.normalized_geometry_revision {
            return;
        }
        self.normalized_geometry_revision = revision;
        self.normalized_geometry = geometry;
        cx.notify();
    }

    pub fn set_frame_completion_sink(&mut self, sender: std::sync::mpsc::SyncSender<u64>) {
        self.frame_completions = Some(sender);
    }

    pub fn set_input_sink(&mut self, sender: std::sync::mpsc::SyncSender<BroodInputEvent>) {
        self.input_sink = Some(sender);
    }

    pub fn set_actor_input_sink(&mut self, sender: std::sync::mpsc::SyncSender<ActorInputEvent>) {
        self.actor_input_sink = Some(sender);
    }

    /// Associates a transport resource with an `external-surface` node.
    #[cfg(target_os = "linux")]
    pub fn replace_external_surface(
        &mut self,
        resource: u64,
        surface: Arc<gpui::ExternalSurface>,
        cx: &mut Context<Self>,
    ) {
        self.external_surfaces.insert(resource, surface);
        cx.notify();
    }

    /// Releases a transport resource after no coherent candidate refers to it.
    #[cfg(target_os = "linux")]
    pub fn remove_external_surface(&mut self, resource: u64, cx: &mut Context<Self>) {
        self.external_surfaces.remove(&resource);
        cx.notify();
    }

    fn reconcile_focus_handles(&mut self, cx: &mut Context<Self>) {
        let mut live = BTreeSet::new();
        for managed in &self.documents {
            collect_focusable_nodes(
                &managed.document.root,
                managed.actor_generation,
                managed.document.pane,
                &mut live,
            );
        }
        self.focus_handles.retain(|key, _| live.contains(key));
        for key in live {
            self.focus_handles
                .entry(key)
                .or_insert_with(|| cx.focus_handle().tab_stop(true));
        }
        let mut text = Vec::new();
        for managed in &self.documents {
            collect_text_nodes(&managed.document.root, managed, &mut text);
        }
        let live_text = text
            .iter()
            .map(|(key, _, _, _, _)| *key)
            .collect::<BTreeSet<_>>();
        self.text_inputs.retain(|key, _| live_text.contains(key));
        for (key, value, placeholder, state_mode, managed) in text {
            if let Some(input) = self.text_inputs.get(&key) {
                input.update(cx, |input, _cx| {
                    input.reconcile(value, placeholder, state_mode, managed)
                });
            } else {
                let actions = self.actions.clone();
                self.text_inputs.insert(
                    key,
                    cx.new(|cx| {
                        ManagedTextInput::new(
                            value,
                            placeholder,
                            state_mode,
                            managed,
                            key.2,
                            actions,
                            cx,
                        )
                    }),
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn visit_ui_nodes(node: &UiNode, visit: &mut impl FnMut(&UiNode)) {
    visit(node);
    for child in &node.children {
        visit_ui_nodes(child, visit);
    }
}

#[cfg(target_os = "linux")]
const fn accesskit_role(role: brood_protocol::managed_ui::UiAccessibilityRole) -> accesskit::Role {
    use brood_protocol::managed_ui::UiAccessibilityRole as UiRole;
    match role {
        UiRole::Generic => accesskit::Role::GenericContainer,
        UiRole::Button => accesskit::Role::Button,
        UiRole::Checkbox => accesskit::Role::CheckBox,
        UiRole::RadioGroup => accesskit::Role::RadioGroup,
        UiRole::TextField => accesskit::Role::TextInput,
        UiRole::SearchBox => accesskit::Role::SearchInput,
        UiRole::List => accesskit::Role::List,
        UiRole::ListItem => accesskit::Role::ListItem,
        UiRole::Link => accesskit::Role::Link,
        UiRole::Dialog => accesskit::Role::Dialog,
        UiRole::ProgressIndicator => accesskit::Role::ProgressIndicator,
        UiRole::Terminal => accesskit::Role::Terminal,
        UiRole::Image => accesskit::Role::Image,
    }
}

#[cfg(target_os = "linux")]
fn inferred_accesskit_role(node: &UiNode) -> accesskit::Role {
    use brood_protocol::managed_ui::UiAccessibilityRole;
    if node.accessibility.role != UiAccessibilityRole::Generic {
        return accesskit_role(node.accessibility.role);
    }
    match &node.kind {
        UiNodeKind::Text { .. } | UiNodeKind::Badge { .. } | UiNodeKind::Tooltip { .. } => {
            accesskit::Role::Label
        }
        UiNodeKind::Image { .. } => accesskit::Role::Image,
        UiNodeKind::Button { .. } | UiNodeKind::CommandTarget { .. } => accesskit::Role::Button,
        UiNodeKind::Toggle { .. } => accesskit::Role::Switch,
        UiNodeKind::RadioGroup { .. } => accesskit::Role::RadioGroup,
        UiNodeKind::Menu { .. } | UiNodeKind::ContextMenu => accesskit::Role::Menu,
        UiNodeKind::SelectableRow { .. } => accesskit::Role::ListBoxOption,
        UiNodeKind::Link { .. } => accesskit::Role::Link,
        UiNodeKind::TextField { .. } => accesskit::Role::TextInput,
        UiNodeKind::MultilineEditor { .. } | UiNodeKind::TextEditorBridge { .. } => {
            accesskit::Role::MultilineTextInput
        }
        UiNodeKind::SearchField { .. } => accesskit::Role::SearchInput,
        UiNodeKind::Dialog { .. } => accesskit::Role::Dialog,
        UiNodeKind::Progress { .. } => accesskit::Role::ProgressIndicator,
        UiNodeKind::VirtualList { .. } => accesskit::Role::List,
        UiNodeKind::Canvas { .. } => accesskit::Role::Canvas,
        UiNodeKind::TerminalView { .. } => accesskit::Role::Terminal,
        _ => accesskit::Role::GenericContainer,
    }
}

#[cfg(target_os = "linux")]
fn default_accessible_label(kind: &UiNodeKind) -> Option<&str> {
    match kind {
        UiNodeKind::Text { text, .. }
        | UiNodeKind::Badge { text }
        | UiNodeKind::Tooltip { text } => Some(text),
        UiNodeKind::Icon { name } => Some(name),
        UiNodeKind::Image { alt, .. } => Some(alt),
        UiNodeKind::Button { label }
        | UiNodeKind::Toggle { label, .. }
        | UiNodeKind::Menu { label }
        | UiNodeKind::Link { label, .. } => Some(label),
        UiNodeKind::Dialog { title } => Some(title),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn build_accessibility_update(
    documents: &[ManagedDocument],
    ids: &mut BTreeMap<(u64, UiPaneId, u64), accesskit::NodeId>,
    targets: &mut BTreeMap<accesskit::NodeId, (ManagedDocument, UiNode)>,
    next_id: &mut u64,
    geometry: &BTreeMap<UiPaneId, PaneGeometry>,
    focus_handles: &BTreeMap<(u64, UiPaneId, u64), FocusHandle>,
    window: &Window,
) -> accesskit::TreeUpdate {
    let mut live_keys = BTreeSet::new();
    for managed in documents {
        visit_ui_nodes(&managed.document.root, &mut |node| {
            let key = (managed.actor_generation, managed.document.pane, node.id);
            live_keys.insert(key);
            ids.entry(key).or_insert_with(|| {
                let id = accesskit::NodeId(*next_id);
                *next_id = next_id.checked_add(1).unwrap_or(1);
                id
            });
        });
    }
    ids.retain(|key, _| live_keys.contains(key));
    targets.clear();

    let root_id = accesskit::NodeId(0);
    let mut root = accesskit::Node::new(accesskit::Role::Window);
    root.set_label("Brood");
    let root_children = documents
        .iter()
        .filter_map(|managed| {
            ids.get(&(
                managed.actor_generation,
                managed.document.pane,
                managed.document.root.id,
            ))
            .copied()
        })
        .collect::<Vec<_>>();
    root.set_children(root_children);
    let mut nodes = vec![(root_id, root)];
    let mut focus = root_id;

    for managed in documents {
        visit_ui_nodes(&managed.document.root, &mut |ui_node| {
            let key = (managed.actor_generation, managed.document.pane, ui_node.id);
            let Some(id) = ids.get(&key).copied() else {
                return;
            };
            let mut node = accesskit::Node::new(inferred_accesskit_role(ui_node));
            if let Some(label) = ui_node
                .accessibility
                .label
                .as_deref()
                .or_else(|| default_accessible_label(&ui_node.kind))
            {
                node.set_label(label);
            }
            if let Some(description) = &ui_node.accessibility.description {
                node.set_description(description);
            }
            if let Some(value) = &ui_node.accessibility.value {
                node.set_value(value);
            }
            if ui_node.accessibility.disabled {
                node.set_disabled();
            }
            if ui_node.accessibility.expanded == Some(true) {
                node.set_expanded(true);
            }
            if ui_node.accessibility.selected == Some(true) {
                node.set_selected(true);
            }
            match &ui_node.kind {
                UiNodeKind::Toggle { value, .. } => node.set_toggled((*value).into()),
                UiNodeKind::SelectableRow { selected } if *selected => node.set_selected(true),
                UiNodeKind::Progress { value: Some(value) } => {
                    node.set_numeric_value(f64::from(*value));
                    node.set_min_numeric_value(0.0);
                    node.set_max_numeric_value(1.0);
                }
                UiNodeKind::TextField { value, .. }
                | UiNodeKind::SearchField { value, .. }
                | UiNodeKind::MultilineEditor { value, .. } => node.set_value(value),
                _ => {}
            }
            let children = ui_node
                .children
                .iter()
                .filter_map(|child| {
                    ids.get(&(managed.actor_generation, managed.document.pane, child.id))
                        .copied()
                })
                .collect::<Vec<_>>();
            node.set_children(children);
            let related = |related: &[u64]| {
                related
                    .iter()
                    .filter_map(|related| {
                        ids.get(&(managed.actor_generation, managed.document.pane, *related))
                            .copied()
                    })
                    .collect::<Vec<_>>()
            };
            node.set_labelled_by(related(&ui_node.accessibility.labelled_by));
            node.set_described_by(related(&ui_node.accessibility.described_by));
            if let Some(bounds) = geometry.get(&managed.document.pane) {
                node.set_bounds(accesskit::Rect {
                    x0: f64::from(bounds.x),
                    y0: f64::from(bounds.y),
                    x1: f64::from(bounds.x + bounds.width),
                    y1: f64::from(bounds.y + bounds.height),
                });
            }
            let interactive = matches!(
                ui_node.kind,
                UiNodeKind::Button { .. }
                    | UiNodeKind::Toggle { .. }
                    | UiNodeKind::SelectableRow { .. }
                    | UiNodeKind::Link { .. }
                    | UiNodeKind::CommandTarget { .. }
                    | UiNodeKind::Menu { .. }
            );
            if interactive || !ui_node.accessibility.commands.is_empty() {
                node.add_action(accesskit::Action::Click);
            }
            if focus_handles.contains_key(&key) {
                node.add_action(accesskit::Action::Focus);
            }
            if matches!(
                ui_node.kind,
                UiNodeKind::TextField { .. }
                    | UiNodeKind::SearchField { .. }
                    | UiNodeKind::MultilineEditor { .. }
            ) {
                node.add_action(accesskit::Action::ReplaceSelectedText);
            }
            if focus_handles
                .get(&key)
                .is_some_and(|handle| handle.is_focused(window))
            {
                focus = id;
            }
            targets.insert(id, (managed.clone(), ui_node.clone()));
            nodes.push((id, node));
        });
    }
    accesskit::TreeUpdate {
        nodes,
        tree: Some(accesskit::TreeInfo {
            root: root_id,
            toolkit_name: Some("Brood GPUI".to_string()),
            toolkit_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        }),
        tree_id: accesskit::TreeId::ROOT,
        focus,
    }
}

#[cfg(target_os = "linux")]
fn handle_accessibility_action(
    request: accesskit::ActionRequest,
    targets: &BTreeMap<accesskit::NodeId, (ManagedDocument, UiNode)>,
    focus_handles: &BTreeMap<(u64, UiPaneId, u64), FocusHandle>,
    actions: &ActionSink,
    window: &mut Window,
) {
    let Some((managed, node)) = targets.get(&request.target_node) else {
        return;
    };
    match request.action {
        accesskit::Action::Focus => {
            if let Some(handle) =
                focus_handles.get(&(managed.actor_generation, managed.document.pane, node.id))
            {
                handle.focus(window);
            }
        }
        accesskit::Action::ReplaceSelectedText => {
            if let Some(accesskit::ActionData::Value(value)) = request.data {
                if actions
                    .emit(
                        &managed.document,
                        managed.actor_generation,
                        node.id,
                        UiActionValue::Text(value.into()),
                    )
                    .is_err()
                {}
            }
        }
        accesskit::Action::Click => {
            let value = match &node.kind {
                UiNodeKind::Toggle { value, .. } => UiActionValue::Toggle(!value),
                _ => UiActionValue::Activate,
            };
            if actions
                .emit(&managed.document, managed.actor_generation, node.id, value)
                .is_err()
            {}
        }
        _ => {}
    }
}

impl Render for ManagedUiRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for (resource, layout, response) in self.pending_text_measurements.drain(..) {
            let result = measure_canvas_text(resource, &layout, window);
            if result.is_ok() {
                self.canvas_text.insert(resource, layout);
            }
            if response.try_send(result).is_err() {}
        }
        let root_entity = cx.weak_entity();
        let bounds = window.bounds();
        let cursor = match self.cursor {
            brood_wasm::CursorRole::Default => gpui::CursorStyle::Arrow,
            brood_wasm::CursorRole::Pointer => gpui::CursorStyle::PointingHand,
            brood_wasm::CursorRole::ColResize => gpui::CursorStyle::ResizeLeftRight,
            brood_wasm::CursorRole::RowResize => gpui::CursorStyle::ResizeUpDown,
        };
        let width = f32::from(bounds.size.width).max(1.0).round() as u32;
        let height = f32::from(bounds.size.height).max(1.0).round() as u32;
        let scale_120 = (window.scale_factor() * 120.0).round().max(1.0) as u32;
        let refresh_millihz = window
            .display(cx)
            .and_then(|display| display.refresh_rate_millihz())
            .unwrap_or(60_000);
        let focused = window.is_window_active();
        let presentation_state = (width, height, refresh_millihz, scale_120, focused);
        if self.last_presentation_state != Some(presentation_state) {
            if let Some(sink) = self.input_sink.as_ref() {
                if self.last_presentation_state.map(|state| state.4) != Some(focused) {
                    drop(sink.try_send(BroodInputEvent::PresentationFocus { focused }));
                }
                drop(sink.try_send(BroodInputEvent::PresentationSize {
                    width,
                    height,
                    refresh_millihz,
                    scale_120,
                }));
            }
            self.last_presentation_state = Some(presentation_state);
            eprintln!(
                "GPUI presentation state size={width}x{height} refresh_millihz={refresh_millihz} scale_120={scale_120} focused={focused}"
            );
        }
        if let Some(sequence) = self.pending_frame_completion.take()
            && let Some(sender) = self.frame_completions.clone()
        {
            // GPUI invokes this at the next frame boundary. Brood's Blade fork
            // completes the prior GPU submission before returning from draw.
            window.on_next_frame(move |_window, _cx| if sender.try_send(sequence).is_err() {});
            window.request_animation_frame();
        }
        let actor_input_sink = self.actor_input_sink.clone();
        let mut geometry = self.pane_geometry.clone();
        for (pane, transform) in &self.normalized_geometry {
            let x = ((transform.offset.u + 1.0) * 0.5 * f64::from(width)) as f32;
            let y = ((transform.offset.v + 1.0) * 0.5 * f64::from(height)) as f32;
            let pane_width = (transform.size.u * 0.5 * f64::from(width)) as f32;
            let pane_height = (transform.size.v * 0.5 * f64::from(height)) as f32;
            geometry.insert(
                *pane,
                PaneGeometry {
                    x,
                    y,
                    width: pane_width,
                    height: pane_height,
                },
            );
        }
        if std::env::var_os("BROOD_TRACE_RESIZE").is_some()
            && self.normalized_geometry_revision > self.last_traced_geometry_revision
        {
            let mut edges = BTreeMap::<i32, f32>::new();
            for pane in geometry.values() {
                for edge in [pane.x, pane.x + pane.width] {
                    if edge > 1.0 && edge < width as f32 - 1.0 {
                        *edges.entry(edge.round() as i32).or_default() += pane.height;
                    }
                }
            }
            if let Some((divider_x, _)) = edges
                .into_iter()
                .max_by(|left, right| left.1.total_cmp(&right.1))
            {
                eprintln!(
                    "resize-present revision={} divider_x={divider_x} phase=settled",
                    self.normalized_geometry_revision
                );
            }
            self.last_traced_geometry_revision = self.normalized_geometry_revision;
        }
        #[cfg(target_os = "linux")]
        {
            while let Ok(request) = self.accessibility_actions.try_recv() {
                handle_accessibility_action(
                    request,
                    &self.accessibility_targets,
                    &self.focus_handles,
                    &self.actions,
                    window,
                );
            }
            let update = build_accessibility_update(
                &self.documents,
                &mut self.accessibility_ids,
                &mut self.accessibility_targets,
                &mut self.next_accessibility_id,
                &geometry,
                &self.focus_handles,
                window,
            );
            *self
                .accessibility_tree
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = update.clone();
            self.accessibility.update_window_focus_state(focused);
            self.accessibility.update_if_active(|| update);
        }
        div()
            .id("brood-managed-root")
            .flex()
            .flex_col()
            .relative()
            .size_full()
            .bg(rgb(0x0011_1318))
            .cursor(cursor)
            .track_focus(&self.root_focus)
            .on_mouse_move({
                let actor_sink = actor_input_sink.clone();
                move |event: &MouseMoveEvent, _window, _cx| {
                    if let Some(sink) = actor_sink.as_ref() {
                        drop(sink.try_send(ActorInputEvent::PointerMotion {
                            x: event.position.x.into(),
                            y: event.position.y.into(),
                            modifiers: brood_modifiers(event.modifiers),
                        }));
                    }
                }
            })
            .capture_key_down({
                let actor_sink = actor_input_sink.clone();
                move |event: &KeyDownEvent, _window, cx| {
                    let modifiers = brood_modifiers(event.keystroke.modifiers);
                    if let Some(actor_sink) = actor_sink.as_ref() {
                        drop(actor_sink.try_send(ActorInputEvent::Key {
                            keycode: event.keycode,
                            logical_key: event.keystroke.key.clone(),
                            pressed: true,
                            modifiers,
                        }));
                        let command =
                            event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
                        if command && event.keystroke.key == "v" {
                            if let Some(text) =
                                cx.read_from_clipboard().and_then(|item| item.text())
                            {
                                drop(actor_sink.try_send(ActorInputEvent::Paste(text)));
                            }
                        } else if !command
                            && !event.keystroke.modifiers.alt
                            && let Some(text) = event.keystroke.key_char.clone()
                        {
                            drop(actor_sink.try_send(ActorInputEvent::Text(text)));
                        }
                    }
                }
            })
            .capture_key_up({
                let actor_sink = actor_input_sink.clone();
                move |event: &KeyUpEvent, _window, _cx| {
                    if let Some(actor_sink) = actor_sink.as_ref() {
                        drop(actor_sink.try_send(ActorInputEvent::Key {
                            keycode: event.keycode,
                            logical_key: event.keystroke.key.clone(),
                            pressed: false,
                            modifiers: brood_modifiers(event.keystroke.modifiers),
                        }));
                    }
                }
            })
            .capture_any_mouse_down({
                let actor_sink = actor_input_sink.clone();
                let focus = self.root_focus.clone();
                move |event: &MouseDownEvent, window, _cx| {
                    focus.focus(window);
                    if let Some(sink) = actor_sink.as_ref() {
                        drop(sink.try_send(ActorInputEvent::PointerButton {
                            x: event.position.x.into(),
                            y: event.position.y.into(),
                            button: linux_mouse_button(event.button),
                            pressed: true,
                            modifiers: brood_modifiers(event.modifiers),
                        }));
                    }
                }
            })
            .capture_any_mouse_up({
                let actor_sink = actor_input_sink.clone();
                move |event: &MouseUpEvent, _window, _cx| {
                    if let Some(sink) = actor_sink.as_ref() {
                        drop(sink.try_send(ActorInputEvent::PointerButton {
                            x: event.position.x.into(),
                            y: event.position.y.into(),
                            button: linux_mouse_button(event.button),
                            pressed: false,
                            modifiers: brood_modifiers(event.modifiers),
                        }));
                    }
                }
            })
            .on_scroll_wheel(move |event: &ScrollWheelEvent, window, _cx| {
                if let Some(sink) = actor_input_sink.as_ref() {
                    let delta = event.delta.pixel_delta(window.line_height());
                    drop(sink.try_send(ActorInputEvent::PointerScroll {
                        x: event.position.x.into(),
                        y: event.position.y.into(),
                        horizontal: delta.x.into(),
                        vertical: delta.y.into(),
                        modifiers: brood_modifiers(event.modifiers),
                    }));
                }
            })
            .children(self.documents.iter().map(|managed| {
                let mut pane = div()
                    .id(("managed-pane", managed.document.pane))
                    .flex()
                    .flex_1()
                    .overflow_hidden();
                if let Some(geometry) = geometry.get(&managed.document.pane) {
                    pane = pane
                        .absolute()
                        .left(px(geometry.x))
                        .top(px(geometry.y))
                        .w(px(geometry.width))
                        .h(px(geometry.height))
                        .flex_none();
                }
                pane.child(render_node(
                    &managed.document.root,
                    managed,
                    &self.actions,
                    &root_entity,
                    &self.canvas_packets,
                    &self.canvas_text,
                    &self.canvas_images,
                    &self.focus_handles,
                    &self.text_inputs,
                    #[cfg(target_os = "linux")]
                    &self.external_surfaces,
                ))
            }))
    }
}

const fn linux_mouse_button(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Navigate(NavigationDirection::Back) => 0x113,
        MouseButton::Navigate(NavigationDirection::Forward) => 0x114,
    }
}

fn brood_modifiers(modifiers: gpui::Modifiers) -> u32 {
    u32::from(modifiers.control)
        | (u32::from(modifiers.alt) << 1)
        | (u32::from(modifiers.shift) << 2)
        | (u32::from(modifiers.platform) << 3)
}

fn render_node(
    node: &UiNode,
    managed: &ManagedDocument,
    actions: &ActionSink,
    root_entity: &WeakEntity<ManagedUiRoot>,
    canvas_packets: &BTreeMap<u64, Arc<brood_graphics::Packet>>,
    canvas_text: &BTreeMap<u64, CanvasTextLayout>,
    canvas_images: &BTreeMap<u64, Arc<gpui::RenderImage>>,
    focus_handles: &BTreeMap<(u64, UiPaneId, u64), FocusHandle>,
    text_inputs: &BTreeMap<(u64, UiPaneId, u64), Entity<ManagedTextInput>>,
    #[cfg(target_os = "linux")] external_surfaces: &BTreeMap<u64, Arc<gpui::ExternalSurface>>,
) -> AnyElement {
    if matches!(
        node.kind,
        UiNodeKind::TextField { .. }
            | UiNodeKind::SearchField { .. }
            | UiNodeKind::MultilineEditor { .. }
    ) && let Some(input) =
        text_inputs.get(&(managed.actor_generation, managed.document.pane, node.id))
    {
        return input.clone().into_any_element();
    }
    #[cfg(target_os = "linux")]
    if let UiNodeKind::ExternalSurface { surface } = node.kind
        && let Some(external) = external_surfaces.get(&surface)
    {
        return gpui::surface(Arc::clone(external))
            .size_full()
            .into_any_element();
    }
    if let UiNodeKind::Canvas { target } | UiNodeKind::TerminalView { terminal: target } = node.kind
        && let Some(packet) = canvas_packets.get(&target)
    {
        let packet = Arc::clone(packet);
        let text = canvas_text.clone();
        let images = canvas_images.clone();
        return gpui::canvas(
            |_, _, _| {},
            move |bounds, (), window, cx| {
                paint_canvas_packet(&packet, bounds, window, cx, &text, &images);
            },
        )
        .size_full()
        .into_any_element();
    }
    if let UiNodeKind::Image { resource, .. } = node.kind
        && let Some(image) = canvas_images.get(&resource)
    {
        return img(Arc::clone(image)).size_full().into_any_element();
    }
    let mut element = div()
        .id((SharedString::from(format!("node-{}", node.id)), 0_usize))
        .flex()
        .children(node.children.iter().map(|child| {
            let rendered = render_node(
                child,
                managed,
                actions,
                root_entity,
                canvas_packets,
                canvas_text,
                canvas_images,
                focus_handles,
                text_inputs,
                #[cfg(target_os = "linux")]
                external_surfaces,
            );
            if matches!(node.kind, UiNodeKind::Stack) {
                div()
                    .absolute()
                    .size_full()
                    .child(rendered)
                    .into_any_element()
            } else {
                rendered
            }
        }));

    element = match &node.kind {
        UiNodeKind::Column => element.flex_col(),
        UiNodeKind::Stack => element.relative(),
        UiNodeKind::Grid { columns } => element.grid().grid_cols((*columns).max(1)),
        UiNodeKind::ScrollRegion { axis } => match axis {
            brood_protocol::managed_ui::UiAxis::Horizontal => element.overflow_x_scroll(),
            brood_protocol::managed_ui::UiAxis::Vertical => element.overflow_y_scroll(),
        },
        UiNodeKind::Text { text, role } => {
            let element = element.child(text.clone());
            match role {
                brood_protocol::managed_ui::UiTextRole::Caption => element.text_xs(),
                brood_protocol::managed_ui::UiTextRole::Body => element.text_base(),
                brood_protocol::managed_ui::UiTextRole::Label => {
                    element.text_sm().font_weight(gpui::FontWeight::MEDIUM)
                }
                brood_protocol::managed_ui::UiTextRole::Heading => {
                    element.text_lg().font_weight(gpui::FontWeight::SEMIBOLD)
                }
                brood_protocol::managed_ui::UiTextRole::Monospace => {
                    element.font_family("monospace")
                }
            }
        }
        UiNodeKind::Badge { text } => element
            .child(text.clone())
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(theme_color(UiColorToken::Surface)),
        UiNodeKind::Tooltip { text } => element
            .child(text.clone())
            .absolute()
            .px_2()
            .py_1()
            .rounded_sm()
            .shadow_sm()
            .bg(theme_color(UiColorToken::Surface)),
        UiNodeKind::Icon { name } => element.child(name.clone()),
        UiNodeKind::Image { alt, .. } => element.child(alt.clone()),
        UiNodeKind::Button { label } | UiNodeKind::Menu { label } => {
            element.child(label.clone()).cursor_pointer()
        }
        UiNodeKind::Toggle { label, value, .. } => element
            .child(format!("{} {}", if *value { "✓" } else { "○" }, label))
            .cursor_pointer(),
        UiNodeKind::Link { label, .. } => element.child(label.clone()).cursor_pointer(),
        UiNodeKind::TextField {
            value, placeholder, ..
        }
        | UiNodeKind::SearchField {
            value, placeholder, ..
        } => element.child(if value.is_empty() {
            placeholder.clone()
        } else {
            value.clone()
        }),
        UiNodeKind::MultilineEditor { value, .. } => element.child(value.clone()),
        UiNodeKind::Dialog { title } => element
            .flex_col()
            .child(title.clone())
            .border_1()
            .border_color(theme_color(UiColorToken::Border))
            .rounded_md()
            .shadow_lg()
            .bg(theme_color(UiColorToken::Surface)),
        UiNodeKind::Progress { value } => element
            .child(value.map_or_else(|| "…".into(), |value| format!("{:.0}%", value * 100.0)))
            .overflow_hidden()
            .rounded_sm()
            .bg(theme_color(UiColorToken::Surface)),
        UiNodeKind::Canvas { target } => element.child(format!("canvas:{target}")),
        UiNodeKind::ExternalSurface { surface } => element.child(format!("surface:{surface}")),
        UiNodeKind::TerminalView { terminal } => element.child(format!("terminal:{terminal}")),
        UiNodeKind::Spacer => element.flex_grow(),
        UiNodeKind::Divider { axis } | UiNodeKind::Separator { axis } => match axis {
            brood_protocol::managed_ui::UiAxis::Horizontal => element
                .h(px(1.0))
                .w_full()
                .bg(theme_color(UiColorToken::Border)),
            brood_protocol::managed_ui::UiAxis::Vertical => element
                .w(px(1.0))
                .h_full()
                .bg(theme_color(UiColorToken::Border)),
        },
        UiNodeKind::VirtualList { .. } => element.flex_col().overflow_y_scroll(),
        UiNodeKind::RadioGroup { value, .. } => element
            .flex_col()
            .when_some(value.clone(), |element, value| {
                element.child(format!("selected: {value}"))
            }),
        UiNodeKind::SelectableRow { selected } => {
            element.cursor_pointer().when(*selected, |element| {
                element.bg(theme_color(UiColorToken::Accent))
            })
        }
        UiNodeKind::CommandTarget { command } => element.child(command.clone()).cursor_pointer(),
        UiNodeKind::TextEditorBridge { editor } => element
            .child(format!("editor:{editor}"))
            .border_1()
            .border_color(theme_color(UiColorToken::Border)),
        UiNodeKind::Popover | UiNodeKind::ContextMenu => element
            .absolute()
            .border_1()
            .border_color(theme_color(UiColorToken::Border))
            .rounded_md()
            .shadow_lg()
            .bg(theme_color(UiColorToken::Surface)),
        UiNodeKind::DragSource { .. } => element.cursor(gpui::CursorStyle::OpenHand),
        UiNodeKind::DropTarget { .. } => element
            .border_1()
            .border_color(theme_color(UiColorToken::Border)),
        UiNodeKind::Row | UiNodeKind::Unsupported => element,
    };

    if let UiNodeKind::DragSource { payload_type } = &node.kind {
        let payload = ManagedDragPayload {
            payload_type: payload_type.clone(),
            bytes: Vec::new(),
        };
        let actions = actions.clone();
        let managed = managed.clone();
        let node_id = node.id;
        element = element.on_drag(payload, move |payload, _offset, _window, cx| {
            drop(actions.emit(
                &managed.document,
                managed.actor_generation,
                node_id,
                UiActionValue::Drag {
                    payload_type: payload.payload_type.clone(),
                    bytes: payload.bytes.clone(),
                },
            ));
            cx.new(|_| ManagedDragPreview(payload.payload_type.clone()))
        });
    }
    if let UiNodeKind::DropTarget { accepted_types } = &node.kind {
        let accepted = accepted_types.clone();
        element = element.can_drop(move |value, _window, _cx| {
            value
                .downcast_ref::<ManagedDragPayload>()
                .is_some_and(|payload| accepted.contains(&payload.payload_type))
        });
        let actions = actions.clone();
        let managed = managed.clone();
        let node_id = node.id;
        element = element.on_drop(move |payload: &ManagedDragPayload, _window, _cx| {
            drop(actions.emit(
                &managed.document,
                managed.actor_generation,
                node_id,
                UiActionValue::Drag {
                    payload_type: payload.payload_type.clone(),
                    bytes: payload.bytes.clone(),
                },
            ));
        });
    }

    if is_activatable(&node.kind) {
        let actions = actions.clone();
        let managed = managed.clone();
        let node_id = node.id;
        let action_value = match &node.kind {
            UiNodeKind::Toggle { value, .. } => UiActionValue::Toggle(!value),
            _ => UiActionValue::Activate,
        };
        let state_mode = match &node.kind {
            UiNodeKind::Toggle { state_mode, .. }
            | UiNodeKind::RadioGroup { state_mode, .. }
            | UiNodeKind::TextField { state_mode, .. }
            | UiNodeKind::MultilineEditor { state_mode, .. }
            | UiNodeKind::SearchField { state_mode, .. } => Some(*state_mode),
            _ => None,
        };
        let root_entity = root_entity.clone();
        let focus = focus_handles
            .get(&(managed.actor_generation, managed.document.pane, node.id))
            .cloned();
        if let Some(handle) = focus.as_ref() {
            element = element.track_focus(handle);
        }
        element = element.on_click(move |_event, window, cx| {
            if let Some(handle) = focus.as_ref() {
                handle.focus(window);
            }
            if let Err(error) = actions.emit(
                &managed.document,
                managed.actor_generation,
                node_id,
                action_value.clone(),
            ) {
                eprintln!("managed UI action rejected: {error}");
            }
            if state_mode
                .is_some_and(|mode| mode != brood_protocol::managed_ui::UiStateMode::Controlled)
            {
                drop(root_entity.update(cx, |root, cx| {
                    if let Some(document) = root.documents.iter_mut().find(|candidate| {
                        candidate.actor_generation == managed.actor_generation
                            && candidate.document.pane == managed.document.pane
                    }) {
                        apply_transient_action(&mut document.document.root, node_id, &action_value);
                        cx.notify();
                    }
                }));
            }
        });
    }
    apply_style(element, &node.style).into_any_element()
}

#[derive(Clone)]
struct ManagedDragPayload {
    payload_type: String,
    bytes: Vec<u8>,
}

struct ManagedDragPreview(String);

impl Render for ManagedDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(rgb(0x0024_2830))
            .child(self.0.clone())
    }
}

fn apply_transient_action(node: &mut UiNode, target: u64, value: &UiActionValue) -> bool {
    if node.id == target {
        if let (UiNodeKind::Toggle { value: current, .. }, UiActionValue::Toggle(next)) =
            (&mut node.kind, value)
        {
            *current = *next;
        }
        return true;
    }
    node.children
        .iter_mut()
        .any(|child| apply_transient_action(child, target, value))
}

struct ManagedTextInput {
    value: String,
    placeholder: String,
    selection: Range<usize>,
    marked: Option<Range<usize>>,
    state_mode: brood_protocol::managed_ui::UiStateMode,
    managed: ManagedDocument,
    node: u64,
    actions: ActionSink,
    focus: FocusHandle,
}

impl ManagedTextInput {
    fn new(
        value: String,
        placeholder: String,
        state_mode: brood_protocol::managed_ui::UiStateMode,
        managed: ManagedDocument,
        node: u64,
        actions: ActionSink,
        cx: &mut Context<Self>,
    ) -> Self {
        let end = value.encode_utf16().count();
        Self {
            value,
            placeholder,
            selection: end..end,
            marked: None,
            state_mode,
            managed,
            node,
            actions,
            focus: cx.focus_handle().tab_stop(true),
        }
    }

    fn reconcile(
        &mut self,
        value: String,
        placeholder: String,
        state_mode: brood_protocol::managed_ui::UiStateMode,
        managed: ManagedDocument,
    ) {
        if state_mode == brood_protocol::managed_ui::UiStateMode::Controlled
            || managed.document.document_revision > self.managed.document.document_revision
        {
            self.value = value;
            let end = self.value.encode_utf16().count();
            self.selection = end..end;
        }
        self.placeholder = placeholder;
        self.state_mode = state_mode;
        self.managed = managed;
    }

    fn byte_offset(&self, utf16: usize) -> usize {
        let mut units = 0;
        for (byte, character) in self.value.char_indices() {
            if units >= utf16 {
                return byte;
            }
            units = units.saturating_add(character.len_utf16());
        }
        self.value.len()
    }

    fn replace(&mut self, range: Option<Range<usize>>, text: &str, cx: &mut Context<Self>) {
        let range = range
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection.clone());
        let bytes = self.byte_offset(range.start)..self.byte_offset(range.end);
        self.value.replace_range(bytes, text);
        let end = range.start.saturating_add(text.encode_utf16().count());
        self.selection = end..end;
        self.marked = None;
        if let Err(error) = self.actions.emit(
            &self.managed.document,
            self.managed.actor_generation,
            self.node,
            UiActionValue::Text(self.value.clone()),
        ) {
            eprintln!("managed text action rejected: {error}");
        }
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let command = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        match (command, event.keystroke.key.as_str()) {
            (true, "a") => self.selection = 0..self.value.encode_utf16().count(),
            (true, "c") => {
                let bytes =
                    self.byte_offset(self.selection.start)..self.byte_offset(self.selection.end);
                if let Some(text) = self.value.get(bytes) {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string()));
                }
            }
            (true, "x") => {
                let bytes =
                    self.byte_offset(self.selection.start)..self.byte_offset(self.selection.end);
                if let Some(text) = self.value.get(bytes).map(str::to_string) {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                    self.replace(None, "", cx);
                }
            }
            (true, "v") => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.replace_text_in_range(None, &text, window, cx);
                }
            }
            (false, "backspace") => {
                if self.selection.is_empty() && self.selection.start > 0 {
                    self.selection = self.selection.start - 1..self.selection.end;
                }
                self.replace(None, "", cx);
            }
            (false, "enter") => {
                if let Err(error) = self.actions.emit(
                    &self.managed.document,
                    self.managed.actor_generation,
                    self.node,
                    UiActionValue::Submit,
                ) {
                    eprintln!("managed text submit rejected: {error}");
                }
            }
            _ => return,
        }
        cx.notify();
    }
}

impl EntityInputHandler for ManagedTextInput {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let start = self.byte_offset(range.start);
        let end = self.byte_offset(range.end);
        adjusted.replace(range);
        self.value.get(start..end).map(str::to_string)
    }
    fn selected_text_range(
        &mut self,
        _ignore_disabled: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.selection.clone(),
            reversed: false,
        })
    }
    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked.clone()
    }
    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked = None;
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace(range, text, cx);
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let start = range
            .as_ref()
            .map_or(self.selection.start, |range| range.start);
        self.replace(range, text, cx);
        let length = text.encode_utf16().count();
        self.marked = (!text.is_empty()).then_some(start..start.saturating_add(length));
        if let Some(selected) = selected {
            self.selection =
                start.saturating_add(selected.start)..start.saturating_add(selected.end);
        }
    }
    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(bounds)
    }
    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.selection.end)
    }
}

struct ManagedTextInputRegistration(Entity<ManagedTextInput>);

impl IntoElement for ManagedTextInputRegistration {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ManagedTextInputRegistration {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }
    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }
    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.0.read(cx).focus.clone();
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.0.clone()), cx);
    }
}

impl Render for ManagedTextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus.clone();
        div()
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .cursor(gpui::CursorStyle::IBeam)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, move |_event, window, _cx| {
                focus.focus(window)
            })
            .child(if self.value.is_empty() {
                self.placeholder.clone()
            } else {
                self.value.clone()
            })
            .child(ManagedTextInputRegistration(cx.entity()))
    }
}

fn collect_text_nodes(
    node: &UiNode,
    managed: &ManagedDocument,
    output: &mut Vec<(
        (u64, UiPaneId, u64),
        String,
        String,
        brood_protocol::managed_ui::UiStateMode,
        ManagedDocument,
    )>,
) {
    let field = match &node.kind {
        UiNodeKind::TextField {
            value,
            placeholder,
            state_mode,
        }
        | UiNodeKind::SearchField {
            value,
            placeholder,
            state_mode,
        } => Some((value.clone(), placeholder.clone(), *state_mode)),
        UiNodeKind::MultilineEditor { value, state_mode } => {
            Some((value.clone(), String::new(), *state_mode))
        }
        _ => None,
    };
    if let Some((value, placeholder, mode)) = field {
        output.push((
            (managed.actor_generation, managed.document.pane, node.id),
            value,
            placeholder,
            mode,
            managed.clone(),
        ));
    }
    for child in &node.children {
        collect_text_nodes(child, managed, output);
    }
}

#[derive(Clone, Copy)]
struct CanvasPaintState {
    transform: brood_graphics::Transform,
    opacity: f32,
    clip: Option<brood_graphics::Rect>,
}

fn canvas_font(options: &brood_wasm::TextLayoutOptions, window: &Window) -> gpui::Font {
    let mut font = if options.font_intent == brood_wasm::FontIntent::Monospace {
        gpui::font("monospace")
    } else {
        window.text_style().font()
    };
    font.weight = match options.font_weight {
        brood_wasm::FontWeight::Normal => gpui::FontWeight::NORMAL,
        brood_wasm::FontWeight::Bold => gpui::FontWeight::BOLD,
    };
    font.style = match options.font_style {
        brood_wasm::FontStyle::Normal => gpui::FontStyle::Normal,
        brood_wasm::FontStyle::Italic => gpui::FontStyle::Italic,
    };
    font
}

fn measure_canvas_text(
    resource: u64,
    layout: &CanvasTextLayout,
    window: &Window,
) -> Result<brood_wasm::TextLayoutResult, brood_wasm::PresentationError> {
    let font = canvas_font(&layout.options, window);
    let run = gpui::TextRun {
        len: layout.text.len(),
        font: font.clone(),
        color: gpui::transparent_black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line_height = px(layout.options.line_height.max(1.0));
    let lines = window
        .text_system()
        .shape_text(
            layout.text.clone().into(),
            px(layout.options.font_size.max(1.0)),
            &[run],
            Some(px(layout.options.max_width.max(1.0))),
            None,
        )
        .map_err(|_error| brood_wasm::PresentationError::InvalidArgument)?;
    let width = lines
        .iter()
        .map(|line| f32::from(line.size(line_height).width))
        .fold(0.0_f32, f32::max);
    let line_count = lines
        .iter()
        .map(|line| line.wrap_boundaries().len().saturating_add(1))
        .sum::<usize>()
        .max(1);
    let font_id = window.text_system().resolve_font(&font);
    let baseline = window.text_system().baseline_offset(
        font_id,
        px(layout.options.font_size.max(1.0)),
        line_height,
    );
    let cell_width = if layout.options.font_intent == brood_wasm::FontIntent::Monospace {
        let cell_run = gpui::TextRun {
            len: 1,
            font,
            color: gpui::transparent_black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        f32::from(
            window
                .text_system()
                .shape_line(
                    "M".into(),
                    px(layout.options.font_size.max(1.0)),
                    &[cell_run],
                    None,
                )
                .width,
        )
    } else {
        0.0
    };
    Ok(brood_wasm::TextLayoutResult {
        handle: resource,
        width,
        height: layout.options.line_height.max(1.0) * line_count as f32,
        first_baseline: f32::from(baseline),
        line_count: u32::try_from(line_count).unwrap_or(u32::MAX),
        cell_width,
        line_height: layout.options.line_height.max(1.0),
    })
}

fn paint_canvas_packet(
    packet: &brood_graphics::Packet,
    bounds: gpui::Bounds<gpui::Pixels>,
    window: &mut Window,
    cx: &mut gpui::App,
    text: &BTreeMap<u64, CanvasTextLayout>,
    images: &BTreeMap<u64, Arc<gpui::RenderImage>>,
) {
    use brood_graphics::Command;
    let mut state = CanvasPaintState {
        transform: brood_graphics::Transform::IDENTITY,
        opacity: 1.0,
        clip: None,
    };
    let mut stack = Vec::new();
    for command in &packet.commands {
        match command {
            Command::Save => stack.push(state),
            Command::Restore => {
                if let Some(saved) = stack.pop() {
                    state = saved;
                }
            }
            Command::SetTransform(transform) => state.transform = *transform,
            Command::SetOpacity(opacity) => state.opacity = *opacity,
            Command::ClipRect(rect) => {
                state.clip = Some(intersect_rect(
                    state.clip,
                    transformed_rect(*rect, state.transform),
                ))
            }
            Command::FillRect { rect, color } => {
                paint_canvas_quad(window, bounds, state, *rect, 0.0, *color, 0.0)
            }
            Command::StrokeRect { rect, color, width } => paint_canvas_quad(
                window,
                bounds,
                state,
                *rect,
                0.0,
                *color,
                *width * axis_scale(state.transform),
            ),
            Command::FillRoundedRect {
                rect,
                radius,
                color,
            } => paint_canvas_quad(
                window,
                bounds,
                state,
                *rect,
                *radius * axis_scale(state.transform),
                *color,
                0.0,
            ),
            Command::StrokeRoundedRect {
                rect,
                radius,
                color,
                width,
            } => {
                let rect = canvas_bounds(bounds, transformed_rect(*rect, state.transform));
                with_canvas_clip(window, bounds, state.clip, |window| {
                    window.paint_quad(gpui::quad(
                        rect,
                        px(*radius * axis_scale(state.transform)),
                        gpui::transparent_black(),
                        px(*width * axis_scale(state.transform)),
                        canvas_color(*color, state.opacity),
                        Default::default(),
                    ))
                });
            }
            Command::FillPath { path, color } => {
                paint_canvas_path(window, bounds, state, path, *color, None)
            }
            Command::StrokePath { path, color, width } => paint_canvas_path(
                window,
                bounds,
                state,
                path,
                *color,
                Some(*width * axis_scale(state.transform)),
            ),
            Command::ClipPath(path) => {
                if let Some(path_bounds) = brood_path_bounds(path) {
                    state.clip = Some(intersect_rect(
                        state.clip,
                        transformed_rect(path_bounds, state.transform),
                    ));
                }
            }
            Command::DrawImage { image, destination } => {
                if let Some(image) = images.get(image) {
                    let image_bounds =
                        canvas_bounds(bounds, transformed_rect(*destination, state.transform));
                    with_canvas_clip(window, bounds, state.clip, |window| {
                        window.with_element_opacity(Some(state.opacity), |window| {
                            if window
                                .paint_image(
                                    image_bounds,
                                    Default::default(),
                                    Arc::clone(image),
                                    0,
                                    false,
                                )
                                .is_err()
                            {}
                        });
                    });
                }
            }
            Command::DrawTextLayout { layout, x, y } => {
                if let Some(layout) = text.get(layout) {
                    let (x, y) = transform_point(state.transform, *x, *y);
                    let color = layout.options.color;
                    let run = gpui::TextRun {
                        len: layout.text.len(),
                        font: canvas_font(&layout.options, window),
                        color: gpui::Rgba {
                            r: color[0],
                            g: color[1],
                            b: color[2],
                            a: color[3] * state.opacity,
                        }
                        .into(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let line = window.text_system().shape_line(
                        layout.text.clone().into(),
                        px(layout.options.font_size),
                        &[run],
                        Some(px(layout.options.max_width)),
                    );
                    with_canvas_clip(window, bounds, state.clip, |window| {
                        if line
                            .paint(
                                gpui::point(bounds.origin.x + px(x), bounds.origin.y + px(y)),
                                px(layout.options.line_height),
                                window,
                                cx,
                            )
                            .is_err()
                        {}
                    });
                }
            }
            Command::SetBlendMode(_) | Command::BeginLayer | Command::EndLayer => {}
        }
    }
}

fn paint_canvas_quad(
    window: &mut Window,
    canvas: gpui::Bounds<gpui::Pixels>,
    state: CanvasPaintState,
    rect: brood_graphics::Rect,
    radius: f32,
    color: brood_graphics::Color,
    border_width: f32,
) {
    let rect = canvas_bounds(canvas, transformed_rect(rect, state.transform));
    with_canvas_clip(window, canvas, state.clip, |window| {
        window.paint_quad(gpui::quad(
            rect,
            px(radius),
            if border_width == 0.0 {
                canvas_color(color, state.opacity)
            } else {
                gpui::Rgba::default()
            },
            px(border_width),
            canvas_color(color, state.opacity),
            Default::default(),
        ))
    });
}

fn paint_canvas_path(
    window: &mut Window,
    canvas: gpui::Bounds<gpui::Pixels>,
    state: CanvasPaintState,
    path: &brood_graphics::Path,
    color: brood_graphics::Color,
    stroke: Option<f32>,
) {
    let mut builder = stroke.map_or_else(gpui::PathBuilder::fill, |width| {
        gpui::PathBuilder::stroke(px(width))
    });
    for segment in &path.segments {
        use brood_graphics::PathSegment;
        match *segment {
            PathSegment::MoveTo(x, y) => {
                builder.move_to(canvas_point(canvas, state.transform, x, y))
            }
            PathSegment::LineTo(x, y) => {
                builder.line_to(canvas_point(canvas, state.transform, x, y))
            }
            PathSegment::QuadTo(cx, cy, x, y) => builder.curve_to(
                canvas_point(canvas, state.transform, x, y),
                canvas_point(canvas, state.transform, cx, cy),
            ),
            PathSegment::CubicTo(ax, ay, bx, by, x, y) => builder.cubic_bezier_to(
                canvas_point(canvas, state.transform, x, y),
                canvas_point(canvas, state.transform, ax, ay),
                canvas_point(canvas, state.transform, bx, by),
            ),
            PathSegment::Close => builder.close(),
        }
    }
    if let Ok(path) = builder.build() {
        with_canvas_clip(window, canvas, state.clip, |window| {
            window.paint_path(path, canvas_color(color, state.opacity))
        });
    }
}

fn canvas_point(
    canvas: gpui::Bounds<gpui::Pixels>,
    transform: brood_graphics::Transform,
    x: f32,
    y: f32,
) -> gpui::Point<gpui::Pixels> {
    let (x, y) = transform_point(transform, x, y);
    gpui::point(canvas.origin.x + px(x), canvas.origin.y + px(y))
}

fn transform_point(transform: brood_graphics::Transform, x: f32, y: f32) -> (f32, f32) {
    (
        transform.sx * x + transform.kx * y + transform.tx,
        transform.ky * x + transform.sy * y + transform.ty,
    )
}

fn axis_scale(transform: brood_graphics::Transform) -> f32 {
    transform
        .sx
        .mul_add(transform.sx, transform.ky * transform.ky)
        .sqrt()
}

fn canvas_packet_uses_resource(packet: &brood_graphics::Packet, resource: u64) -> bool {
    packet.commands.iter().any(|command| match command {
        brood_graphics::Command::DrawImage { image, .. } => *image == resource,
        brood_graphics::Command::DrawTextLayout { layout, .. } => *layout == resource,
        _ => false,
    })
}

fn transformed_rect(
    rect: brood_graphics::Rect,
    transform: brood_graphics::Transform,
) -> brood_graphics::Rect {
    let points = [
        transform_point(transform, rect.x, rect.y),
        transform_point(transform, rect.x + rect.width, rect.y),
        transform_point(transform, rect.x, rect.y + rect.height),
        transform_point(transform, rect.x + rect.width, rect.y + rect.height),
    ];
    let min_x = points
        .iter()
        .map(|point| point.0)
        .fold(f32::INFINITY, f32::min);
    let max_x = points
        .iter()
        .map(|point| point.0)
        .fold(f32::NEG_INFINITY, f32::max);
    let min_y = points
        .iter()
        .map(|point| point.1)
        .fold(f32::INFINITY, f32::min);
    let max_y = points
        .iter()
        .map(|point| point.1)
        .fold(f32::NEG_INFINITY, f32::max);
    brood_graphics::Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
}

fn canvas_bounds(
    canvas: gpui::Bounds<gpui::Pixels>,
    rect: brood_graphics::Rect,
) -> gpui::Bounds<gpui::Pixels> {
    gpui::Bounds::new(
        gpui::point(canvas.origin.x + px(rect.x), canvas.origin.y + px(rect.y)),
        gpui::size(px(rect.width), px(rect.height)),
    )
}

fn with_canvas_clip(
    window: &mut Window,
    canvas: gpui::Bounds<gpui::Pixels>,
    clip: Option<brood_graphics::Rect>,
    paint: impl FnOnce(&mut Window),
) {
    let mask = clip.map(|clip| gpui::ContentMask {
        bounds: canvas_bounds(canvas, clip),
    });
    window.with_content_mask(mask, paint);
}

fn intersect_rect(
    current: Option<brood_graphics::Rect>,
    next: brood_graphics::Rect,
) -> brood_graphics::Rect {
    let Some(current) = current else { return next };
    let x = current.x.max(next.x);
    let y = current.y.max(next.y);
    let right = (current.x + current.width).min(next.x + next.width);
    let bottom = (current.y + current.height).min(next.y + next.height);
    brood_graphics::Rect::new(x, y, (right - x).max(0.0), (bottom - y).max(0.0))
}

fn brood_path_bounds(path: &brood_graphics::Path) -> Option<brood_graphics::Rect> {
    let mut points = Vec::new();
    for segment in &path.segments {
        match *segment {
            brood_graphics::PathSegment::MoveTo(x, y)
            | brood_graphics::PathSegment::LineTo(x, y) => points.push((x, y)),
            brood_graphics::PathSegment::QuadTo(ax, ay, x, y) => points.extend([(ax, ay), (x, y)]),
            brood_graphics::PathSegment::CubicTo(ax, ay, bx, by, x, y) => {
                points.extend([(ax, ay), (bx, by), (x, y)])
            }
            brood_graphics::PathSegment::Close => {}
        }
    }
    let first = *points.first()?;
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (first.0, first.0, first.1, first.1);
    for (x, y) in points {
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }
    Some(brood_graphics::Rect::new(
        min_x,
        min_y,
        max_x - min_x,
        max_y - min_y,
    ))
}

fn canvas_color(color: brood_graphics::Color, opacity: f32) -> gpui::Rgba {
    gpui::Rgba {
        r: color.r,
        g: color.g,
        b: color.b,
        a: color.a * opacity,
    }
}

fn collect_focusable_nodes(
    node: &UiNode,
    generation: u64,
    pane: UiPaneId,
    output: &mut BTreeSet<(u64, UiPaneId, u64)>,
) {
    if is_activatable(&node.kind) || is_text_input(&node.kind) {
        output.insert((generation, pane, node.id));
    }
    for child in &node.children {
        collect_focusable_nodes(child, generation, pane, output);
    }
}

const fn is_text_input(kind: &UiNodeKind) -> bool {
    matches!(
        kind,
        UiNodeKind::TextField { .. }
            | UiNodeKind::SearchField { .. }
            | UiNodeKind::MultilineEditor { .. }
            | UiNodeKind::TextEditorBridge { .. }
    )
}

const fn is_activatable(kind: &UiNodeKind) -> bool {
    matches!(
        kind,
        UiNodeKind::Button { .. }
            | UiNodeKind::Menu { .. }
            | UiNodeKind::Link { .. }
            | UiNodeKind::CommandTarget { .. }
            | UiNodeKind::SelectableRow { .. }
            | UiNodeKind::Toggle { .. }
            | UiNodeKind::RadioGroup { .. }
    )
}

fn apply_style(
    mut element: gpui::Stateful<gpui::Div>,
    style: &brood_protocol::managed_ui::UiStyle,
) -> gpui::Stateful<gpui::Div> {
    if style.hidden {
        element = element.hidden();
    }
    if let Some(width) = style.width {
        element = element.w(px(width));
    }
    if let Some(height) = style.height {
        element = element.h(px(height));
    }
    if let Some(width) = style.min_width {
        element = element.min_w(px(width));
    }
    if let Some(height) = style.min_height {
        element = element.min_h(px(height));
    }
    if let Some(width) = style.max_width {
        element = element.max_w(px(width));
    }
    if let Some(height) = style.max_height {
        element = element.max_h(px(height));
    }
    if style.grow > 0.0 {
        element = element.flex_grow();
    }
    if style.shrink > 0.0 {
        element = element.flex_shrink();
    } else {
        element = element.flex_shrink_0();
    }
    element = element
        .gap(px(style.gap))
        .pt(px(style.padding.top))
        .pr(px(style.padding.right))
        .pb(px(style.padding.bottom))
        .pl(px(style.padding.left))
        .rounded(px(style.radius));
    element = match style.align {
        UiAlignment::Start => element.items_start(),
        UiAlignment::Center => element.items_center(),
        UiAlignment::End => element.items_end(),
        UiAlignment::Stretch => element,
        UiAlignment::SpaceBetween => element.justify_between(),
    };
    if let Some(color) = style.background {
        element = element.bg(theme_color(color));
    }
    if let Some(color) = style.foreground {
        element = element.text_color(theme_color(color));
    }
    element
}

fn theme_color(token: UiColorToken) -> gpui::Rgba {
    match token {
        UiColorToken::Foreground => rgb(0x00e6_e8ee),
        UiColorToken::MutedForeground => rgb(0x0092_98a8),
        UiColorToken::Background => rgb(0x0011_1318),
        UiColorToken::Surface => rgb(0x001b_1f27),
        UiColorToken::Border => rgb(0x0034_3b49),
        UiColorToken::Accent => rgb(0x0067_a8ff),
        UiColorToken::Danger => rgb(0x00ff_6b72),
        UiColorToken::Success => rgb(0x0065_c98b),
        UiColorToken::Warning => rgb(0x00e6_b85c),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "test fixtures fail immediately when their setup is invalid"
)]
mod tests {
    use super::*;
    use brood_protocol::managed_ui::{
        MANAGED_UI_PROTOCOL_VERSION, UiAccessibility, UiNodeKind, UiStyle,
    };

    fn document(revision: u64) -> UiDocument {
        UiDocument {
            protocol_version: MANAGED_UI_PROTOCOL_VERSION,
            pane: 3,
            document_revision: revision,
            root: UiNode {
                id: 1,
                kind: UiNodeKind::Column,
                style: UiStyle::default(),
                accessibility: UiAccessibility::default(),
                optional: false,
                interaction_revision: 0,
                children: Vec::new(),
            },
        }
    }

    #[test]
    fn stale_snapshot_keeps_last_complete_tree() {
        let mut store = DocumentStore::new(UiLimits::default());
        store
            .publish(5, UiPublication::Snapshot(Box::new(document(2))))
            .expect("valid snapshot");
        assert!(
            store
                .publish(5, UiPublication::Snapshot(Box::new(document(1))))
                .is_err()
        );
        assert_eq!(
            store.document(3).map(|document| document.document_revision),
            Some(2)
        );
        assert_eq!(store.diagnostics().count(), 1);
    }

    #[test]
    fn replacement_generation_can_publish_revision_one() {
        let mut store = DocumentStore::new(UiLimits::default());
        store
            .publish(5, UiPublication::Snapshot(Box::new(document(8))))
            .expect("valid snapshot");
        store
            .publish(6, UiPublication::Snapshot(Box::new(document(1))))
            .expect("replacement generation");
        assert_eq!(
            store
                .documents
                .get(&3)
                .map(AcceptedUiDocument::actor_generation),
            Some(6)
        );
    }

    #[test]
    fn canvas_exactness_rejects_operations_gpui_cannot_preserve() {
        use brood_graphics::{BlendMode, Color, Command, Packet, Rect, Transform};
        let mut packet = Packet::new();
        packet.push(Command::SetTransform(Transform {
            sx: 0.0,
            ky: 1.0,
            kx: -1.0,
            sy: 0.0,
            tx: 10.0,
            ty: 0.0,
        }));
        packet.push(Command::FillRect {
            rect: Rect::new(0.0, 0.0, 4.0, 4.0),
            color: Color::rgba(1.0, 1.0, 1.0, 1.0),
        });
        assert!(!canvas_packet_is_exact(&packet));

        let mut packet = Packet::new();
        packet.push(Command::SetBlendMode(BlendMode::Multiply));
        assert!(!canvas_packet_is_exact(&packet));

        let mut packet = Packet::new();
        packet.push(Command::Save);
        assert!(!canvas_packet_is_exact(&packet));
    }

    #[test]
    fn canvas_exactness_accepts_supported_affine_paths() {
        use brood_graphics::{Color, Command, Packet, Path, PathSegment, Transform};
        let mut packet = Packet::new();
        packet.push(Command::SetTransform(Transform {
            sx: 0.0,
            ky: 2.0,
            kx: -2.0,
            sy: 0.0,
            tx: 20.0,
            ty: 2.0,
        }));
        packet.push(Command::StrokePath {
            path: Path {
                segments: vec![PathSegment::MoveTo(0.0, 0.0), PathSegment::LineTo(3.0, 3.0)],
            },
            color: Color::rgba(1.0, 1.0, 1.0, 1.0),
            width: 1.0,
        });
        assert!(canvas_packet_is_exact(&packet));
    }
}
