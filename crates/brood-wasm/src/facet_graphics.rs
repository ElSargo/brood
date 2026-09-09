//! Packet validation and invocation resource ownership before native preparation.

use crate::{
    facet_frames::{FacetFrames, PreparedFrame},
    facet_resources::{FacetResourceLease, ResourceKind},
    snapshot_budget::{BudgetedValue, ByteBudget},
};
use anyhow::{Result, ensure};
use brood_graphics::{Command, Packet};
use brood_protocol::facets::RenderResponse;
use std::{collections::BTreeMap, sync::Arc};

/// This value is still speculative. The client must finish native resource/GPU
/// preparation, then promote this complete set and its paired frame together.
pub struct ValidatedFacetGraphics {
    frame: PreparedFrame,
    packets: BTreeMap<u64, BudgetedValue<Packet>>,
}

impl ValidatedFacetGraphics {
    pub fn validate(
        frames: &FacetFrames,
        response: &RenderResponse,
        resources: Option<Arc<FacetResourceLease>>,
        budget: &ByteBudget,
    ) -> Result<Self> {
        let frame = frames.prepare_with_resources(response, resources)?;
        let mut packets = BTreeMap::new();
        for (&pane, pair) in frame.panes() {
            ensure!(
                pair.token().scope.actor_generation == budget.generation(),
                "foreign graphics budget"
            );
            let bytes = pair.packet()?;
            let bound = Packet::decode_allocation_bound(bytes)?;
            let packet = BudgetedValue::capture(budget, bound, || Packet::decode(bytes))?;
            for command in &packet.value()?.commands {
                let handle = match command {
                    Command::DrawImage { image, .. } => Some((*image, ResourceKind::Image)),
                    Command::DrawTextLayout { layout, .. } => Some((*layout, ResourceKind::Text)),
                    _ => None,
                };
                if let Some((handle, kind)) = handle {
                    pair.resources()
                        .ok_or_else(|| anyhow::anyhow!("facet packet has no resource authority"))?
                        .validate_kind(pane, handle, kind)?;
                }
            }
            packets.insert(pane, packet);
        }
        Ok(Self { frame, packets })
    }

    pub const fn token(&self) -> &brood_protocol::facets::RenderToken {
        self.frame.token()
    }

    pub fn panes(&self) -> impl Iterator<Item = (&u64, &Arc<crate::facet_frames::PaneFrame>)> {
        self.frame.panes()
    }

    pub fn validate_commit(&self, frames: &FacetFrames) -> Result<()> {
        frames.validate_commit(&self.frame)
    }

    pub fn packets(&self) -> impl Iterator<Item = (&u64, &BudgetedValue<Packet>)> {
        self.packets.iter()
    }

    /// Host commit only; the native compositor remains responsible for honest
    /// confirmed-presentation tracking and for retaining its prepared resources.
    pub fn commit(self, frames: &mut FacetFrames) -> Result<Option<u64>> {
        frames.commit(self.frame)
    }
}
