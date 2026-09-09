use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use brood_graphics::{Color, Command, Packet, Rect};
use brood_protocol::facets::{
    InvocationLimits, PaneContent, PaneOutput, PaneTarget, RenderResponse, RenderToken, Scope,
};
use brood_wasm::{
    facet_frames::FacetFrames,
    facet_graphics::ValidatedFacetGraphics,
    snapshot_budget::{ByteBudget, ByteBudgetLimits},
    snapshot_reclaimer::{
        BackgroundSnapshotReclaimer, ManagedSnapshotStore, SnapshotReclaimerConfig,
    },
    snapshot_store::{SnapshotLease, SnapshotStoreLimits},
};

use super::{PreparedFacetRender, PreparedFacetRenderInner};

struct Harness {
    _reclaimer: BackgroundSnapshotReclaimer,
    store: ManagedSnapshotStore,
    frames: Arc<Mutex<FacetFrames>>,
    budget: ByteBudget,
}

const fn scope() -> Scope {
    Scope {
        client_generation: 1,
        actor_generation: 2,
        facet_generation: 3,
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

fn token(revision: u64) -> RenderToken {
    RenderToken {
        scope: scope(),
        invocation: 0,
        snapshot_revision: revision,
        candidate: revision,
        pane_membership_revision: 1,
        geometry_revision: 1,
        host_configuration_revision: 1,
        renderer_generation: 1,
        interaction_revision: revision,
    }
}

fn setup() -> Result<Harness> {
    let reclaimer = BackgroundSnapshotReclaimer::new(SnapshotReclaimerConfig {
        max_generations: 2,
        poll_interval: Duration::from_millis(1),
    })?;
    let store = reclaimer.register(
        2,
        SnapshotStoreLimits {
            max_live_bytes: 4096,
            max_live_allocations: 8,
            max_live_roots: 4,
            max_snapshot_bytes: 512,
            max_chunks_per_snapshot: 4,
        },
    )?;
    let budget = ByteBudget::new(
        2,
        ByteBudgetLimits {
            max_live_bytes: 8192,
            max_live_allocations: 64,
            max_buffer_bytes: 4096,
        },
    );
    let frames = FacetFrames::new(scope(), budget.clone(), 1, limits())?;
    Ok(Harness {
        _reclaimer: reclaimer,
        store,
        frames: Arc::new(Mutex::new(frames)),
        budget,
    })
}

fn snapshot(store: &ManagedSnapshotStore, revision: u64) -> Result<SnapshotLease> {
    store.publish_copy_of(revision, &[u8::try_from(revision)?])?;
    store.acquire_latest()?.context("snapshot missing")
}

fn packet() -> Result<Vec<u8>> {
    let mut packet = Packet::new();
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, 1.0, 1.0),
        color: Color::rgba(1.0, 0.0, 0.0, 1.0),
    });
    Ok(packet.encode()?)
}

fn receipt(h: &Harness, revision: u64, interaction: &[u8]) -> Result<PreparedFacetRender> {
    let issued = h
        .frames
        .lock()
        .map_err(|poison| anyhow::anyhow!("frames poisoned: {poison}"))?
        .issue(
            token(revision),
            vec![target()],
            snapshot(&h.store, revision)?,
        )?;
    let response = RenderResponse {
        token: issued,
        panes: vec![PaneOutput {
            pane: 7,
            content: PaneContent::Draw {
                packet: packet()?,
                interaction: interaction.to_vec(),
            },
        }],
        next_frame_ns: None,
    };
    let frames = h
        .frames
        .lock()
        .map_err(|poison| anyhow::anyhow!("frames poisoned: {poison}"))?;
    let validated = ValidatedFacetGraphics::validate(&frames, &response, None, &h.budget)?;
    drop(frames);
    Ok(PreparedFacetRender {
        inner: Arc::new(PreparedFacetRenderInner {
            validated: Mutex::new(Some(validated)),
            frames: Arc::clone(&h.frames),
            discard_wake: None,
        }),
    })
}

#[test]
fn receipt_commits_once_and_retains_its_owned_pair() -> Result<()> {
    let h = setup()?;
    let receipt = receipt(&h, 1, b"interaction")?;
    ensure!(receipt.with_graphics(|graphics| graphics.panes().count())? == 1);
    ensure!(
        h.frames
            .lock()
            .map_err(|poison| anyhow::anyhow!("frames poisoned: {poison}"))?
            .pin_input(7)
            .is_err()
    );
    let retained = receipt.clone();
    ensure!(retained.commit()?.is_none());
    ensure!(receipt.commit().is_err());
    let frame = h
        .frames
        .lock()
        .map_err(|poison| anyhow::anyhow!("frames poisoned: {poison}"))?
        .pin_input(7)?;
    ensure!(frame.interaction()? == b"interaction");
    Ok(())
}

#[test]
fn discarded_speculative_receipt_does_not_replace_visible_pair() -> Result<()> {
    let h = setup()?;
    receipt(&h, 1, b"visible")?.commit()?;
    let speculative = receipt(&h, 2, b"speculative")?;
    drop(speculative);
    let frame = h
        .frames
        .lock()
        .map_err(|poison| anyhow::anyhow!("frames poisoned: {poison}"))?
        .pin_input(7)?;
    ensure!(frame.interaction()? == b"visible");
    Ok(())
}

#[test]
fn geometry_discard_invalidates_receipt_but_preserves_old_input_authority() -> Result<()> {
    let h = setup()?;
    receipt(&h, 1, b"old")?.commit()?;
    let pending = receipt(&h, 2, b"new")?;
    h.frames
        .lock()
        .map_err(|poison| anyhow::anyhow!("frames poisoned: {poison}"))?
        .discard_pending();
    ensure!(pending.commit().is_err());
    let frame = h
        .frames
        .lock()
        .map_err(|poison| anyhow::anyhow!("frames poisoned: {poison}"))?
        .pin_input(7)?;
    ensure!(frame.interaction()? == b"old");
    Ok(())
}

#[test]
fn only_last_uncommitted_receipt_wakes_replacement_demand() -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let h = setup()?;
    let wakes = Arc::new(AtomicUsize::new(0));
    let mut pending = receipt(&h, 1, b"discard")?;
    let counter = Arc::clone(&wakes);
    Arc::get_mut(&mut pending.inner)
        .context("unexpected shared receipt")?
        .discard_wake = Some(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    let retained = pending.clone();
    drop(pending);
    ensure!(wakes.load(Ordering::SeqCst) == 0);
    drop(retained);
    ensure!(wakes.load(Ordering::SeqCst) == 1);

    let mut accepted = receipt(&h, 2, b"commit")?;
    let counter = Arc::clone(&wakes);
    Arc::get_mut(&mut accepted.inner)
        .context("unexpected shared receipt")?
        .discard_wake = Some(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    accepted.commit()?;
    drop(accepted);
    ensure!(wakes.load(Ordering::SeqCst) == 1);
    Ok(())
}
