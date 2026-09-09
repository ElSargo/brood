use anyhow::{Context, Result, ensure};
use brood_protocol::facets::{
    PaneContent, PaneOutput, PaneTarget, RenderResponse, RenderToken, Scope,
};
use brood_wasm::{
    facet_frames::FacetFrames,
    snapshot_budget::{ByteBudget, ByteBudgetLimits},
    snapshot_reclaimer::{
        BackgroundSnapshotReclaimer, ManagedSnapshotStore, SnapshotReclaimerConfig,
    },
    snapshot_store::{SnapshotLease, SnapshotStoreLimits},
};
use std::time::Duration;

const GENERATION: u64 = 7;

struct Harness {
    _reclaimer: BackgroundSnapshotReclaimer,
    snapshots: ManagedSnapshotStore,
    frames: FacetFrames,
    budget: ByteBudget,
}

fn harness(byte_limit: usize) -> Result<Harness> {
    let reclaimer = BackgroundSnapshotReclaimer::new(SnapshotReclaimerConfig {
        max_generations: 2,
        poll_interval: Duration::from_millis(1),
    })?;
    let snapshots = reclaimer.register(
        GENERATION,
        SnapshotStoreLimits {
            max_live_bytes: 1024,
            max_live_allocations: 8,
            max_live_roots: 4,
            max_snapshot_bytes: 512,
            max_chunks_per_snapshot: 4,
        },
    )?;
    let budget = ByteBudget::new(
        GENERATION,
        ByteBudgetLimits {
            max_live_bytes: byte_limit,
            max_live_allocations: 8,
            max_buffer_bytes: byte_limit,
        },
    );
    let frames = FacetFrames::new(
        scope(),
        budget.clone(),
        2,
        brood_protocol::facets::InvocationLimits {
            max_output_bytes: 1024,
            max_packet_bytes: 512,
            max_interaction_bytes: 512,
            max_sequence_bytes: 64,
            max_events: 8,
            max_event_bytes: 64,
            max_host_requests: 4,
            max_resources: 16,
            max_resource_bytes: 1024,
        },
    )?;
    Ok(Harness {
        _reclaimer: reclaimer,
        snapshots,
        frames,
        budget,
    })
}

#[test]
fn pinned_replay_frame_stays_charged_after_generation_invalidation_until_drop() -> Result<()> {
    let mut h = harness(64)?;
    commit_draw(&mut h, 1, b"budgeted-packet", b"budgeted-hit-map")?;
    let replay = h.frames.pin_input(1)?;
    let charged = h.budget.retention().live_bytes;
    ensure!(charged >= b"budgeted-packet".len() + b"budgeted-hit-map".len());

    h.frames.invalidate();
    ensure!(!h.budget.is_active());
    ensure!(h.budget.retention().live_bytes == charged);
    drop(replay);
    ensure!(h.budget.retention().live_bytes == 0);
    Ok(())
}

const fn scope() -> Scope {
    Scope {
        client_generation: 3,
        actor_generation: GENERATION,
        facet_generation: 11,
    }
}
const fn token(revision: u64) -> RenderToken {
    RenderToken {
        scope: scope(),
        invocation: 999,
        snapshot_revision: revision,
        candidate: revision,
        pane_membership_revision: 1,
        geometry_revision: 1,
        host_configuration_revision: 1,
        renderer_generation: 1,
        interaction_revision: 1,
    }
}
const fn target(pane: u64) -> PaneTarget {
    PaneTarget {
        pane,
        logical_width: 100.0,
        logical_height: 50.0,
        physical_width: 200,
        physical_height: 100,
        scale: 2.0,
        clip: [0.0, 0.0, 100.0, 50.0],
        visible: true,
        focused: false,
    }
}
fn snapshot(store: &ManagedSnapshotStore, revision: u64) -> Result<SnapshotLease> {
    store.publish_copy_of(revision, &[u8::try_from(revision)?])?;
    store.acquire_latest()?.context("snapshot missing")
}
fn draw(token: RenderToken, pane: u64, packet: &[u8], interaction: &[u8]) -> RenderResponse {
    RenderResponse {
        token,
        panes: vec![PaneOutput {
            pane,
            content: PaneContent::Draw {
                packet: packet.to_vec(),
                interaction: interaction.to_vec(),
            },
        }],
        next_frame_ns: None,
    }
}
fn commit_draw(h: &mut Harness, revision: u64, packet: &[u8], interaction: &[u8]) -> Result<()> {
    let lease = snapshot(&h.snapshots, revision)?;
    let issued = h.frames.issue(token(revision), vec![target(1)], lease)?;
    let prepared = h.frames.prepare(&draw(issued, 1, packet, interaction))?;
    h.frames.commit(prepared)?;
    Ok(())
}

#[test]
fn pending_never_routes_input_and_commit_pins_pair() -> Result<()> {
    let mut h = harness(64)?;
    let lease = snapshot(&h.snapshots, 1)?;
    let issued = h.frames.issue(token(1), vec![target(1)], lease)?;
    ensure!(h.frames.pin_input(1).is_err());
    let prepared = h.frames.prepare(&draw(issued, 1, b"packet", b"hit"))?;
    h.frames.commit(prepared)?;
    let pinned = h.frames.pin_input(1)?;
    ensure!(pinned.packet()? == b"packet" && pinned.interaction()? == b"hit");
    ensure!(pinned.snapshot().revision() == 1);
    Ok(())
}

#[test]
fn queued_old_pair_survives_new_commit() -> Result<()> {
    let mut h = harness(64)?;
    commit_draw(&mut h, 1, b"old", b"old-hit")?;
    let queued = h.frames.pin_input(1)?;
    commit_draw(&mut h, 2, b"new", b"new-hit")?;
    ensure!(queued.packet()? == b"old" && queued.interaction()? == b"old-hit");
    ensure!(queued.snapshot().revision() == 1);
    ensure!(h.frames.pin_input(1)?.packet()? == b"new");
    Ok(())
}

#[test]
fn no_change_preserves_application_and_interaction_but_rejects_dependency_changes() -> Result<()> {
    let mut h = harness(128)?;
    commit_draw(&mut h, 1, b"old", b"hit-map")?;
    let lease = snapshot(&h.snapshots, 2)?;
    let issued = h.frames.issue(token(2), vec![target(1)], lease)?;
    let response = RenderResponse {
        token: issued,
        panes: vec![PaneOutput {
            pane: 1,
            content: PaneContent::NoChange,
        }],
        next_frame_ns: None,
    };
    h.frames.commit(h.frames.prepare(&response)?)?;
    let retained = h.frames.pin_input(1)?;
    ensure!(retained.snapshot().revision() == 1 && retained.interaction()? == b"hit-map");

    for change in 0..5 {
        let lease = h.snapshots.acquire_latest()?.context("snapshot missing")?;
        let mut changed = token(2);
        let mut pane = target(1);
        match change {
            0 => {
                pane.logical_width += 1.0;
                changed.geometry_revision += 1;
            }
            1 => changed.host_configuration_revision += 1,
            2 => changed.renderer_generation += 1,
            3 => changed.interaction_revision += 1,
            4 => changed.geometry_revision += 1,
            _ => return Err(anyhow::anyhow!("invalid dependency case")),
        }
        let issued = h.frames.issue(changed, vec![pane], lease)?;
        let response = RenderResponse {
            token: issued,
            panes: vec![PaneOutput {
                pane: 1,
                content: PaneContent::NoChange,
            }],
            next_frame_ns: None,
        };
        ensure!(
            h.frames.prepare(&response).is_err(),
            "dependency change {change} admitted no-change"
        );
    }
    Ok(())
}

#[test]
fn superseded_prepared_cannot_commit() -> Result<()> {
    let mut h = harness(64)?;
    let first = snapshot(&h.snapshots, 1)?;
    let issued = h.frames.issue(token(1), vec![target(1)], first)?;
    let prepared = h.frames.prepare(&draw(issued, 1, b"a", b"b"))?;
    let second = snapshot(&h.snapshots, 2)?;
    h.frames.issue(token(2), vec![target(1)], second)?;
    ensure!(h.frames.commit(prepared).is_err());
    ensure!(h.frames.pin_input(1).is_err());
    Ok(())
}

#[test]
fn malformed_multi_pane_response_rejects_atomically() -> Result<()> {
    let mut h = harness(128)?;
    commit_draw(&mut h, 1, b"prior", b"prior-hit")?;
    for panes in [
        vec![PaneOutput {
            pane: 1,
            content: PaneContent::Draw {
                packet: vec![1],
                interaction: vec![2],
            },
        }],
        vec![
            PaneOutput {
                pane: 1,
                content: PaneContent::NoChange,
            },
            PaneOutput {
                pane: 1,
                content: PaneContent::NoChange,
            },
        ],
        vec![
            PaneOutput {
                pane: 1,
                content: PaneContent::NoChange,
            },
            PaneOutput {
                pane: 9,
                content: PaneContent::NoChange,
            },
        ],
    ] {
        let lease = h.snapshots.acquire_latest()?.context("snapshot missing")?;
        let issued = h
            .frames
            .issue(token(1), vec![target(1), target(2)], lease)?;
        ensure!(
            h.frames
                .prepare(&RenderResponse {
                    token: issued,
                    panes,
                    next_frame_ns: None
                })
                .is_err()
        );
        ensure!(h.frames.pin_input(1)?.packet()? == b"prior");
    }
    Ok(())
}

#[test]
fn late_native_discard_preserves_newer_pending_and_committed_input() -> Result<()> {
    let mut h = harness(4096)?;
    commit_draw(&mut h, 1, b"visible", b"input")?;
    let lease = snapshot(&h.snapshots, 2)?;
    let old = h.frames.issue(token(2), vec![target(1)], lease)?;
    let lease = snapshot(&h.snapshots, 3)?;
    let current = h.frames.issue(token(3), vec![target(1)], lease)?;
    h.frames.discard_invocation(old.invocation);
    ensure!(h.frames.has_pending());
    let prepared = h
        .frames
        .prepare(&draw(current.clone(), 1, b"next", b"next-input"))?;
    h.frames.validate_commit(&prepared)?;
    ensure!(h.frames.pin_input(1)?.packet()? == b"visible");
    h.frames.discard_invocation(current.invocation);
    ensure!(!h.frames.has_pending());
    ensure!(h.frames.validate_commit(&prepared).is_err());
    ensure!(h.frames.pin_input(1)?.packet()? == b"visible");
    Ok(())
}

#[test]
fn byte_exhaustion_preserves_prior_commit_and_invalidation_refuses_input() -> Result<()> {
    let mut h = harness(8)?;
    commit_draw(&mut h, 1, b"aa", b"bb")?;
    let prior = h.frames.pin_input(1)?;
    let lease = snapshot(&h.snapshots, 2)?;
    let issued = h.frames.issue(token(2), vec![target(1)], lease)?;
    ensure!(h.frames.prepare(&draw(issued, 1, b"ccc", b"ddd")).is_err());
    ensure!(h.frames.pin_input(1)?.packet()? == b"aa");
    h.frames.invalidate();
    ensure!(h.frames.pin_input(1).is_err());
    ensure!(prior.packet().is_err());
    Ok(())
}
