use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use brood_protocol::facets::{
    HostRequest, InputEvent, InputResponse, InvocationLimits, PaneContent, PaneOutput, PaneTarget,
    RenderResponse, RenderToken, Scope,
};
use brood_wasm::{
    facet_frames::FacetFrames,
    facet_input::{FacetInput, FacetInputConfig},
    snapshot_budget::{ByteBudget, ByteBudgetLimits},
    snapshot_reclaimer::{
        BackgroundSnapshotReclaimer, ManagedSnapshotStore, SnapshotReclaimerConfig,
    },
    snapshot_store::{SnapshotLease, SnapshotStoreLimits},
};

const GENERATION: u64 = 7;

struct Harness {
    _reclaimer: BackgroundSnapshotReclaimer,
    snapshots: ManagedSnapshotStore,
    frames: FacetFrames,
    input: FacetInput,
    budget: ByteBudget,
}

const fn scope() -> Scope {
    Scope {
        client_generation: 3,
        actor_generation: GENERATION,
        facet_generation: 11,
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

const fn limits() -> InvocationLimits {
    InvocationLimits {
        max_output_bytes: 1024,
        max_packet_bytes: 1024,
        max_interaction_bytes: 1024,
        max_sequence_bytes: 1024,
        max_events: 4,
        max_event_bytes: 1024,
        max_host_requests: 4,
        max_resources: 16,
        max_resource_bytes: 1024,
    }
}

const fn render_token(revision: u64) -> RenderToken {
    RenderToken {
        scope: scope(),
        invocation: 0,
        snapshot_revision: revision,
        candidate: revision,
        pane_membership_revision: 1,
        geometry_revision: 1,
        host_configuration_revision: 1,
        renderer_generation: 1,
        interaction_revision: 1,
    }
}

fn snapshot(store: &ManagedSnapshotStore, revision: u64) -> Result<SnapshotLease> {
    store.publish_copy_of(revision, &[u8::try_from(revision)?])?;
    store.acquire_latest()?.context("snapshot missing")
}

fn harness(config: FacetInputConfig) -> Result<Harness> {
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
            max_live_bytes: 4096,
            max_live_allocations: 64,
            max_buffer_bytes: 4096,
        },
    );
    let mut frames = FacetFrames::new(scope(), budget.clone(), 1, limits())?;
    let lease = snapshot(&snapshots, 1)?;
    let issued = frames.issue(render_token(1), vec![target(1)], lease)?;
    let response = RenderResponse {
        token: issued,
        panes: vec![PaneOutput {
            pane: 1,
            content: PaneContent::Draw {
                packet: b"packet".to_vec(),
                interaction: b"hit".to_vec(),
            },
        }],
        next_frame_ns: None,
    };
    frames.commit(frames.prepare(&response)?)?;
    let input = FacetInput::new(scope(), budget.clone(), config)?;
    Ok(Harness {
        _reclaimer: reclaimer,
        snapshots,
        frames,
        input,
        budget,
    })
}

fn response(
    request: &brood_protocol::facets::InputRequest,
    state: &[u8],
    event: &[u8],
) -> InputResponse {
    InputResponse {
        token: request.token.clone(),
        next_sequence_state: state.to_vec(),
        events: vec![event.to_vec()],
        requests: vec![HostRequest::Render],
    }
}

fn commit_new_frame(h: &mut Harness) -> Result<Arc<brood_wasm::facet_frames::PaneFrame>> {
    let lease = snapshot(&h.snapshots, 2)?;
    let issued = h.frames.issue(render_token(2), vec![target(1)], lease)?;
    let response = RenderResponse {
        token: issued,
        panes: vec![PaneOutput {
            pane: 1,
            content: PaneContent::Draw {
                packet: b"new-packet".to_vec(),
                interaction: b"new-hit".to_vec(),
            },
        }],
        next_frame_ns: None,
    };
    let prepared = h.frames.prepare(&response)?;
    h.frames.commit(prepared)?;
    h.frames.pin_input(1)
}

#[test]
fn old_frame_is_processed_before_new_frame_and_commit_is_exactly_once() -> Result<()> {
    let mut h = harness(FacetInputConfig {
        limits: limits(),
        max_queued_inputs: 4,
        max_pending_delivery_batches: 4,
        max_events_per_result: 4,
        max_requests_per_result: 4,
        max_text_bytes_per_input: 32,
    })?;
    let old = h.frames.pin_input(1)?;
    let old_event = InputEvent::Text { text: "old".into() };
    h.input.admit(Arc::clone(&old), &old_event)?;
    let new = commit_new_frame(&mut h)?;
    let new_event = InputEvent::Text { text: "new".into() };
    h.input.admit(new, &new_event)?;
    let request = h.input.request()?.context("first request")?;
    ensure!(request.value()?.token.frame == old.token().invocation);
    ensure!(request.value()?.limits.max_events == 4);
    ensure!(request.value()?.limits.max_host_requests == 4);
    let first_response = response(request.value()?, b"s1", b"old-result");
    h.input.commit(&first_response)?;
    ensure!(h.input.take_delivery().is_some());
    let second = h.input.request()?.context("second request")?;
    ensure!(second.value()?.token.snapshot_revision == 2);
    let second_token = second.value()?.token.clone();
    let second_response = response(second.value()?, b"s2", b"new-result");
    h.input.commit(&second_response)?;
    ensure!(h.input.take_delivery().context("second delivery")?.token == second_token);
    Ok(())
}

#[test]
fn queue_and_delivery_saturation_preserve_retryable_state() -> Result<()> {
    let mut h = harness(FacetInputConfig {
        limits: limits(),
        max_queued_inputs: 1,
        max_pending_delivery_batches: 1,
        max_events_per_result: 2,
        max_requests_per_result: 2,
        max_text_bytes_per_input: 4,
    })?;
    let frame = h.frames.pin_input(1)?;
    let first_event = InputEvent::Paste { text: "one".into() };
    let oversized = InputEvent::Text {
        text: "12345".into(),
    };
    ensure!(h.input.admit(Arc::clone(&frame), &oversized).is_err());
    h.input.admit(Arc::clone(&frame), &first_event)?;
    let rejected_event = InputEvent::Text { text: "two".into() };
    ensure!(h.input.admit(frame, &rejected_event).is_err());
    let first = h.input.request()?.context("first request")?;
    let first_response = response(first.value()?, b"state", b"first");
    h.input.commit(&first_response)?;
    let frame = h.frames.pin_input(1)?;
    let second_event = InputEvent::Composition {
        text: "two".into(),
        committed: true,
    };
    h.input.admit(frame, &second_event)?;
    let second = h.input.request()?.context("second request")?;
    let retry = response(second.value()?, b"state-2", b"second");
    ensure!(h.input.commit(&retry).is_err());
    ensure!(h.input.request()?.is_none());
    ensure!(h.input.take_delivery().is_some());
    h.input.commit(&retry)?;
    ensure!(h.input.take_delivery().is_some());
    Ok(())
}

#[test]
fn pending_request_can_retry_without_advancing_input_or_losing_delivery() -> Result<()> {
    let mut h = harness(FacetInputConfig {
        limits: limits(),
        max_queued_inputs: 4,
        max_pending_delivery_batches: 4,
        max_events_per_result: 2,
        max_requests_per_result: 2,
        max_text_bytes_per_input: 16,
    })?;
    let frame = h.frames.pin_input(1)?;

    let committed_event = InputEvent::Text {
        text: "committed".into(),
    };
    h.input.admit(Arc::clone(&frame), &committed_event)?;
    let committed_request = h.input.request()?.context("committed request")?;
    let committed_response = response(committed_request.value()?, b"state-1", b"delivered");
    h.input.commit(&committed_response)?;
    drop(committed_request);

    let retry_event = InputEvent::Text {
        text: "retry".into(),
    };
    h.input.admit(frame, &retry_event)?;
    let first = h.input.request()?.context("first retry request")?;
    let first_value = first.value()?.clone();
    h.input.retry_pending_request()?;
    drop(first);

    let second = h.input.request()?.context("second retry request")?;
    ensure!(*second.value()? == first_value);
    let second_response = response(second.value()?, b"state-2", b"retried");
    let old_response = second_response.clone();
    h.input.commit(&second_response)?;
    drop(second);
    ensure!(h.input.commit(&old_response).is_err());

    ensure!(
        h.input
            .take_delivery()
            .context("first delivery")?
            .events
            .len()
            == 1
    );
    ensure!(
        h.input
            .take_delivery()
            .context("second delivery")?
            .events
            .len()
            == 1
    );
    ensure!(h.input.take_delivery().is_none());
    Ok(())
}

#[test]
fn cancel_revokes_pending_response_and_validates_pointer_inputs() -> Result<()> {
    let mut h = harness(FacetInputConfig {
        limits: limits(),
        max_queued_inputs: 2,
        max_pending_delivery_batches: 2,
        max_events_per_result: 2,
        max_requests_per_result: 2,
        max_text_bytes_per_input: 8,
    })?;
    let frame = h.frames.pin_input(1)?;
    let invalid_event = InputEvent::PointerMotion {
        x: f64::NAN,
        y: 1.0,
    };
    ensure!(h.input.admit(Arc::clone(&frame), &invalid_event).is_err());
    let valid_event = InputEvent::PointerButton {
        x: 1.0,
        y: 2.0,
        button: 1,
        pressed: true,
        modifiers: 4,
    };
    h.input.admit(frame, &valid_event)?;
    let request = h.input.request()?.context("pending request")?;
    let committed = response(request.value()?, b"state", b"committed");
    h.input.commit(&committed)?;
    let frame = h.frames.pin_input(1)?;
    let pending_event = InputEvent::Text {
        text: "pending".into(),
    };
    h.input.admit(frame, &pending_event)?;
    let pending = h.input.request()?.context("pending request")?;
    let stale_response = response(pending.value()?, b"new-state", b"stale");
    let retained_with_request = h.budget.retention().live_bytes;
    h.input.cancel()?;
    ensure!(h.input.commit(&stale_response).is_err());
    ensure!(h.input.take_delivery().is_some());
    ensure!(h.input.request()?.is_none());
    let retained_after_cancel = h.budget.retention().live_bytes;
    ensure!(retained_after_cancel > 0 && retained_after_cancel < retained_with_request);
    drop(pending);
    ensure!(h.budget.retention().live_bytes < retained_after_cancel);
    Ok(())
}
