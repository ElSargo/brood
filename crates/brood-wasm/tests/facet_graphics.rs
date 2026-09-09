use anyhow::{Context, Result, ensure};
use brood_graphics::{Color, Command, Packet, Rect};
use brood_protocol::facets::{
    InvocationLimits, PaneContent, PaneOutput, PaneTarget, RenderResponse, RenderToken, Scope,
};
use brood_wasm::{
    facet_frames::FacetFrames,
    facet_graphics::ValidatedFacetGraphics,
    facet_resources::{FacetResourceBackend, FacetResourceRegistry, FacetResourceRegistryConfig},
    presentation::{PresentationError, TextLayoutOptions, TextLayoutResult},
    snapshot_budget::{ByteBudget, ByteBudgetLimits},
    snapshot_reclaimer::{
        BackgroundSnapshotReclaimer, ManagedSnapshotStore, SnapshotReclaimerConfig,
    },
    snapshot_store::{SnapshotLease, SnapshotStoreLimits},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

#[derive(Debug, Default)]
struct Backend {
    next: Mutex<u64>,
    live: Mutex<BTreeMap<u64, usize>>,
}

fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
impl FacetResourceBackend for Backend {
    fn create_image(
        &self,
        _pane: u64,
        _w: u32,
        _h: u32,
        pixels: &[u8],
        max: usize,
    ) -> Result<(u64, usize), PresentationError> {
        if pixels.len() > max {
            return Err(PresentationError::LimitReached);
        }
        let mut n = guard(&self.next);
        *n += 1;
        let handle = *n;
        drop(n);
        guard(&self.live).insert(handle, pixels.len());
        Ok((handle, pixels.len()))
    }
    fn create_text(
        &self,
        _: u64,
        text: &str,
        _: TextLayoutOptions,
        max: usize,
    ) -> Result<(u64, TextLayoutResult, usize), PresentationError> {
        if text.len() > max {
            return Err(PresentationError::LimitReached);
        }
        let mut n = guard(&self.next);
        *n += 1;
        let handle = *n;
        drop(n);
        guard(&self.live).insert(handle, text.len());
        Ok((
            handle,
            TextLayoutResult {
                handle,
                width: 1.0,
                height: 1.0,
                first_baseline: 1.0,
                line_count: 1,
                cell_width: 1.0,
                line_height: 1.0,
            },
            text.len(),
        ))
    }
    fn release(&self, _: u64, h: u64) -> Result<(), PresentationError> {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&h)
            .map(|_| ())
            .ok_or(PresentationError::InvalidResource)
    }
}
struct H {
    _r: BackgroundSnapshotReclaimer,
    store: ManagedSnapshotStore,
    frames: FacetFrames,
    registry: FacetResourceRegistry,
    budget: ByteBudget,
}
const fn scope() -> Scope {
    Scope {
        client_generation: 1,
        actor_generation: 2,
        facet_generation: 3,
    }
}
const fn token() -> RenderToken {
    RenderToken {
        scope: scope(),
        invocation: 0,
        snapshot_revision: 1,
        candidate: 1,
        pane_membership_revision: 1,
        geometry_revision: 1,
        host_configuration_revision: 1,
        renderer_generation: 1,
        interaction_revision: 1,
    }
}
const fn target() -> PaneTarget {
    PaneTarget {
        pane: 7,
        logical_width: 10.0,
        logical_height: 10.0,
        physical_width: 10,
        physical_height: 10,
        scale: 1.0,
        clip: [0.0, 0.0, 10.0, 10.0],
        visible: true,
        focused: true,
    }
}
const fn limits() -> InvocationLimits {
    InvocationLimits {
        max_output_bytes: 1024,
        max_packet_bytes: 1024,
        max_interaction_bytes: 1024,
        max_sequence_bytes: 1024,
        max_events: 4,
        max_event_bytes: 1024,
        max_host_requests: 4,
        max_resources: 4,
        max_resource_bytes: 1024,
    }
}
fn setup() -> Result<H> {
    let r = BackgroundSnapshotReclaimer::new(SnapshotReclaimerConfig {
        max_generations: 2,
        poll_interval: Duration::from_millis(1),
    })?;
    let store = r.register(
        2,
        SnapshotStoreLimits {
            max_live_bytes: 1024,
            max_live_allocations: 8,
            max_live_roots: 4,
            max_snapshot_bytes: 512,
            max_chunks_per_snapshot: 4,
        },
    )?;
    let b = ByteBudget::new(
        2,
        ByteBudgetLimits {
            max_live_bytes: 8192,
            max_live_allocations: 64,
            max_buffer_bytes: 4096,
        },
    );
    let frames = FacetFrames::new(scope(), b.clone(), 1, limits())?;
    let registry = FacetResourceRegistry::new(
        Arc::new(Backend::default()),
        FacetResourceRegistryConfig {
            max_invocations: 4,
            retry_interval: Duration::from_millis(1),
        },
    )?;
    Ok(H {
        _r: r,
        store,
        frames,
        registry,
        budget: b,
    })
}
fn snapshot(s: &ManagedSnapshotStore) -> Result<SnapshotLease> {
    s.publish_copy_of(1, &[1])?;
    s.acquire_latest()?.context("snapshot")
}
fn response(t: RenderToken, payload: Vec<u8>) -> RenderResponse {
    RenderResponse {
        token: t,
        panes: vec![PaneOutput {
            pane: 7,
            content: PaneContent::Draw {
                packet: payload,
                interaction: vec![],
            },
        }],
        next_frame_ns: None,
    }
}
fn fill_packet() -> Result<Vec<u8>, brood_graphics::PacketError> {
    let mut p = Packet::new();
    p.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, 1.0, 1.0),
        color: Color::rgba(1.0, 0.0, 0.0, 1.0),
    });
    p.encode()
}

#[test]
fn invalid_packet_and_missing_lease_are_rejected() -> Result<()> {
    let mut h = setup()?;
    let issued = h
        .frames
        .issue(token(), vec![target()], snapshot(&h.store)?)?;
    ensure!(
        ValidatedFacetGraphics::validate(
            &h.frames,
            &response(issued.clone(), vec![0]),
            None,
            &h.budget
        )
        .is_err()
    );
    let mut p = Packet::new();
    p.push(Command::DrawImage {
        image: 1,
        destination: Rect::new(0.0, 0.0, 1.0, 1.0),
    });
    ensure!(
        ValidatedFacetGraphics::validate(
            &h.frames,
            &response(issued, p.encode()?),
            None,
            &h.budget
        )
        .is_err()
    );
    Ok(())
}
#[test]
fn current_lease_accepts_and_foreign_lease_rejects() -> Result<()> {
    let mut h = setup()?;
    let issued = h
        .frames
        .issue(token(), vec![target()], snapshot(&h.store)?)?;
    let lease = h
        .registry
        .begin(issued.clone(), &[7], limits(), h.budget.clone())?;
    let handle = lease.create_image(7, 1, 1, &[1, 2, 3, 4])?;
    lease.seal()?;
    let mut p = Packet::new();
    p.push(Command::DrawTextLayout {
        layout: handle,
        x: 0.0,
        y: 0.0,
    });
    ensure!(
        ValidatedFacetGraphics::validate(
            &h.frames,
            &response(issued.clone(), p.encode()?),
            Some(Arc::clone(&lease)),
            &h.budget,
        )
        .is_err()
    );
    let mut p = Packet::new();
    p.push(Command::DrawImage {
        image: handle,
        destination: Rect::new(0.0, 0.0, 1.0, 1.0),
    });
    let valid = ValidatedFacetGraphics::validate(
        &h.frames,
        &response(issued.clone(), p.encode()?),
        Some(Arc::clone(&lease)),
        &h.budget,
    )?;
    let foreign = h.registry.begin(
        RenderToken {
            invocation: 99,
            ..issued.clone()
        },
        &[7],
        limits(),
        h.budget.clone(),
    )?;
    ensure!(
        ValidatedFacetGraphics::validate(
            &h.frames,
            &response(issued, fill_packet()?),
            Some(foreign),
            &h.budget
        )
        .is_err()
    );
    valid.commit(&mut h.frames)?;
    Ok(())
}

#[test]
fn committed_frame_keeps_resource_lease_until_frame_retirement() -> Result<()> {
    let mut h = setup()?;
    let issued = h
        .frames
        .issue(token(), vec![target()], snapshot(&h.store)?)?;
    let lease = h
        .registry
        .begin(issued.clone(), &[7], limits(), h.budget.clone())?;
    let handle = lease.create_image(7, 1, 1, &[1, 2, 3, 4])?;
    lease.seal()?;
    let mut packet = Packet::new();
    packet.push(Command::DrawImage {
        image: handle,
        destination: Rect::new(0.0, 0.0, 1.0, 1.0),
    });
    let validated = ValidatedFacetGraphics::validate(
        &h.frames,
        &response(issued, packet.encode()?),
        Some(Arc::clone(&lease)),
        &h.budget,
    )?;
    validated.commit(&mut h.frames)?;
    drop(lease);
    ensure!(h.registry.active_invocations() == 1);
    let pinned = h.frames.pin_input(7)?;
    drop(pinned);
    h.frames.invalidate();
    for _ in 0..100 {
        if h.registry.active_invocations() == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    ensure!(h.registry.active_invocations() == 0);
    Ok(())
}
