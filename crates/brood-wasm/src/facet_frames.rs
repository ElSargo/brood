//! Atomic ownership of a pane's draw packet and its input interpretation data.
//!
//! This admission layer does not prepare graphics resources or announce physical
//! presentation. The compositor must prepare every packet before calling commit.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Result, ensure};
use brood_protocol::facets::{
    InvocationLimits, PaneContent, PaneTarget, RenderResponse, RenderToken, Scope,
};

use crate::{
    facet_resources::FacetResourceLease,
    snapshot_budget::{BudgetedBytes, ByteBudget},
    snapshot_store::SnapshotLease,
};

/// An immutable pair, including the application revision which produced it.
#[derive(Debug)]
pub struct PaneFrame {
    token: RenderToken,
    geometry: PaneTarget,
    packet: BudgetedBytes,
    interaction: BudgetedBytes,
    snapshot: SnapshotLease,
    resources: Option<Arc<FacetResourceLease>>,
}

impl PaneFrame {
    pub const fn token(&self) -> &RenderToken {
        &self.token
    }
    pub const fn geometry(&self) -> &PaneTarget {
        &self.geometry
    }
    pub fn packet(&self) -> Result<&[u8]> {
        ensure!(
            self.snapshot.is_authoritative(),
            "frame snapshot authority is invalidated"
        );
        Ok(self.packet.bytes()?)
    }
    pub fn interaction(&self) -> Result<&[u8]> {
        ensure!(
            self.snapshot.is_authoritative(),
            "frame snapshot authority is invalidated"
        );
        Ok(self.interaction.bytes()?)
    }
    pub const fn snapshot(&self) -> &SnapshotLease {
        &self.snapshot
    }
    pub fn resources(&self) -> Option<&FacetResourceLease> {
        self.resources.as_deref()
    }
}

/// Host-owned pending work. Tokens are issued by the coordinator, never accepted
/// as authority solely because a guest echoed their numeric fields.
struct Invocation {
    token: RenderToken,
    targets: BTreeMap<u64, PaneTarget>,
    snapshot: SnapshotLease,
}

pub struct PreparedFrame {
    token: RenderToken,
    panes: BTreeMap<u64, Arc<PaneFrame>>,
    next_frame_ns: Option<u64>,
}

impl PreparedFrame {
    pub const fn token(&self) -> &RenderToken {
        &self.token
    }

    pub fn panes(&self) -> impl Iterator<Item = (&u64, &Arc<PaneFrame>)> {
        self.panes.iter()
    }
}

/// One coordinator belongs to one facet generation and one owned pane set.
pub struct FacetFrames {
    scope: Scope,
    budget: ByteBudget,
    max_panes: usize,
    limits: InvocationLimits,
    next_invocation: u64,
    pending: Option<Invocation>,
    committed: BTreeMap<u64, Arc<PaneFrame>>,
    active: bool,
}

impl FacetFrames {
    pub fn new(
        scope: Scope,
        budget: ByteBudget,
        max_panes: usize,
        limits: InvocationLimits,
    ) -> Result<Self> {
        ensure!(max_panes > 0, "facet pane limit must be nonzero");
        ensure!(
            scope.actor_generation == budget.generation(),
            "foreign facet byte budget"
        );
        Ok(Self {
            scope,
            budget,
            max_panes,
            limits,
            next_invocation: 1,
            pending: None,
            committed: BTreeMap::new(),
            active: true,
        })
    }

    /// Supersede earlier speculative work. Geometry/configuration changes must
    /// issue a new invocation before an older prepared result can commit.
    pub fn issue(
        &mut self,
        mut token: RenderToken,
        targets: Vec<PaneTarget>,
        snapshot: SnapshotLease,
    ) -> Result<RenderToken> {
        ensure!(
            self.active && self.budget.is_active(),
            "facet generation is unavailable"
        );
        ensure!(token.scope == self.scope, "foreign facet scope");
        ensure!(
            snapshot.generation() == self.scope.actor_generation,
            "foreign snapshot generation"
        );
        ensure!(
            snapshot.is_authoritative(),
            "snapshot authority is invalidated"
        );
        ensure!(
            snapshot.revision() == token.snapshot_revision,
            "snapshot revision mismatch"
        );
        ensure!(
            !targets.is_empty() && targets.len() <= self.max_panes,
            "invalid facet pane count"
        );
        let mut panes = BTreeMap::new();
        for target in targets {
            target.validate().map_err(anyhow::Error::msg)?;
            ensure!(
                panes.insert(target.pane, target).is_none(),
                "duplicate pane target"
            );
        }
        let next = self
            .next_invocation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("facet invocation IDs exhausted"))?;
        token.invocation = self.next_invocation;
        self.next_invocation = next;
        self.pending = Some(Invocation {
            token: token.clone(),
            targets: panes,
            snapshot,
        });
        Ok(token)
    }

    /// Validate all pane entries and reserve retained bytes before exposing any
    /// of the new pairs. The caller separately owns/admit-bounds decoded input.
    pub fn prepare(&self, response: &RenderResponse) -> Result<PreparedFrame> {
        self.prepare_with_resources(response, None)
    }

    pub fn prepare_with_resources(
        &self,
        response: &RenderResponse,
        resources: Option<Arc<FacetResourceLease>>,
    ) -> Result<PreparedFrame> {
        if let Some(resources) = &resources {
            ensure!(
                resources.token() == &response.token,
                "foreign invocation resource lease"
            );
            resources.seal()?;
        }
        ensure!(
            self.active && self.budget.is_active(),
            "facet generation is unavailable"
        );
        let pending = self
            .pending
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no pending facet invocation"))?;
        ensure!(
            response.token == pending.token,
            "stale or foreign facet response"
        );
        ensure!(
            pending.snapshot.is_authoritative(),
            "snapshot authority is invalidated"
        );
        ensure!(
            response.panes.len() == pending.targets.len(),
            "incomplete facet pane set"
        );
        let mut panes = BTreeMap::new();
        for output in &response.panes {
            let target = pending
                .targets
                .get(&output.pane)
                .ok_or_else(|| anyhow::anyhow!("foreign output pane"))?;
            ensure!(!panes.contains_key(&output.pane), "duplicate output pane");
            let frame = match &output.content {
                PaneContent::Draw {
                    packet,
                    interaction,
                } => {
                    ensure!(
                        packet.len() <= self.limits.max_packet_bytes as usize
                            && interaction.len() <= self.limits.max_interaction_bytes as usize,
                        "pane output exceeds invocation limits"
                    );
                    Arc::new(PaneFrame {
                        token: pending.token.clone(),
                        geometry: target.clone(),
                        packet: BudgetedBytes::copy_from_slice(&self.budget, packet)?,
                        interaction: BudgetedBytes::copy_from_slice(&self.budget, interaction)?,
                        snapshot: pending.snapshot.clone(),
                        resources: resources.as_ref().map(Arc::clone),
                    })
                }
                PaneContent::NoChange => {
                    let retained = self
                        .committed
                        .get(&output.pane)
                        .ok_or_else(|| anyhow::anyhow!("no retained pane output"))?;
                    ensure!(
                        retained.geometry == *target,
                        "no-change requires identical pane geometry"
                    );
                    ensure!(
                        retained.token.host_configuration_revision
                            == pending.token.host_configuration_revision
                            && retained.token.geometry_revision == pending.token.geometry_revision
                            && retained.token.renderer_generation
                                == pending.token.renderer_generation
                            && retained.token.interaction_revision
                                == pending.token.interaction_revision
                            && retained.token.pane_membership_revision
                                == pending.token.pane_membership_revision,
                        "no-change visual dependencies changed"
                    );
                    // Preserve both the old layout and its old application provenance.
                    Arc::clone(retained)
                }
            };
            panes.insert(output.pane, frame);
        }
        Ok(PreparedFrame {
            token: response.token.clone(),
            panes,
            next_frame_ns: response.next_frame_ns,
        })
    }

    /// Called only after successful graphics preparation. This records host
    /// commit identity, not confirmation that pixels reached a physical display.
    pub fn commit(&mut self, prepared: PreparedFrame) -> Result<Option<u64>> {
        self.validate_commit(&prepared)?;
        self.committed = prepared.panes;
        self.pending = None;
        Ok(prepared.next_frame_ns)
    }

    /// Preflight native promotion without publishing interaction authority.
    /// The caller must serialize this check and commit with geometry changes.
    pub fn validate_commit(&self, prepared: &PreparedFrame) -> Result<()> {
        ensure!(
            self.active && self.budget.is_active(),
            "facet generation is unavailable"
        );
        ensure!(
            self.pending
                .as_ref()
                .is_some_and(|pending| pending.token == prepared.token
                    && pending.snapshot.is_authoritative()),
            "superseded prepared facet frame"
        );
        Ok(())
    }

    /// Input admission pins the committed pair now, before queuing the event.
    pub fn pin_input(&self, pane: u64) -> Result<Arc<PaneFrame>> {
        ensure!(
            self.active && self.budget.is_active(),
            "facet generation is unavailable"
        );
        self.committed
            .get(&pane)
            .map(Arc::clone)
            .ok_or_else(|| anyhow::anyhow!("pane has no committed facet frame"))
    }

    /// Whether an issued frame still awaits native commit or explicit discard.
    pub const fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// A late discard from an older receipt must not cancel its successor.
    pub fn discard_invocation(&mut self, invocation: u64) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.token.invocation == invocation)
        {
            self.pending = None;
        }
    }

    /// Retire speculative authority while retaining the visible input frame.
    pub fn discard_pending(&mut self) {
        self.pending = None;
    }

    pub fn invalidate(&mut self) {
        self.active = false;
        self.pending = None;
        self.committed.clear();
        self.budget.invalidate();
    }
}
