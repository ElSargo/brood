//! Bounded admission and exactly-once coordination for pane input facets.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{Result, ensure};
use brood_protocol::facets::{HostRequest, InputEvent, InputRequest, InputResponse, InputToken};

use crate::{
    facet_frames::PaneFrame,
    snapshot_budget::{BudgetedBytes, BudgetedValue, ByteBudget},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FacetInputConfig {
    pub limits: brood_protocol::facets::InvocationLimits,
    pub max_queued_inputs: usize,
    pub max_pending_delivery_batches: usize,
    pub max_events_per_result: usize,
    pub max_requests_per_result: usize,
    pub max_text_bytes_per_input: usize,
}

struct QueuedInput {
    frame: Arc<PaneFrame>,
    event: BudgetedValue<InputEvent>,
}

struct Pending {
    token: InputToken,
}

#[derive(Debug)]
pub struct AdmittedInputBatch {
    pub token: InputToken,
    pub events: Vec<BudgetedBytes>,
    pub requests: Vec<HostRequest>,
}

impl AdmittedInputBatch {
    pub fn event_bytes(&self) -> Result<Vec<&[u8]>> {
        self.events
            .iter()
            .map(BudgetedBytes::bytes)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

pub struct FacetInput {
    scope: brood_protocol::facets::Scope,
    budget: ByteBudget,
    config: FacetInputConfig,
    next_input_sequence: u64,
    sequence_revision: u64,
    sequence_state: Option<BudgetedBytes>,
    queue: VecDeque<QueuedInput>,
    pending: Option<Pending>,
    deliveries: VecDeque<AdmittedInputBatch>,
}

impl FacetInput {
    pub fn new(
        scope: brood_protocol::facets::Scope,
        budget: ByteBudget,
        config: FacetInputConfig,
    ) -> Result<Self> {
        ensure!(
            budget.generation() == scope.actor_generation,
            "foreign input byte budget"
        );
        ensure!(
            config.max_queued_inputs > 0,
            "input queue limit must be nonzero"
        );
        ensure!(
            config.max_pending_delivery_batches > 0,
            "input delivery limit must be nonzero"
        );
        Ok(Self {
            scope,
            budget,
            config,
            next_input_sequence: 1,
            sequence_revision: 0,
            sequence_state: None,
            queue: VecDeque::new(),
            pending: None,
            deliveries: VecDeque::new(),
        })
    }

    /// Admit an event against the exact committed frame it was routed to.
    pub fn admit(&mut self, frame: Arc<PaneFrame>, event: &InputEvent) -> Result<()> {
        ensure!(
            self.queue.len() < self.config.max_queued_inputs,
            "input queue is full"
        );
        ensure!(
            frame.token().scope == self.scope,
            "foreign committed frame scope"
        );
        validate_event(event)?;
        let text_bytes = event_text_bytes(event);
        if text_bytes > 0 {
            ensure!(
                text_bytes <= self.config.max_text_bytes_per_input,
                "input text exceeds its per-input limit"
            );
        }
        frame.interaction()?;
        let event_bytes = event_payload_len(event)?;
        let event = BudgetedValue::capture(&self.budget, event_bytes, || {
            Ok::<_, std::convert::Infallible>(event.clone())
        })?;
        self.queue.push_back(QueuedInput { frame, event });
        Ok(())
    }

    pub fn can_accept_result(&self) -> bool {
        self.deliveries.len() < self.config.max_pending_delivery_batches
    }

    /// Prepare one request for the queue head. The head remains retryable until
    /// its matching response is committed.
    pub fn request(&mut self) -> Result<Option<BudgetedValue<InputRequest>>> {
        if self.pending.is_some() {
            return Ok(None);
        }
        let Some(head) = self.queue.front() else {
            return Ok(None);
        };
        let frame_token = head.frame.token();
        let interaction = head.frame.interaction()?;
        let event = head.event.value()?;
        let state = self
            .sequence_state
            .as_ref()
            .map(BudgetedBytes::bytes)
            .transpose()?
            .unwrap_or_default();
        let allocation_bytes = event_payload_len(event)?
            .checked_add(interaction.len())
            .and_then(|bytes| bytes.checked_add(state.len()))
            .ok_or_else(|| anyhow::anyhow!("input request allocation length overflow"))?;
        let token = InputToken {
            scope: self.scope.clone(),
            input_sequence: self.next_input_sequence,
            frame: frame_token.invocation,
            pane: head.frame.geometry().pane,
            snapshot_revision: frame_token.snapshot_revision,
            sequence_revision: self.sequence_revision,
        };
        let request = BudgetedValue::capture(&self.budget, allocation_bytes, || {
            Ok::<_, std::convert::Infallible>(InputRequest {
                token: token.clone(),
                geometry: head.frame.geometry().clone(),
                event: event.clone(),
                interaction: interaction.to_vec(),
                sequence_state: state.to_vec(),
                limits: self.config.limits,
            })
        })?;
        self.pending = Some(Pending { token });
        Ok(Some(request))
    }

    /// Release downstream admission state while retaining the exact queue
    /// head.  The next request therefore gets the same token and payload;
    /// callers use this when encoding or executor admission must be retried.
    pub fn retry_pending_request(&mut self) -> Result<()> {
        ensure!(self.pending.is_some(), "no pending input request");
        self.pending = None;
        Ok(())
    }

    /// Commit exactly one matching response and retain its event batch.
    pub fn commit(&mut self, response: &InputResponse) -> Result<()> {
        let pending_token = self
            .pending
            .as_ref()
            .map(|pending| pending.token.clone())
            .ok_or_else(|| anyhow::anyhow!("no pending input request"))?;
        ensure!(
            response.token == pending_token,
            "stale or foreign input response"
        );
        ensure!(
            response.events.len() <= self.config.max_events_per_result,
            "input event result is too large"
        );
        ensure!(
            response.events.len() <= self.config.limits.max_events as usize
                && response
                    .events
                    .iter()
                    .all(|event| event.len() <= self.config.limits.max_event_bytes as usize)
                && response.next_sequence_state.len()
                    <= self.config.limits.max_sequence_bytes as usize
                && response.requests.len() <= self.config.limits.max_host_requests as usize,
            "input response exceeds its invocation limits"
        );
        ensure!(
            response.requests.len() <= self.config.max_requests_per_result,
            "host request result is too large"
        );
        ensure!(
            self.deliveries.len() < self.config.max_pending_delivery_batches,
            "input delivery queue is full"
        );

        ensure!(self.budget.is_active(), "input generation is unavailable");
        let next_revision = self
            .sequence_revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("input sequence revisions exhausted"))?;
        let next_input = self
            .next_input_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("input sequence IDs exhausted"))?;
        let next_state =
            BudgetedBytes::copy_from_slice(&self.budget, &response.next_sequence_state)?;
        let events = response
            .events
            .iter()
            .map(|event| BudgetedBytes::copy_from_slice(&self.budget, event))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let token = response.token.clone();
        let requests = response.requests.clone();
        self.pending = None;
        self.sequence_state = Some(next_state);
        self.sequence_revision = next_revision;
        self.next_input_sequence = next_input;
        self.queue.pop_front();
        self.deliveries.push_back(AdmittedInputBatch {
            token,
            events,
            requests,
        });
        Ok(())
    }

    pub fn take_delivery(&mut self) -> Option<AdmittedInputBatch> {
        self.deliveries.pop_front()
    }

    /// Inspect the ordered batch while reserving actual actor mailbox capacity.
    pub fn delivery(&self) -> Option<&AdmittedInputBatch> {
        self.deliveries.front()
    }

    /// Acknowledge only after the complete batch has entered reserved delivery.
    pub fn acknowledge_delivery(&mut self, token: &InputToken) -> Result<()> {
        ensure!(
            self.deliveries
                .front()
                .is_some_and(|batch| batch.token == *token),
            "stale input delivery acknowledgement"
        );
        self.deliveries.pop_front();
        Ok(())
    }

    pub const fn sequence_revision(&self) -> u64 {
        self.sequence_revision
    }

    pub fn sequence_state(&self) -> Result<&[u8]> {
        ensure!(self.budget.is_active(), "input generation is unavailable");
        Ok(self
            .sequence_state
            .as_ref()
            .map(BudgetedBytes::bytes)
            .transpose()?
            .unwrap_or_default())
    }

    /// Cancel speculative input and gesture state. Committed event deliveries
    /// remain ordered and available; cancellation never erases actor actions.
    pub fn cancel(&mut self) -> Result<()> {
        let next_revision = self
            .sequence_revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("input sequence revisions exhausted"))?;
        let next_input = self
            .next_input_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("input sequence IDs exhausted"))?;
        self.queue.clear();
        self.pending = None;
        self.sequence_state = None;
        self.sequence_revision = next_revision;
        self.next_input_sequence = next_input;
        Ok(())
    }
}

fn validate_event(event: &InputEvent) -> Result<()> {
    let coordinates: &[f64] = match event {
        InputEvent::PointerMotion { x, y }
        | InputEvent::PointerButton { x, y, .. }
        | InputEvent::PointerScroll { x, y, .. } => &[*x, *y],
        _ => &[],
    };
    ensure!(
        coordinates.iter().all(|value| value.is_finite()),
        "pointer coordinates must be finite"
    );
    if let InputEvent::PointerScroll {
        horizontal,
        vertical,
        ..
    } = event
    {
        ensure!(
            horizontal.is_finite() && vertical.is_finite(),
            "scroll deltas must be finite"
        );
    }
    Ok(())
}

const fn event_text_bytes(event: &InputEvent) -> usize {
    match event {
        InputEvent::Text { text }
        | InputEvent::Paste { text }
        | InputEvent::Composition { text, .. } => text.len(),
        _ => 0,
    }
}

fn event_payload_len(event: &InputEvent) -> Result<usize> {
    match event {
        InputEvent::Key { logical_key, .. } => Ok(logical_key.len()),
        InputEvent::Text { text }
        | InputEvent::Paste { text }
        | InputEvent::Composition { text, .. } => Ok(text.len()),
        InputEvent::Image {
            name,
            mime_type,
            bytes,
            preview_rgba8,
            ..
        } => name
            .len()
            .checked_add(mime_type.len())
            .and_then(|n| n.checked_add(bytes.len()))
            .and_then(|n| n.checked_add(preview_rgba8.len()))
            .ok_or_else(|| anyhow::anyhow!("input payload length overflow")),
        _ => Ok(0),
    }
}
