use std::{env, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use brood_protocol::{
    facet_codec::DecodeLimits,
    facets::{InputEvent, InvocationLimits, PaneTarget, RenderRequest, RenderToken, Scope},
};
use brood_wasm::{
    ComponentActor, ComponentActorConfig, ComponentActorEvent,
    facet_executor::{FacetExecutor, FacetWorker},
    facet_frames::FacetFrames,
    facet_graphics::ValidatedFacetGraphics,
    facet_input::{FacetInput, FacetInputConfig},
    facet_resources::{FacetResourceBackend, FacetResourceRegistry, FacetResourceRegistryConfig},
    facet_runtime::{FacetOperation, FacetRuntimeConfig},
    facet_wire,
    presentation::{PresentationError, TextLayoutOptions, TextLayoutResult},
    snapshot_budget::{BudgetedBytes, ByteBudget, ByteBudgetLimits},
    snapshot_reclaimer::{BackgroundSnapshotReclaimer, SnapshotReclaimerConfig},
    snapshot_store::SnapshotStoreLimits,
};

#[derive(Debug)]
struct ProbeBackend;
impl FacetResourceBackend for ProbeBackend {
    fn create_image(
        &self,
        _pane: u64,
        _width: u32,
        _height: u32,
        pixels: &[u8],
        max_bytes: usize,
    ) -> Result<(u64, usize), PresentationError> {
        if pixels.len() > max_bytes {
            return Err(PresentationError::LimitReached);
        }
        Ok((100, pixels.len()))
    }
    fn create_text(
        &self,
        _pane: u64,
        text: &str,
        _options: TextLayoutOptions,
        max_bytes: usize,
    ) -> Result<(u64, TextLayoutResult, usize), PresentationError> {
        if text.len() > max_bytes {
            return Err(PresentationError::LimitReached);
        }
        Ok((
            200,
            TextLayoutResult {
                handle: 200,
                width: 10.0,
                height: 14.0,
                first_baseline: 11.0,
                line_count: 1,
                cell_width: 0.0,
                line_height: 14.0,
            },
            text.len(),
        ))
    }
    fn release(&self, _pane: u64, _handle: u64) -> Result<(), PresentationError> {
        Ok(())
    }
}

const WIRE_BYTES: usize = 256 * 1024;
const WIRE_BYTES_U32: u32 = 256 * 1024;
const INVOCATION_LIMITS: InvocationLimits = InvocationLimits {
    max_output_bytes: WIRE_BYTES_U32,
    max_packet_bytes: 64 * 1024,
    max_interaction_bytes: 64 * 1024,
    max_sequence_bytes: 4096,
    max_events: 8,
    max_event_bytes: 4096,
    max_host_requests: 4,
    max_resources: 2,
    max_resource_bytes: 4096,
};

#[tokio::main]
async fn main() -> Result<()> {
    let component = PathBuf::from(
        env::args_os()
            .nth(1)
            .context("usage: paired_facet_probe <component>")?,
    );
    let mut actor = ComponentActor::load(&component, ComponentActorConfig::default())
        .await
        .context("load paired actor instance")?;
    let generation = actor.generation();
    let scope = Scope {
        client_generation: 1,
        actor_generation: generation,
        facet_generation: 1,
    };
    let reclaimer = BackgroundSnapshotReclaimer::new(SnapshotReclaimerConfig {
        max_generations: 1,
        poll_interval: Duration::from_millis(1),
    })?;
    let snapshots = reclaimer.register(
        generation,
        SnapshotStoreLimits {
            max_live_bytes: 4096,
            max_live_allocations: 16,
            max_live_roots: 8,
            max_snapshot_bytes: 1024,
            max_chunks_per_snapshot: 4,
        },
    )?;
    let bytes = ByteBudget::new(
        generation,
        ByteBudgetLimits {
            max_live_bytes: 16 * 1024 * 1024,
            max_live_allocations: 64,
            max_buffer_bytes: 4 * 1024 * 1024,
        },
    );
    let mut frames = FacetFrames::new(scope.clone(), bytes.clone(), 1, INVOCATION_LIMITS)?;
    let mut input = FacetInput::new(
        scope.clone(),
        bytes.clone(),
        FacetInputConfig {
            limits: INVOCATION_LIMITS,
            max_queued_inputs: 4,
            max_pending_delivery_batches: 1,
            max_events_per_result: 8,
            max_requests_per_result: 4,
            max_text_bytes_per_input: 4096,
        },
    )?;
    let executor = FacetExecutor::new(1, 2)?;
    let resource_registry = FacetResourceRegistry::new(
        Arc::new(ProbeBackend),
        FacetResourceRegistryConfig {
            max_invocations: 2,
            retry_interval: Duration::from_millis(1),
        },
    )?;
    let resource_budget = ByteBudget::new(
        generation,
        ByteBudgetLimits {
            max_live_bytes: 1024 * 1024,
            max_live_allocations: 4,
            max_buffer_bytes: 1024 * 1024,
        },
    );
    let runtime_config = FacetRuntimeConfig {
        max_input_bytes: WIRE_BYTES,
        max_output_bytes: WIRE_BYTES,
        ..FacetRuntimeConfig::default()
    };
    let worker = executor
        .load(component.clone(), runtime_config)
        .await?
        .context("paired facets absent")?;

    let first_ids = ids(&[101, 202, 303]);
    snapshots.publish_copy_of(1, &first_ids)?;
    let first_snapshot = snapshots
        .acquire_latest()?
        .context("first snapshot missing")?;
    let first = render(
        &worker,
        &bytes,
        render_request(&scope, 1, b"draw", first_ids.clone()),
        &mut frames,
        first_snapshot,
        true,
    )
    .await?;
    ensure!(
        first.packet()?.starts_with(b"MFGX"),
        "guest emitted invalid graphics packet header"
    );
    input.admit(
        std::sync::Arc::clone(&first),
        &InputEvent::PointerButton {
            x: 2.0,
            y: 25.0,
            button: 1,
            pressed: true,
            modifiers: 0,
        },
    )?;

    let second_ids = ids(&[303, 101]);
    snapshots.publish_copy_of(2, &second_ids)?;
    let second_snapshot = snapshots
        .acquire_latest()?
        .context("second snapshot missing")?;
    let _second = render(
        &worker,
        &bytes,
        render_request(&scope, 2, b"draw", second_ids),
        &mut frames,
        second_snapshot,
        false,
    )
    .await?;
    let request = input.request()?.context("queued input missing")?;
    ensure!(
        request.value()?.token.snapshot_revision == 1,
        "queued input changed provenance"
    );
    let response = invoke_input(&worker, &bytes, request.value()?).await?;
    input.commit(response.value()?)?;
    let delivered = input
        .delivery()
        .context("stable target was not delivered")?;
    ensure!(
        delivered.event_bytes()? == vec![202_u64.to_le_bytes().as_slice()],
        "reordered/deleted view changed stable target"
    );
    let delivered_token = delivered.token.clone();
    actor.try_send_facet_input(delivered)?;
    input.acknowledge_delivery(&delivered_token)?;
    let actor_payload = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match actor.recv_event().await {
                Some(ComponentActorEvent::HostMessageRequested { payload }) => break Ok(payload),
                Some(ComponentActorEvent::DispatcherFailed { message }) => {
                    break Err(anyhow::anyhow!("paired actor dispatcher failed: {message}"));
                }
                Some(ComponentActorEvent::Stopped) | None => {
                    break Err(anyhow::anyhow!(
                        "paired actor stopped before semantic delivery"
                    ));
                }
                Some(_) => {}
            }
        }
    })
    .await
    .context("paired actor did not process semantic input")??;
    let mut expected_actor_payload = b"facet-input-rejected:".to_vec();
    expected_actor_payload.extend_from_slice(&202_u64.to_le_bytes());
    expected_actor_payload.extend_from_slice(&1_u64.to_le_bytes());
    ensure!(
        actor_payload == expected_actor_payload,
        "actor did not reject the removed stable ID with its source revision"
    );

    snapshots.publish_copy_of(3, &ids(&[303, 101]))?;
    let third_snapshot = snapshots.acquire_latest()?.context("snapshot missing")?;
    let retained = render(
        &worker,
        &bytes,
        render_request(&scope, 3, b"no-change", ids(&[303, 101])),
        &mut frames,
        third_snapshot,
        false,
    )
    .await?;
    ensure!(
        retained.snapshot().revision() == 2,
        "no-change changed source revision"
    );

    let resource_snapshot = snapshots.acquire_latest()?.context("snapshot missing")?;
    let mut resource_request = render_request(&scope, 3, b"graphics-probe", ids(&[303, 101]));
    resource_request.token = frames.issue(
        resource_request.token,
        resource_request.panes.clone(),
        resource_snapshot,
    )?;
    let lease = resource_registry.begin(
        resource_request.token.clone(),
        &[99],
        INVOCATION_LIMITS,
        resource_budget.clone(),
    )?;
    let encoded = facet_wire::render_request(&resource_request, &bytes, WIRE_BYTES)?;
    let output = worker
        .try_invoke_with_resources(encoded, bytes.clone(), Arc::clone(&lease))?
        .await??;
    let decoded = facet_wire::render_response(&output, &bytes, decode_limits())?;
    let validated = ValidatedFacetGraphics::validate(
        &frames,
        decoded.value()?,
        Some(Arc::clone(&lease)),
        &bytes,
    )?;
    validated.commit(&mut frames)?;
    lease.validate_handle(99, 100)?;
    ensure!(
        lease.validate_handle(99, 200).is_err(),
        "guest-dropped text handle remained usable"
    );

    input.admit(
        frames.pin_input(99)?,
        &InputEvent::Text {
            text: "graphics-denied".into(),
        },
    )?;
    let denied_request = input
        .request()?
        .context("graphics denial request missing")?;
    let denied_response = invoke_input(&worker, &bytes, denied_request.value()?).await?;
    input.commit(denied_response.value()?)?;
    input
        .take_delivery()
        .context("graphics denial delivery missing")?;

    // Delivery saturation preserves both semantic output and sequence state for retry.
    input.admit(
        std::sync::Arc::clone(&retained),
        &InputEvent::PointerButton {
            x: 1.0,
            y: 1.0,
            button: 1,
            pressed: true,
            modifiers: 0,
        },
    )?;
    let first_request = input
        .request()?
        .context("first saturation request missing")?;
    let first_response = invoke_input(&worker, &bytes, first_request.value()?).await?;
    input.commit(first_response.value()?)?;
    input.admit(
        retained,
        &InputEvent::PointerButton {
            x: 1.0,
            y: 21.0,
            button: 1,
            pressed: true,
            modifiers: 0,
        },
    )?;
    let second_request = input
        .request()?
        .context("second saturation request missing")?;
    let second_response = invoke_input(&worker, &bytes, second_request.value()?).await?;
    ensure!(
        input.commit(second_response.value()?).is_err(),
        "saturated delivery unexpectedly committed"
    );
    input
        .take_delivery()
        .context("first saturated delivery missing")?;
    input.commit(second_response.value()?)?;
    ensure!(
        input.commit(second_response.value()?).is_err(),
        "input response committed twice"
    );
    let second_delivery = input.take_delivery().context("retried delivery missing")?;
    ensure!(
        second_delivery.event_bytes()? == vec![101_u64.to_le_bytes().as_slice()],
        "semantic output was not atomic"
    );
    input.admit(
        frames.pin_input(99)?,
        &InputEvent::PointerMotion { x: 0.0, y: 0.0 },
    )?;
    let state_probe = input.request()?.context("state probe missing")?;
    ensure!(
        state_probe.value()?.sequence_state == 101_u64.to_le_bytes(),
        "sequence state did not commit atomically with semantic output"
    );
    input.cancel()?;

    // A speculative render is not input authority until commit.
    let speculative_snapshot = snapshots.acquire_latest()?.context("snapshot missing")?;
    let request = render_request(&scope, 3, b"draw", ids(&[303, 101]));
    let committed_before = frames.pin_input(99)?;
    let token = frames.issue(
        request.token.clone(),
        request.panes.clone(),
        speculative_snapshot,
    )?;
    ensure!(
        frames.pin_input(99)?.token().invocation == committed_before.token().invocation,
        "speculative render became input authority before commit"
    );
    ensure!(token.invocation > 0);

    let trap_request = render_request(&scope, 2, b"trap", ids(&[303, 101]));
    let encoded = facet_wire::render_request(&trap_request, &bytes, WIRE_BYTES)?;
    ensure!(
        invoke(&worker, FacetOperation::Render, encoded, &bytes)
            .await
            .is_err()
    );
    drop(worker);
    let fresh = executor
        .load(component, runtime_config)
        .await?
        .context("fresh paired facets absent")?;
    let probe = facet_wire::render_request(
        &render_request(&scope, 2, b"draw", ids(&[101])),
        &bytes,
        WIRE_BYTES,
    )?;
    ensure!(
        invoke(&fresh, FacetOperation::Render, probe, &bytes)
            .await
            .is_ok()
    );

    tokio::time::timeout(Duration::from_secs(2), actor.shutdown())
        .await
        .context("paired actor shutdown timed out")??;
    drop(actor);

    println!(
        "PASS paired facet resources, stable actor input, no-change, trap recovery, and atomic delivery"
    );
    Ok(())
}

async fn render(
    worker: &FacetWorker,
    budget: &ByteBudget,
    request: RenderRequest,
    frames: &mut FacetFrames,
    snapshot: brood_wasm::snapshot_store::SnapshotLease,
    expect_pending_only: bool,
) -> Result<std::sync::Arc<brood_wasm::facet_frames::PaneFrame>> {
    let token = frames.issue(request.token.clone(), request.panes.clone(), snapshot)?;
    let mut request = request;
    request.token = token;
    let encoded = facet_wire::render_request(&request, budget, WIRE_BYTES)?;
    let output = invoke(worker, FacetOperation::Render, encoded, budget).await?;
    let decoded = facet_wire::render_response(&output, budget, decode_limits())?;
    let prepared = ValidatedFacetGraphics::validate(frames, decoded.value()?, None, budget)?;
    ensure!(
        prepared.packets().count() == 1,
        "validated packet set incomplete"
    );
    ensure!(frames.pin_input(99).is_err() || !expect_pending_only);
    prepared.commit(frames)?;
    frames.pin_input(99)
}

async fn invoke_input(
    worker: &FacetWorker,
    budget: &ByteBudget,
    request: &brood_protocol::facets::InputRequest,
) -> Result<brood_wasm::snapshot_budget::BudgetedValue<brood_protocol::facets::InputResponse>> {
    let encoded = facet_wire::input_request(request, budget, WIRE_BYTES)?;
    let output = invoke(worker, FacetOperation::Interpret, encoded, budget).await?;
    facet_wire::input_response(&output, budget, decode_limits())
}
async fn invoke(
    worker: &FacetWorker,
    operation: FacetOperation,
    input: BudgetedBytes,
    budget: &ByteBudget,
) -> Result<BudgetedBytes> {
    worker.try_invoke(operation, input, budget.clone())?.await?
}
const fn decode_limits() -> DecodeLimits {
    DecodeLimits {
        max_wire_bytes: WIRE_BYTES,
        max_allocated_bytes: WIRE_BYTES,
        max_allocations: 128,
    }
}
const fn pane() -> PaneTarget {
    PaneTarget {
        pane: 99,
        logical_width: 320.0,
        logical_height: 200.0,
        physical_width: 320,
        physical_height: 200,
        scale: 1.0,
        clip: [0.0, 0.0, 320.0, 200.0],
        visible: true,
        focused: true,
    }
}
fn render_request(
    scope: &Scope,
    revision: u64,
    configuration: &[u8],
    application_view: Vec<u8>,
) -> RenderRequest {
    RenderRequest {
        token: RenderToken {
            scope: scope.clone(),
            invocation: 0,
            snapshot_revision: revision,
            candidate: revision,
            pane_membership_revision: 1,
            geometry_revision: 1,
            host_configuration_revision: 1,
            renderer_generation: 1,
            interaction_revision: 1,
        },
        panes: vec![pane()],
        target_time_ns: 0,
        cutoff_ns: 1,
        configuration: configuration.to_vec(),
        application_view,
        sequence_state: Vec::new(),
        limits: INVOCATION_LIMITS,
    }
}
fn ids(ids: &[u64]) -> Vec<u8> {
    ids.iter().flat_map(|id| id.to_le_bytes()).collect()
}
