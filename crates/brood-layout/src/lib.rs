//! Portable viewport layout primitives.
//!
//! This crate deliberately knows nothing about compositor surfaces, rendering,
//! transports, or plugin runtimes. Presentation hosts supply pane inventory and
//! execute the accepted layout.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use num_traits::ToPrimitive;

const MAX_RESOLVED_LAYOUT_ITEMS: usize = 512;
const MAX_INTERACTION_ZONES: usize = 512;
const MAX_LAYOUT_DIVIDERS: usize = 512;
const MAX_LAYOUT_LEASE_DEPTH: usize = 32;
const NORMALIZED_MIN: f64 = -1.0;
const NORMALIZED_MAX: f64 = 1.0;

fn rounded_layout_coordinate(value: f64) -> i32 {
    if value.is_nan() {
        return 0;
    }
    value.round().to_i32().unwrap_or_else(|| {
        if value.is_sign_negative() {
            i32::MIN
        } else {
            i32::MAX
        }
    })
}

fn rounded_layout_extent(value: f64, bounds: i32) -> i32 {
    if bounds <= 0 || value.is_nan() || value <= 0.0 {
        return 0;
    }
    value
        .round()
        .to_i32()
        .unwrap_or(i32::MAX)
        .clamp(1, i32::MAX)
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
pub struct ResolvedLayout {
    pub items: Vec<ResolvedLayoutItem>,
}

impl ResolvedLayout {
    #[must_use]
    pub const fn empty() -> Self {
        Self { items: Vec::new() }
    }

    pub fn validate_for_panes(self, valid_panes: &[u64]) -> Result<Self, ResolvedLayoutError> {
        if self.items.len() > MAX_RESOLVED_LAYOUT_ITEMS {
            return Err(ResolvedLayoutError(format!(
                "resolved layout item count exceeds {MAX_RESOLVED_LAYOUT_ITEMS}"
            )));
        }

        let mut item_ids = BTreeSet::new();
        let mut pane_ids = BTreeSet::new();
        for item in &self.items {
            if item.id.0 == 0 {
                return Err(ResolvedLayoutError(
                    "resolved layout item IDs must be non-zero".into(),
                ));
            }
            if !item_ids.insert(item.id) {
                return Err(ResolvedLayoutError(format!(
                    "resolved layout item id {} appears more than once",
                    item.id.0
                )));
            }
            if !item.z.is_finite() {
                return Err(ResolvedLayoutError(format!(
                    "resolved layout item {} has non-finite z",
                    item.id.0
                )));
            }
            item.transform.validate(item.id)?;
            match item.kind {
                LayoutItemKind::Pane { pane } => {
                    if !valid_panes.contains(&pane) {
                        return Err(ResolvedLayoutError(format!(
                            "resolved layout references unknown pane {pane}"
                        )));
                    }
                    if !pane_ids.insert(pane) {
                        return Err(ResolvedLayoutError(format!(
                            "resolved layout references pane {pane} more than once"
                        )));
                    }
                    if item.input != InputBehavior::Pane {
                        return Err(ResolvedLayoutError(format!(
                            "pane item {} must use pane input behavior",
                            item.id.0
                        )));
                    }
                }
                LayoutItemKind::Handle => {
                    if !matches!(item.input, InputBehavior::Hit | InputBehavior::Drag) {
                        return Err(ResolvedLayoutError(format!(
                            "handle item {} must use hit or drag input behavior",
                            item.id.0
                        )));
                    }
                }
                LayoutItemKind::Placeholder | LayoutItemKind::Chrome | LayoutItemKind::Overlay => {
                    if item.input == InputBehavior::Pane {
                        return Err(ResolvedLayoutError(format!(
                            "non-pane item {} cannot use pane input behavior",
                            item.id.0
                        )));
                    }
                }
            }
        }
        Ok(self)
    }

    #[must_use]
    pub fn hit_stack(&self, point: LocalPoint) -> Vec<HitResult> {
        let mut hits = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.input != InputBehavior::None)
            .filter_map(|(index, item)| {
                item.transform
                    .project_point(point)
                    .map(|local_point| (index, item, local_point))
            })
            .collect::<Vec<_>>();
        hits.sort_by(|(left_index, left, _), (right_index, right, _)| {
            right
                .z
                .total_cmp(&left.z)
                .then_with(|| right_index.cmp(left_index))
        });
        hits.into_iter()
            .map(|(_, item, local_point)| HitResult {
                item: item.id,
                kind: item.kind,
                local_point,
            })
            .collect()
    }

    #[must_use]
    pub fn pane_item(&self, pane: u64) -> Option<&ResolvedLayoutItem> {
        self.items
            .iter()
            .rev()
            .find(|item| item.kind == LayoutItemKind::Pane { pane })
    }

    #[must_use]
    pub fn placeholder_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.kind == LayoutItemKind::Placeholder)
            .count()
    }

    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| matches!(item.kind, LayoutItemKind::Pane { .. }))
            .count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LayoutLeaseId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutLeaseToken {
    pub id: LayoutLeaseId,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutChildSlot {
    pub z: f64,
    pub transform: LocalTransform,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayoutFragment {
    pub layout: ResolvedLayout,
    pub zones: Vec<InteractionZone>,
    pub dividers: Vec<LayoutDivider>,
    pub rest: Option<LayoutChildSlot>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ComposedLayout {
    pub layout: ResolvedLayout,
    pub zones: Vec<ComposedInteractionZone>,
    pub dividers: Vec<ComposedDivider>,
    pub focus: Option<u64>,
}

impl ComposedLayout {
    /// Returns interaction zones under `point`, ordered from topmost to
    /// bottommost. Positions are expressed in the owning lease's viewport,
    /// not in the interaction zone itself.
    #[must_use]
    pub fn zone_hit_stack(&self, point: LocalPoint) -> Vec<ZoneHitResult> {
        self.zone_hit_stack_for_event(point, PointerEventMask::SUPPORTED)
    }

    /// Returns interaction zones under `point` which subscribe to at least one
    /// event in `events`, ordered from topmost to bottommost.
    #[must_use]
    pub fn zone_hit_stack_for_event(
        &self,
        point: LocalPoint,
        events: PointerEventMask,
    ) -> Vec<ZoneHitResult> {
        let mut hits = self
            .zones
            .iter()
            .enumerate()
            .filter(|(_, zone)| zone.event_mask.intersects(events))
            .filter(|(_, zone)| zone.transform.contains(point))
            .filter_map(|(index, zone)| {
                zone.lease_viewport
                    .project_point(point)
                    .map(|local_position| (index, zone, local_position))
            })
            .collect::<Vec<_>>();
        hits.sort_by(|(left_index, left, _), (right_index, right, _)| {
            right
                .z
                .total_cmp(&left.z)
                .then_with(|| right_index.cmp(left_index))
        });
        hits.into_iter()
            .map(|(_, zone, local_position)| ZoneHitResult {
                zone: zone.id,
                local_position,
                event_mask: zone.event_mask,
                cursor: zone.cursor,
                input: zone.input,
            })
            .collect()
    }

    /// Returns the topmost interaction zone under `point` which subscribes to
    /// `event`.
    #[must_use]
    pub fn top_zone_at(&self, point: LocalPoint, event: PointerEventMask) -> Option<ZoneHitResult> {
        self.zone_hit_stack_for_event(point, event)
            .into_iter()
            .next()
    }
}

#[derive(Debug, Clone)]
pub struct LayoutLeaseTree {
    root: LayoutLeaseId,
    nodes: BTreeMap<LayoutLeaseId, LayoutLeaseNode>,
    next_lease: u64,
    inventory_revision: u64,
    inventory: Vec<u64>,
}

#[derive(Debug, Clone)]
struct LayoutLeaseNode {
    generation: u64,
    parent: Option<LayoutLeaseId>,
    child: Option<LayoutLeaseId>,
    owned_panes: Vec<u64>,
    accepted: Option<AcceptedLayoutFragment>,
}

#[derive(Debug, Clone)]
struct AcceptedLayoutFragment {
    revision: u64,
    fragment: LayoutFragment,
    focus: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutLeaseError(String);

impl Default for LayoutLeaseTree {
    fn default() -> Self {
        let root = LayoutLeaseId(1);
        let mut nodes = BTreeMap::new();
        nodes.insert(
            root,
            LayoutLeaseNode {
                generation: 1,
                parent: None,
                child: None,
                owned_panes: Vec::new(),
                accepted: None,
            },
        );
        Self {
            root,
            nodes,
            next_lease: 2,
            inventory_revision: 0,
            inventory: Vec::new(),
        }
    }
}

impl LayoutLeaseTree {
    #[must_use]
    pub fn root_token(&self) -> LayoutLeaseToken {
        self.token_for(self.root).unwrap_or(LayoutLeaseToken {
            id: self.root,
            generation: 0,
        })
    }

    #[must_use]
    pub const fn inventory_revision(&self) -> u64 {
        self.inventory_revision
    }

    pub fn renew_root(&mut self) -> Result<LayoutLeaseToken, LayoutLeaseError> {
        let root_id = self.root;
        let root = self.node_mut(root_id)?;
        root.generation = root
            .generation
            .checked_add(1)
            .ok_or_else(|| LayoutLeaseError("layout lease generation is exhausted".into()))?;
        root.accepted = None;
        let token = self.root_token();
        if token.generation == 0 {
            return Err(LayoutLeaseError("root layout lease is unavailable".into()));
        }
        Ok(token)
    }

    pub fn update_inventory(
        &mut self,
        revision: u64,
        panes: &[u64],
    ) -> Result<(), LayoutLeaseError> {
        if revision == 0 || revision < self.inventory_revision {
            return Err(LayoutLeaseError(format!(
                "layout inventory revision {revision} is not current after {}",
                self.inventory_revision
            )));
        }
        let inventory = panes.iter().copied().collect::<BTreeSet<_>>();
        if inventory.len() != panes.len() {
            return Err(LayoutLeaseError(
                "layout inventory contains duplicate pane identities".into(),
            ));
        }
        if revision != self.inventory_revision {
            for node in self.nodes.values_mut() {
                node.accepted = None;
            }
        }
        for node in self.nodes.values_mut() {
            node.owned_panes.retain(|pane| inventory.contains(pane));
        }
        let assigned = self
            .nodes
            .values()
            .flat_map(|node| node.owned_panes.iter().copied())
            .collect::<BTreeSet<_>>();
        let unassigned = panes
            .iter()
            .copied()
            .filter(|pane| !assigned.contains(pane))
            .collect::<Vec<_>>();
        let leaf = self.deepest_lease()?;
        let leaf_node = self.node_mut(leaf)?;
        leaf_node.owned_panes.extend(unassigned);
        for node in self.nodes.values_mut() {
            let owned = node.owned_panes.iter().copied().collect::<BTreeSet<_>>();
            node.owned_panes = panes
                .iter()
                .copied()
                .filter(|pane| owned.contains(pane))
                .collect();
        }
        self.inventory_revision = revision;
        self.inventory = panes.to_vec();
        Ok(())
    }

    pub fn grant_rest(
        &mut self,
        parent: LayoutLeaseToken,
        panes: &[u64],
    ) -> Result<LayoutLeaseToken, LayoutLeaseError> {
        self.validate_token(parent)?;
        let depth = self.depth(parent.id)?;
        if depth >= MAX_LAYOUT_LEASE_DEPTH {
            return Err(LayoutLeaseError(format!(
                "layout lease depth exceeds {MAX_LAYOUT_LEASE_DEPTH}"
            )));
        }
        let delegated = panes.iter().copied().collect::<BTreeSet<_>>();
        if delegated.len() != panes.len() {
            return Err(LayoutLeaseError(
                "delegated layout pane set contains duplicates".into(),
            ));
        }
        let parent_node = self.node(parent.id)?;
        if parent_node.child.is_some() {
            return Err(LayoutLeaseError(
                "layout lease already has a rest child".into(),
            ));
        }
        if !delegated
            .iter()
            .all(|pane| parent_node.owned_panes.contains(pane))
        {
            return Err(LayoutLeaseError(
                "layout lease cannot delegate panes it does not own".into(),
            ));
        }
        let id = LayoutLeaseId(self.next_lease);
        self.next_lease = self
            .next_lease
            .checked_add(1)
            .ok_or_else(|| LayoutLeaseError("layout lease identity space is exhausted".into()))?;
        let parent_node = self.node_mut(parent.id)?;
        parent_node
            .owned_panes
            .retain(|pane| !delegated.contains(pane));
        parent_node.child = Some(id);
        self.nodes.insert(
            id,
            LayoutLeaseNode {
                generation: 1,
                parent: Some(parent.id),
                child: None,
                owned_panes: panes.to_vec(),
                accepted: None,
            },
        );
        Ok(LayoutLeaseToken { id, generation: 1 })
    }

    pub fn submit(
        &mut self,
        token: LayoutLeaseToken,
        revision: u64,
        fragment: LayoutFragment,
        focus: Option<u64>,
    ) -> Result<(), LayoutLeaseError> {
        self.validate_token(token)?;
        if revision != self.inventory_revision {
            return Err(LayoutLeaseError(format!(
                "layout result revision {revision} does not match current revision {}",
                self.inventory_revision
            )));
        }
        let node = self.node(token.id)?;
        if fragment.rest.is_some() != node.child.is_some() {
            return Err(LayoutLeaseError(
                "layout fragment rest slot does not match its delegated child".into(),
            ));
        }
        validate_layout_fragment(&fragment, &node.owned_panes)?;
        if let Some(rest) = fragment.rest {
            validate_child_slot(rest)?;
        }
        if let Some(focus) = focus
            && !self.subtree_panes(token.id)?.contains(&focus)
        {
            return Err(LayoutLeaseError(format!(
                "layout lease {} cannot focus pane {focus} outside its subtree",
                token.id.0
            )));
        }
        self.node_mut(token.id)?.accepted = Some(AcceptedLayoutFragment {
            revision,
            fragment,
            focus,
        });
        Ok(())
    }

    pub fn compose(&self) -> Result<ComposedLayout, LayoutLeaseError> {
        let root_viewport = LocalTransform {
            offset: LocalPoint {
                u: NORMALIZED_MIN,
                v: NORMALIZED_MIN,
            },
            size: LocalPoint { u: 2.0, v: 2.0 },
        };
        let (mut entries, focus) = self.compose_node(self.root, 1, root_viewport, root_viewport)?;
        for (index, entry) in entries.iter_mut().enumerate() {
            let z = index.to_f64().unwrap_or(f64::MAX);
            match &mut entry.kind {
                PaintEntryKind::Item(item) => item.z = z,
                PaintEntryKind::Zone(zone) => zone.z = z,
                PaintEntryKind::Divider(divider) => divider.z = z,
                PaintEntryKind::Child(_) => {
                    return Err(LayoutLeaseError(
                        "composed layout retained an unresolved child slot".into(),
                    ));
                }
            }
        }
        let mut items = Vec::new();
        let mut zones = Vec::new();
        let mut dividers = Vec::new();
        for entry in entries {
            match entry.kind {
                PaintEntryKind::Item(item) => items.push(item),
                PaintEntryKind::Zone(zone) => zones.push(zone),
                PaintEntryKind::Divider(divider) => dividers.push(divider),
                PaintEntryKind::Child(_) => {
                    return Err(LayoutLeaseError(
                        "composed layout retained an unresolved child slot".into(),
                    ));
                }
            }
        }
        let layout = ResolvedLayout { items }
            .validate_for_panes(&self.inventory)
            .map_err(|error| LayoutLeaseError(error.to_string()))?;
        Ok(ComposedLayout {
            layout,
            zones,
            dividers,
            focus,
        })
    }

    pub fn collapse_wrapper(
        &mut self,
        token: LayoutLeaseToken,
    ) -> Result<LayoutLeaseToken, LayoutLeaseError> {
        self.validate_token(token)?;
        let node = self.node(token.id)?.clone();
        let child = node.child.ok_or_else(|| {
            LayoutLeaseError("only a layout lease with a rest child can collapse".into())
        })?;
        let promoted_panes = self
            .inventory
            .iter()
            .copied()
            .filter(|pane| {
                node.owned_panes.contains(pane)
                    || self
                        .node(child)
                        .is_ok_and(|child_node| child_node.owned_panes.contains(pane))
            })
            .collect::<Vec<_>>();
        {
            let child_node = self.node_mut(child)?;
            child_node.owned_panes = promoted_panes;
            child_node.generation = child_node
                .generation
                .checked_add(1)
                .ok_or_else(|| LayoutLeaseError("layout lease generation is exhausted".into()))?;
            child_node.parent = node.parent;
        }
        if let Some(parent) = node.parent {
            self.node_mut(parent)?.child = Some(child);
        } else {
            self.root = child;
        }
        self.nodes.remove(&token.id);
        self.token_for(child)
    }

    #[must_use]
    pub fn owned_panes(&self, token: LayoutLeaseToken) -> Option<Vec<u64>> {
        self.validate_token(token).ok()?;
        Some(self.node(token.id).ok()?.owned_panes.clone())
    }

    /// Panes this lease may name in a synchronized wait set. This includes the
    /// lease's direct panes and panes owned by every delegated rest descendant.
    #[must_use]
    pub fn waitable_panes(&self, token: LayoutLeaseToken) -> Option<Vec<u64>> {
        self.validate_token(token).ok()?;
        let subtree = self.subtree_panes(token.id).ok()?;
        Some(
            self.inventory
                .iter()
                .copied()
                .filter(|pane| subtree.contains(pane))
                .collect(),
        )
    }

    #[must_use]
    pub fn lease_tokens(&self) -> Vec<LayoutLeaseToken> {
        self.nodes
            .keys()
            .filter_map(|id| self.token_for(*id).ok())
            .collect()
    }

    #[must_use]
    pub fn has_rest(&self, token: LayoutLeaseToken) -> Option<bool> {
        self.validate_token(token).ok()?;
        Some(self.node(token.id).ok()?.child.is_some())
    }

    #[must_use]
    pub fn rest_token(&self, token: LayoutLeaseToken) -> Option<LayoutLeaseToken> {
        self.validate_token(token).ok()?;
        self.token_for(self.node(token.id).ok()?.child?).ok()
    }

    #[must_use]
    pub fn subtree_contains(&self, token: LayoutLeaseToken, pane: u64) -> bool {
        self.validate_token(token).is_ok()
            && self
                .subtree_panes(token.id)
                .is_ok_and(|panes| panes.contains(&pane))
    }

    pub fn assign_panes(
        &mut self,
        token: LayoutLeaseToken,
        panes: &[u64],
    ) -> Result<(), LayoutLeaseError> {
        self.validate_token(token)?;
        let assigned = panes.iter().copied().collect::<BTreeSet<_>>();
        if assigned.len() != panes.len()
            || !assigned.iter().all(|pane| self.inventory.contains(pane))
        {
            return Err(LayoutLeaseError(
                "layout pane assignment must be a unique subset of current inventory".into(),
            ));
        }
        for node in self.nodes.values_mut() {
            node.owned_panes.retain(|pane| !assigned.contains(pane));
        }
        self.node_mut(token.id)?.owned_panes.extend(panes);
        for node in self.nodes.values_mut() {
            let owned = node.owned_panes.iter().copied().collect::<BTreeSet<_>>();
            node.owned_panes = self
                .inventory
                .iter()
                .copied()
                .filter(|pane| owned.contains(pane))
                .collect();
        }
        Ok(())
    }

    #[must_use]
    pub fn viewport_transform(&self, token: LayoutLeaseToken) -> Option<LocalTransform> {
        self.validate_token(token).ok()?;
        let mut path = Vec::new();
        let mut current = token.id;
        while current != self.root {
            let node = self.node(current).ok()?;
            let parent = node.parent?;
            path.push(parent);
            current = parent;
        }
        let mut transform = LocalTransform {
            offset: LocalPoint {
                u: NORMALIZED_MIN,
                v: NORMALIZED_MIN,
            },
            size: LocalPoint { u: 2.0, v: 2.0 },
        };
        for parent in path.into_iter().rev() {
            let accepted = self.node(parent).ok()?.accepted.as_ref()?;
            if accepted.revision != self.inventory_revision {
                return None;
            }
            let rest = accepted.fragment.rest?;
            transform = transform.compose_child(rest.transform);
        }
        Some(transform)
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.nodes.values().all(|node| {
            node.accepted
                .as_ref()
                .is_some_and(|accepted| accepted.revision == self.inventory_revision)
        })
    }

    fn compose_node(
        &self,
        id: LayoutLeaseId,
        depth: usize,
        viewport: LocalTransform,
        inherited_clip: LocalTransform,
    ) -> Result<(Vec<PaintEntry>, Option<u64>), LayoutLeaseError> {
        if depth > MAX_LAYOUT_LEASE_DEPTH {
            return Err(LayoutLeaseError(format!(
                "layout lease depth exceeds {MAX_LAYOUT_LEASE_DEPTH}"
            )));
        }
        let node = self.node(id)?;
        let accepted = node.accepted.as_ref().ok_or_else(|| {
            LayoutLeaseError(format!("layout lease {} has no accepted fragment", id.0))
        })?;
        if accepted.revision != self.inventory_revision {
            return Err(LayoutLeaseError(format!(
                "layout lease {} has no fragment for revision {}",
                id.0, self.inventory_revision
            )));
        }
        let clip = viewport.intersection(inherited_clip).ok_or_else(|| {
            LayoutLeaseError(format!("layout lease {} has an empty viewport", id.0))
        })?;
        let token = self.token_for(id)?;
        let mut entries = Vec::new();
        for (index, item) in accepted.fragment.layout.items.iter().copied().enumerate() {
            let mut item = item;
            item.transform = viewport
                .compose_child(item.transform)
                .intersection(clip)
                .ok_or_else(|| {
                    LayoutLeaseError(format!(
                        "layout item {} is clipped out of lease {}",
                        item.id.0, id.0
                    ))
                })?;
            entries.push(PaintEntry {
                z: item.z,
                index,
                kind: PaintEntryKind::Item(item),
            });
        }
        let mut next_index = entries.len();
        for zone in &accepted.fragment.zones {
            let transform = viewport
                .compose_child(zone.transform)
                .intersection(clip)
                .ok_or_else(|| {
                    LayoutLeaseError(format!(
                        "interaction zone {} is clipped out of lease {}",
                        zone.id.0, id.0
                    ))
                })?;
            entries.push(PaintEntry {
                z: zone.z,
                index: next_index,
                kind: PaintEntryKind::Zone(ComposedInteractionZone {
                    id: ComposedZoneId {
                        lease: id,
                        lease_generation: token.generation,
                        local_item: zone.id,
                    },
                    transform,
                    lease_viewport: viewport,
                    z: zone.z,
                    event_mask: zone.event_mask,
                    cursor: zone.cursor,
                    input: zone.input,
                }),
            });
            next_index += 1;
        }
        for divider in &accepted.fragment.dividers {
            let transform = viewport
                .compose_child(divider.transform)
                .intersection(clip)
                .ok_or_else(|| {
                    LayoutLeaseError(format!(
                        "layout divider {} is clipped out of lease {}",
                        divider.id.0, id.0
                    ))
                })?;
            entries.push(PaintEntry {
                z: divider.z,
                index: next_index,
                kind: PaintEntryKind::Divider(ComposedDivider {
                    id: ComposedDividerId {
                        lease: id,
                        lease_generation: token.generation,
                        local_item: divider.id,
                    },
                    zone: ComposedZoneId {
                        lease: id,
                        lease_generation: token.generation,
                        local_item: divider.zone,
                    },
                    z: divider.z,
                    transform,
                    axis: divider.axis,
                    role: divider.role,
                }),
            });
            next_index += 1;
        }
        if let Some(rest) = accepted.fragment.rest {
            entries.push(PaintEntry {
                z: rest.z,
                index: next_index,
                kind: PaintEntryKind::Child(rest),
            });
        }
        entries.sort_by(|left, right| {
            left.z
                .total_cmp(&right.z)
                .then_with(|| left.index.cmp(&right.index))
        });
        let mut composed = Vec::new();
        let mut child_focus = None;
        for entry in entries {
            match entry.kind {
                PaintEntryKind::Item(_) | PaintEntryKind::Zone(_) | PaintEntryKind::Divider(_) => {
                    composed.push(entry);
                }
                PaintEntryKind::Child(slot) => {
                    let child = node.child.ok_or_else(|| {
                        LayoutLeaseError(format!(
                            "layout lease {} supplied a rest slot without a child",
                            id.0
                        ))
                    })?;
                    let child_viewport = viewport.compose_child(slot.transform);
                    let child_clip = child_viewport.intersection(clip).ok_or_else(|| {
                        LayoutLeaseError(format!(
                            "layout lease {} rest slot has an empty viewport",
                            id.0
                        ))
                    })?;
                    let (child_entries, focus) =
                        self.compose_node(child, depth + 1, child_viewport, child_clip)?;
                    composed.extend(child_entries);
                    child_focus = focus;
                }
            }
        }
        Ok((composed, accepted.focus.or(child_focus)))
    }

    fn subtree_panes(&self, id: LayoutLeaseId) -> Result<BTreeSet<u64>, LayoutLeaseError> {
        let node = self.node(id)?;
        let mut panes = node.owned_panes.iter().copied().collect::<BTreeSet<_>>();
        if let Some(child) = node.child {
            panes.extend(self.subtree_panes(child)?);
        }
        Ok(panes)
    }

    fn deepest_lease(&self) -> Result<LayoutLeaseId, LayoutLeaseError> {
        let mut current = self.root;
        for _ in 0..MAX_LAYOUT_LEASE_DEPTH {
            let Some(child) = self.node(current)?.child else {
                return Ok(current);
            };
            current = child;
        }
        Err(LayoutLeaseError(format!(
            "layout lease depth exceeds {MAX_LAYOUT_LEASE_DEPTH}"
        )))
    }

    fn depth(&self, id: LayoutLeaseId) -> Result<usize, LayoutLeaseError> {
        let mut current = Some(id);
        let mut depth = 0;
        while let Some(id) = current {
            depth += 1;
            if depth > MAX_LAYOUT_LEASE_DEPTH {
                return Err(LayoutLeaseError(format!(
                    "layout lease depth exceeds {MAX_LAYOUT_LEASE_DEPTH}"
                )));
            }
            current = self.node(id)?.parent;
        }
        Ok(depth)
    }

    fn validate_token(&self, token: LayoutLeaseToken) -> Result<(), LayoutLeaseError> {
        let node = self.node(token.id)?;
        if token.generation != node.generation {
            return Err(LayoutLeaseError(format!(
                "stale layout lease generation {} for lease {}; current generation is {}",
                token.generation, token.id.0, node.generation
            )));
        }
        Ok(())
    }

    fn token_for(&self, id: LayoutLeaseId) -> Result<LayoutLeaseToken, LayoutLeaseError> {
        Ok(LayoutLeaseToken {
            id,
            generation: self.node(id)?.generation,
        })
    }

    fn node(&self, id: LayoutLeaseId) -> Result<&LayoutLeaseNode, LayoutLeaseError> {
        self.nodes
            .get(&id)
            .ok_or_else(|| LayoutLeaseError(format!("unknown layout lease {}", id.0)))
    }

    fn node_mut(&mut self, id: LayoutLeaseId) -> Result<&mut LayoutLeaseNode, LayoutLeaseError> {
        self.nodes
            .get_mut(&id)
            .ok_or_else(|| LayoutLeaseError(format!("unknown layout lease {}", id.0)))
    }
}

#[derive(Debug, Clone, Copy)]
struct PaintEntry {
    z: f64,
    index: usize,
    kind: PaintEntryKind,
}

#[derive(Debug, Clone, Copy)]
enum PaintEntryKind {
    Item(ResolvedLayoutItem),
    Zone(ComposedInteractionZone),
    Divider(ComposedDivider),
    Child(LayoutChildSlot),
}

fn validate_layout_fragment(
    fragment: &LayoutFragment,
    valid_panes: &[u64],
) -> Result<(), LayoutLeaseError> {
    fragment
        .layout
        .clone()
        .validate_for_panes(valid_panes)
        .map_err(|error| LayoutLeaseError(error.to_string()))?;
    if fragment.zones.len() > MAX_INTERACTION_ZONES {
        return Err(LayoutLeaseError(format!(
            "interaction zone count exceeds {MAX_INTERACTION_ZONES}"
        )));
    }
    if fragment.dividers.len() > MAX_LAYOUT_DIVIDERS {
        return Err(LayoutLeaseError(format!(
            "layout divider count exceeds {MAX_LAYOUT_DIVIDERS}"
        )));
    }

    // All actor-local layout identities share one namespace. This prevents a
    // zone or divider from being confused with a render item when messages
    // refer to the actor's local item identity.
    let mut local_ids = fragment
        .layout
        .items
        .iter()
        .map(|item| item.id)
        .collect::<BTreeSet<_>>();
    let mut zone_ids = BTreeSet::new();
    for zone in &fragment.zones {
        if zone.id.0 == 0 {
            return Err(LayoutLeaseError(
                "interaction zone IDs must be non-zero".into(),
            ));
        }
        if !local_ids.insert(zone.id) {
            return Err(LayoutLeaseError(format!(
                "layout local item id {} appears more than once",
                zone.id.0
            )));
        }
        zone_ids.insert(zone.id);
        if !zone.z.is_finite() {
            return Err(LayoutLeaseError(format!(
                "interaction zone {} has non-finite z",
                zone.id.0
            )));
        }
        zone.transform.validate(zone.id).map_err(|error| {
            LayoutLeaseError(format!("invalid interaction zone {}: {error}", zone.id.0))
        })?;
        if !zone.event_mask.is_valid() {
            return Err(LayoutLeaseError(format!(
                "interaction zone {} has an empty or unsupported pointer event mask",
                zone.id.0
            )));
        }
    }
    for divider in &fragment.dividers {
        if divider.id.0 == 0 {
            return Err(LayoutLeaseError(
                "layout divider IDs must be non-zero".into(),
            ));
        }
        if !local_ids.insert(divider.id) {
            return Err(LayoutLeaseError(format!(
                "layout local item id {} appears more than once",
                divider.id.0
            )));
        }
        if !divider.z.is_finite() {
            return Err(LayoutLeaseError(format!(
                "layout divider {} has non-finite z",
                divider.id.0
            )));
        }
        divider.transform.validate(divider.id).map_err(|error| {
            LayoutLeaseError(format!("invalid layout divider {}: {error}", divider.id.0))
        })?;
        if !zone_ids.contains(&divider.zone) {
            return Err(LayoutLeaseError(format!(
                "layout divider {} references unknown interaction zone {}",
                divider.id.0, divider.zone.0
            )));
        }
    }
    Ok(())
}

fn validate_child_slot(slot: LayoutChildSlot) -> Result<(), LayoutLeaseError> {
    if !slot.z.is_finite() {
        return Err(LayoutLeaseError(
            "layout fragment rest slot has non-finite z".into(),
        ));
    }
    slot.transform
        .validate(LayoutItemId(1))
        .map_err(|error| LayoutLeaseError(format!("invalid layout rest slot: {error}")))
}

impl fmt::Display for LayoutLeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LayoutLeaseError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(transparent)]
pub struct LayoutItemId(pub u64);

/// Identity of an interaction zone after recursive lease composition.
///
/// Layout item IDs are local to an actor's lease. Including the lease and its
/// generation keeps the identity stable across layout submissions while also
/// preventing a renewed lease from inheriting an old pointer capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ComposedZoneId {
    pub lease: LayoutLeaseId,
    pub lease_generation: u64,
    pub local_item: LayoutItemId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ComposedDividerId {
    pub lease: LayoutLeaseId,
    pub lease_generation: u64,
    pub local_item: LayoutItemId,
}

/// Pointer event classes an interaction zone asks to receive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(transparent)]
pub struct PointerEventMask(u8);

impl PointerEventMask {
    pub const BUTTON: Self = Self(1 << 0);
    pub const BUTTON_PRESS: Self = Self::BUTTON;
    pub const MOTION: Self = Self(1 << 1);
    pub const RELEASE: Self = Self(1 << 2);
    pub const CANCEL: Self = Self(1 << 3);
    pub const SUPPORTED: Self =
        Self(Self::BUTTON.0 | Self::MOTION.0 | Self::RELEASE.0 | Self::CANCEL.0);

    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn from_bits(bits: u8) -> Option<Self> {
        if bits & !Self::SUPPORTED.0 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// Preserves raw wire bits so normal fragment validation can reject an
    /// unsupported mask without the conversion layer silently changing it.
    #[must_use]
    pub const fn from_bits_retain(bits: u8) -> Self {
        Self(bits)
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    const fn is_valid(self) -> bool {
        !self.is_empty() && self.0 & !Self::SUPPORTED.0 == 0
    }
}

impl std::ops::BitOr for PointerEventMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for PointerEventMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CursorRole {
    Default,
    Pointer,
    ColResize,
    RowResize,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ZoneInputBehavior {
    Hit,
    Drag,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq)]
pub struct InteractionZone {
    pub id: LayoutItemId,
    pub transform: LocalTransform,
    pub z: f64,
    pub event_mask: PointerEventMask,
    pub cursor: CursorRole,
    pub input: ZoneInputBehavior,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ComposedInteractionZone {
    pub id: ComposedZoneId,
    /// Zone geometry in the composed presentation viewport, clipped to every
    /// lease viewport on its path from the root.
    pub transform: LocalTransform,
    /// Owning lease viewport in composed presentation coordinates.
    pub lease_viewport: LocalTransform,
    pub z: f64,
    pub event_mask: PointerEventMask,
    pub cursor: CursorRole,
    pub input: ZoneInputBehavior,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DividerAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DividerRole {
    Split,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq)]
pub struct LayoutDivider {
    pub id: LayoutItemId,
    pub zone: LayoutItemId,
    pub z: f64,
    pub transform: LocalTransform,
    pub axis: DividerAxis,
    pub role: DividerRole,
}

impl LayoutDivider {
    #[must_use]
    pub fn pixel_rect(self, size: LayoutSize) -> LayoutRect {
        self.transform.pixel_rect(size)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ComposedDivider {
    pub id: ComposedDividerId,
    pub zone: ComposedZoneId,
    pub z: f64,
    pub transform: LocalTransform,
    pub axis: DividerAxis,
    pub role: DividerRole,
}

impl ComposedDivider {
    #[must_use]
    pub fn pixel_rect(self, size: LayoutSize) -> LayoutRect {
        self.transform.pixel_rect(size)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoneHitResult {
    pub zone: ComposedZoneId,
    pub local_position: LocalPoint,
    pub event_mask: PointerEventMask,
    pub cursor: CursorRole,
    pub input: ZoneInputBehavior,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq)]
pub struct ResolvedLayoutItem {
    pub id: LayoutItemId,
    pub kind: LayoutItemKind,
    pub z: f64,
    pub transform: LocalTransform,
    pub input: InputBehavior,
}

impl ResolvedLayoutItem {
    #[must_use]
    pub fn pixel_rect(self, size: LayoutSize) -> LayoutRect {
        self.transform.pixel_rect(size)
    }
}

#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LayoutItemKind {
    Pane { pane: u64 },
    Placeholder,
    Handle,
    Chrome,
    Overlay,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InputBehavior {
    None,
    Hit,
    Drag,
    Pane,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq)]
pub struct LocalPoint {
    pub u: f64,
    pub v: f64,
}

impl LocalPoint {
    #[must_use]
    pub fn from_pixel_in_rect(x: f64, y: f64, rect: LayoutRect) -> Self {
        Self {
            u: ((x - f64::from(rect.x)) / f64::from(rect.w.max(1))).mul_add(2.0, -1.0),
            v: ((y - f64::from(rect.y)) / f64::from(rect.h.max(1))).mul_add(2.0, -1.0),
        }
    }

    const fn is_finite(self) -> bool {
        self.u.is_finite() && self.v.is_finite()
    }
}

#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq)]
pub struct LocalTransform {
    pub offset: LocalPoint,
    pub size: LocalPoint,
}

impl LocalTransform {
    #[must_use]
    pub fn from_pixel_rect(rect: LayoutRect, bounds: LayoutSize) -> Self {
        let bounds_w = f64::from(bounds.w.max(1));
        let bounds_h = f64::from(bounds.h.max(1));
        Self {
            offset: LocalPoint {
                u: (f64::from(rect.x) / bounds_w).mul_add(2.0, -1.0),
                v: (f64::from(rect.y) / bounds_h).mul_add(2.0, -1.0),
            },
            size: LocalPoint {
                u: (f64::from(rect.w.max(1)) / bounds_w) * 2.0,
                v: (f64::from(rect.h.max(1)) / bounds_h) * 2.0,
            },
        }
    }

    fn validate(self, id: LayoutItemId) -> Result<(), ResolvedLayoutError> {
        if !self.offset.is_finite() || !self.size.is_finite() {
            return Err(ResolvedLayoutError(format!(
                "resolved layout item {} has non-finite transform",
                id.0
            )));
        }
        if self.size.u <= 0.0 || self.size.v <= 0.0 {
            return Err(ResolvedLayoutError(format!(
                "resolved layout item {} has non-positive size",
                id.0
            )));
        }
        let max = LocalPoint {
            u: self.offset.u + self.size.u,
            v: self.offset.v + self.size.v,
        };
        if self.offset.u < NORMALIZED_MIN
            || self.offset.v < NORMALIZED_MIN
            || max.u > NORMALIZED_MAX
            || max.v > NORMALIZED_MAX
        {
            return Err(ResolvedLayoutError(format!(
                "resolved layout item {} is outside normalized presentation bounds",
                id.0
            )));
        }
        Ok(())
    }

    fn project_point(self, point: LocalPoint) -> Option<LocalPoint> {
        if !point.is_finite()
            || point.u < self.offset.u
            || point.v < self.offset.v
            || point.u >= self.offset.u + self.size.u
            || point.v >= self.offset.v + self.size.v
        {
            return None;
        }
        Some(LocalPoint {
            u: ((point.u - self.offset.u) / self.size.u).mul_add(2.0, -1.0),
            v: ((point.v - self.offset.v) / self.size.v).mul_add(2.0, -1.0),
        })
    }

    fn contains(self, point: LocalPoint) -> bool {
        self.project_point(point).is_some()
    }

    fn intersection(self, other: Self) -> Option<Self> {
        let min_u = self.offset.u.max(other.offset.u);
        let min_v = self.offset.v.max(other.offset.v);
        let max_u = (self.offset.u + self.size.u).min(other.offset.u + other.size.u);
        let max_v = (self.offset.v + self.size.v).min(other.offset.v + other.size.v);
        if !min_u.is_finite()
            || !min_v.is_finite()
            || !max_u.is_finite()
            || !max_v.is_finite()
            || max_u <= min_u
            || max_v <= min_v
        {
            return None;
        }
        Some(Self {
            offset: LocalPoint { u: min_u, v: min_v },
            size: LocalPoint {
                u: max_u - min_u,
                v: max_v - min_v,
            },
        })
    }

    #[must_use]
    pub fn compose_child(self, child: Self) -> Self {
        Self {
            offset: LocalPoint {
                u: self
                    .size
                    .u
                    .mul_add((child.offset.u + 1.0) * 0.5, self.offset.u),
                v: self
                    .size
                    .v
                    .mul_add((child.offset.v + 1.0) * 0.5, self.offset.v),
            },
            size: LocalPoint {
                u: self.size.u * child.size.u * 0.5,
                v: self.size.v * child.size.v * 0.5,
            },
        }
    }

    #[must_use]
    pub fn pixel_rect(self, size: LayoutSize) -> LayoutRect {
        let bounds_w = size.w.max(0);
        let bounds_h = size.h.max(0);
        let w = f64::from(bounds_w);
        let h = f64::from(bounds_h);
        LayoutRect {
            x: rounded_layout_coordinate(((self.offset.u + 1.0) * 0.5) * w),
            y: rounded_layout_coordinate(((self.offset.v + 1.0) * 0.5) * h),
            w: rounded_layout_extent((self.size.u * 0.5) * w, bounds_w),
            h: rounded_layout_extent((self.size.v * 0.5) * h, bounds_h),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HitResult {
    pub item: LayoutItemId,
    pub kind: LayoutItemKind,
    pub local_point: LocalPoint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLayoutError(String);

impl fmt::Display for ResolvedLayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ResolvedLayoutError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutAction {
    FocusNext,
    FocusPrevious,
    CloseActivePane,
    ToggleManagedFullscreen,
    SplitActiveRight,
    SplitActiveDown,
    JoinActivePane,
    SplitActiveSmart,
    FocusNextSplit,
    FocusPreviousSplit,
    MoveActiveToLeftDock,
    MoveActiveToRightDock,
    MoveActiveToBottomDock,
    MoveActiveToCenter,
    ToggleLeftDock,
    ToggleRightDock,
    ToggleBottomDock,
    IncreaseLeftDockWidth,
    DecreaseLeftDockWidth,
}

impl LayoutAction {
    #[allow(
        dead_code,
        reason = "the legacy ABI decoder remains part of the public compatibility surface"
    )]
    #[must_use]
    pub const fn from_abi(value: i32) -> Option<Self> {
        Some(match value {
            1 => Self::FocusNext,
            2 => Self::FocusPrevious,
            3 => Self::CloseActivePane,
            4 => Self::ToggleManagedFullscreen,
            5 => Self::SplitActiveRight,
            6 => Self::SplitActiveDown,
            7 => Self::JoinActivePane,
            8 => Self::MoveActiveToLeftDock,
            9 => Self::MoveActiveToRightDock,
            10 => Self::MoveActiveToBottomDock,
            11 => Self::MoveActiveToCenter,
            12 => Self::ToggleLeftDock,
            13 => Self::ToggleRightDock,
            14 => Self::ToggleBottomDock,
            15 => Self::IncreaseLeftDockWidth,
            16 => Self::DecreaseLeftDockWidth,
            17 => Self::SplitActiveSmart,
            18 => Self::FocusNextSplit,
            19 => Self::FocusPreviousSplit,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::FocusNext => "focus-next",
            Self::FocusPrevious => "focus-previous",
            Self::CloseActivePane => "close-active-pane",
            Self::ToggleManagedFullscreen => "toggle-managed-fullscreen",
            Self::SplitActiveRight => "split-active-right",
            Self::SplitActiveDown => "split-active-down",
            Self::JoinActivePane => "join-active-pane",
            Self::SplitActiveSmart => "split-active-smart",
            Self::FocusNextSplit => "focus-next-split",
            Self::FocusPreviousSplit => "focus-previous-split",
            Self::MoveActiveToLeftDock => "move-active-to-left-dock",
            Self::MoveActiveToRightDock => "move-active-to-right-dock",
            Self::MoveActiveToBottomDock => "move-active-to-bottom-dock",
            Self::MoveActiveToCenter => "move-active-to-center",
            Self::ToggleLeftDock => "toggle-left-dock",
            Self::ToggleRightDock => "toggle-right-dock",
            Self::ToggleBottomDock => "toggle-bottom-dock",
            Self::IncreaseLeftDockWidth => "increase-left-dock-width",
            Self::DecreaseLeftDockWidth => "decrease-left-dock-width",
        }
    }
}

impl fmt::Display for LayoutAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for LayoutAction {
    type Err = LayoutActionParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "focus-next" => Self::FocusNext,
            "focus-previous" | "focus-prev" => Self::FocusPrevious,
            "close-active-pane" | "close-active-window" => Self::CloseActivePane,
            "toggle-managed-fullscreen" | "toggle-fullscreen" => Self::ToggleManagedFullscreen,
            "split-active-right" | "split-right" => Self::SplitActiveRight,
            "split-active-down" | "split-down" | "split-active-vertical" | "split-vertical" => {
                Self::SplitActiveDown
            }
            "join-active-pane"
            | "join-pane"
            | "remove-active-split"
            | "remove-split"
            | "unsplit" => Self::JoinActivePane,
            "split-active-smart" | "split-smart" | "smart-split" => Self::SplitActiveSmart,
            "focus-next-split" | "focus-split-next" => Self::FocusNextSplit,
            "focus-previous-split"
            | "focus-prev-split"
            | "focus-split-previous"
            | "focus-split-prev" => Self::FocusPreviousSplit,
            "move-active-to-left-dock" => Self::MoveActiveToLeftDock,
            "move-active-to-right-dock" => Self::MoveActiveToRightDock,
            "move-active-to-bottom-dock" => Self::MoveActiveToBottomDock,
            "move-active-to-center" => Self::MoveActiveToCenter,
            "toggle-left-dock" => Self::ToggleLeftDock,
            "toggle-right-dock" => Self::ToggleRightDock,
            "toggle-bottom-dock" => Self::ToggleBottomDock,
            "increase-left-dock-width" | "left-dock-width-up" => Self::IncreaseLeftDockWidth,
            "decrease-left-dock-width" | "left-dock-width-down" => Self::DecreaseLeftDockWidth,
            _ => return Err(LayoutActionParseError),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutActionParseError;

impl fmt::Display for LayoutActionParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown layout action")
    }
}

impl std::error::Error for LayoutActionParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutState {
    pub left_dock_visible: bool,
    pub right_dock_visible: bool,
    pub bottom_dock_visible: bool,
    pub center_visible: bool,
    root: LayoutNode,
    active_leaf: LayoutLeafId,
    next_leaf: u64,
}

impl Default for LayoutState {
    fn default() -> Self {
        let active_leaf = LayoutLeafId(1);
        Self {
            left_dock_visible: false,
            right_dock_visible: false,
            bottom_dock_visible: false,
            center_visible: true,
            root: LayoutNode::Leaf(LayoutLeaf {
                id: active_leaf,
                kind: LayoutLeafKind::Client,
            }),
            active_leaf,
            next_leaf: 2,
        }
    }
}

impl LayoutState {
    pub fn split_active(&mut self, axis: LayoutAxis) -> Option<LayoutLeafId> {
        let new_leaf = LayoutLeaf {
            id: self.allocate_leaf(),
            kind: LayoutLeafKind::Client,
        };
        let new_leaf_id = new_leaf.id;
        self.root
            .split_leaf(self.active_leaf, axis, new_leaf)
            .then_some(new_leaf_id)
    }

    #[must_use]
    pub const fn active_leaf(&self) -> LayoutLeafId {
        self.active_leaf
    }

    pub fn set_active_leaf(&mut self, leaf: LayoutLeafId) -> bool {
        if self.root.leaf_kind(leaf).is_none() {
            return false;
        }
        self.active_leaf = leaf;
        true
    }

    pub fn focus_leaf_by_offset(&mut self, offset: isize) -> Option<LayoutLeafId> {
        let leaves = self.leaf_ids();
        let leaf_count = leaves.len();
        if leaf_count == 0 {
            return None;
        }
        let active_index = leaves
            .iter()
            .position(|leaf| *leaf == self.active_leaf)
            .unwrap_or(0);
        let step = offset.unsigned_abs() % leaf_count;
        let next_index = if offset.is_negative() {
            active_index
                .checked_sub(step)
                .unwrap_or_else(|| leaf_count - (step - active_index))
        } else if step < leaf_count - active_index {
            active_index + step
        } else {
            step - (leaf_count - active_index)
        };
        self.active_leaf = *leaves.get(next_index)?;
        Some(self.active_leaf)
    }

    pub fn remove_active_leaf(&mut self) -> Option<(LayoutLeafId, LayoutLeafId)> {
        let removed_leaf = self.active_leaf;
        let next_active_leaf = self.root.remove_leaf(removed_leaf)?;
        self.active_leaf = next_active_leaf;
        Some((removed_leaf, next_active_leaf))
    }

    pub fn remove_leaf(&mut self, leaf: LayoutLeafId) -> Option<(LayoutLeafId, LayoutLeafId)> {
        let next_active_leaf = self.root.remove_leaf(leaf)?;
        if self.active_leaf == leaf {
            self.active_leaf = next_active_leaf;
        }
        Some((leaf, next_active_leaf))
    }

    #[must_use]
    pub fn active_rect(&self, size: LayoutSize) -> Option<LayoutRect> {
        self.leaf_rect(self.active_leaf, size)
    }

    #[must_use]
    pub fn leaf_rect(&self, leaf: LayoutLeafId, size: LayoutSize) -> Option<LayoutRect> {
        self.resolve(size)
            .into_iter()
            .find(|region| region.leaf == leaf)
            .map(|region| region.rect)
    }

    #[must_use]
    pub fn active_kind(&self) -> Option<LayoutLeafKind> {
        self.root.leaf_kind(self.active_leaf)
    }

    #[must_use]
    pub fn resolve(&self, size: LayoutSize) -> Vec<LayoutRegion> {
        let mut regions = Vec::new();
        self.root.resolve(
            LayoutRect {
                x: 0,
                y: 0,
                w: size.w.max(0),
                h: size.h.max(0),
            },
            self.active_leaf,
            &mut regions,
        );
        regions
    }

    #[must_use]
    pub fn leaf_ids(&self) -> Vec<LayoutLeafId> {
        let mut leaves = Vec::new();
        self.root.collect_leaf_ids(&mut leaves);
        leaves
    }

    const fn allocate_leaf(&mut self) -> LayoutLeafId {
        let id = LayoutLeafId(self.next_leaf);
        self.next_leaf += 1;
        id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutSize {
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl LayoutRect {
    #[must_use]
    pub fn contains_f64(self, x: f64, y: f64) -> bool {
        let max_x = f64::from(self.x) + f64::from(self.w.max(0));
        let max_y = f64::from(self.y) + f64::from(self.h.max(0));
        x >= f64::from(self.x) && y >= f64::from(self.y) && x < max_x && y < max_y
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutRegion {
    pub leaf: LayoutLeafId,
    pub rect: LayoutRect,
    pub kind: LayoutLeafKind,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutLeafId(u64);

impl LayoutLeafId {
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutLeafKind {
    Client,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LayoutLeaf {
    id: LayoutLeafId,
    kind: LayoutLeafKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LayoutNode {
    Leaf(LayoutLeaf),
    Split {
        axis: LayoutAxis,
        first: Box<Self>,
        second: Box<Self>,
    },
}

impl LayoutNode {
    fn leaf_kind(&self, target: LayoutLeafId) -> Option<LayoutLeafKind> {
        match self {
            Self::Leaf(leaf) if leaf.id == target => Some(leaf.kind),
            Self::Leaf(_) => None,
            Self::Split { first, second, .. } => {
                first.leaf_kind(target).or_else(|| second.leaf_kind(target))
            }
        }
    }

    fn split_leaf(&mut self, target: LayoutLeafId, axis: LayoutAxis, new_leaf: LayoutLeaf) -> bool {
        match self {
            Self::Leaf(leaf) if leaf.id == target => {
                let current = Self::Leaf(leaf.clone());
                let new = Self::Leaf(new_leaf);
                *self = Self::Split {
                    axis,
                    first: Box::new(current),
                    second: Box::new(new),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split_leaf(target, axis, new_leaf.clone())
                    || second.split_leaf(target, axis, new_leaf)
            }
        }
    }

    fn remove_leaf(&mut self, target: LayoutLeafId) -> Option<LayoutLeafId> {
        match self {
            Self::Leaf(_) => None,
            Self::Split { first, second, .. } => {
                if first.is_leaf(target) {
                    let replacement = (**second).clone();
                    let next_active_leaf = replacement.first_leaf_id();
                    *self = replacement;
                    return Some(next_active_leaf);
                }
                if second.is_leaf(target) {
                    let replacement = (**first).clone();
                    let next_active_leaf = replacement.first_leaf_id();
                    *self = replacement;
                    return Some(next_active_leaf);
                }
                first
                    .remove_leaf(target)
                    .or_else(|| second.remove_leaf(target))
            }
        }
    }

    fn is_leaf(&self, target: LayoutLeafId) -> bool {
        matches!(self, Self::Leaf(leaf) if leaf.id == target)
    }

    fn first_leaf_id(&self) -> LayoutLeafId {
        match self {
            Self::Leaf(leaf) => leaf.id,
            Self::Split { first, .. } => first.first_leaf_id(),
        }
    }

    fn collect_leaf_ids(&self, leaves: &mut Vec<LayoutLeafId>) {
        match self {
            Self::Leaf(leaf) => leaves.push(leaf.id),
            Self::Split { first, second, .. } => {
                first.collect_leaf_ids(leaves);
                second.collect_leaf_ids(leaves);
            }
        }
    }

    fn resolve(
        &self,
        rect: LayoutRect,
        active_leaf: LayoutLeafId,
        regions: &mut Vec<LayoutRegion>,
    ) {
        match self {
            Self::Leaf(leaf) => regions.push(LayoutRegion {
                leaf: leaf.id,
                rect,
                kind: leaf.kind,
                active: leaf.id == active_leaf,
            }),
            Self::Split {
                axis,
                first,
                second,
            } => {
                let (first_rect, second_rect) = split_rect(rect, *axis);
                first.resolve(first_rect, active_leaf, regions);
                second.resolve(second_rect, active_leaf, regions);
            }
        }
    }
}

const fn split_rect(rect: LayoutRect, axis: LayoutAxis) -> (LayoutRect, LayoutRect) {
    match axis {
        LayoutAxis::Horizontal => {
            let first_w = rect.w / 2;
            let second_w = rect.w - first_w;
            (
                LayoutRect { w: first_w, ..rect },
                LayoutRect {
                    x: rect.x.saturating_add(first_w),
                    w: second_w,
                    ..rect
                },
            )
        }
        LayoutAxis::Vertical => {
            let first_h = rect.h / 2;
            let second_h = rect.h - first_h;
            (
                LayoutRect { h: first_h, ..rect },
                LayoutRect {
                    y: rect.y.saturating_add(first_h),
                    h: second_h,
                    ..rect
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic_in_result_fn,
        reason = "test setup uses Result propagation while assertions still report behavioral mismatches"
    )]

    use super::*;

    fn transform(u: f64, v: f64, width: f64, height: f64) -> LocalTransform {
        LocalTransform {
            offset: LocalPoint { u, v },
            size: LocalPoint {
                u: width,
                v: height,
            },
        }
    }

    fn full_transform() -> LocalTransform {
        transform(-1.0, -1.0, 2.0, 2.0)
    }

    fn pane_item(id: u64, pane: u64, z: f64) -> ResolvedLayoutItem {
        ResolvedLayoutItem {
            id: LayoutItemId(id),
            kind: LayoutItemKind::Pane { pane },
            z,
            transform: full_transform(),
            input: InputBehavior::Pane,
        }
    }

    fn zone(
        id: u64,
        z: f64,
        transform: LocalTransform,
        event_mask: PointerEventMask,
    ) -> InteractionZone {
        InteractionZone {
            id: LayoutItemId(id),
            transform,
            z,
            event_mask,
            cursor: CursorRole::ColResize,
            input: ZoneInputBehavior::Drag,
        }
    }

    fn divider(id: u64, zone: u64, z: f64, transform: LocalTransform) -> LayoutDivider {
        LayoutDivider {
            id: LayoutItemId(id),
            zone: LayoutItemId(zone),
            z,
            transform,
            axis: DividerAxis::Vertical,
            role: DividerRole::Split,
        }
    }

    fn empty_fragment() -> LayoutFragment {
        LayoutFragment {
            layout: ResolvedLayout::empty(),
            zones: Vec::new(),
            dividers: Vec::new(),
            rest: None,
        }
    }

    fn test_error(message: &str) -> LayoutLeaseError {
        LayoutLeaseError(message.into())
    }

    #[test]
    fn nested_zones_and_dividers_are_transformed_clipped_and_stable() -> Result<(), LayoutLeaseError>
    {
        let mut tree = LayoutLeaseTree::default();
        tree.update_inventory(1, &[11, 22])?;
        let root = tree.root_token();
        let child = tree.grant_rest(root, &[22])?;

        tree.submit(
            root,
            1,
            LayoutFragment {
                layout: ResolvedLayout {
                    items: vec![pane_item(1, 11, -10.0)],
                },
                zones: vec![zone(10, -5.0, full_transform(), PointerEventMask::MOTION)],
                dividers: Vec::new(),
                rest: Some(LayoutChildSlot {
                    z: 0.0,
                    transform: transform(0.0, -1.0, 1.0, 2.0),
                }),
            },
            None,
        )?;
        let child_fragment = LayoutFragment {
            layout: ResolvedLayout {
                items: vec![pane_item(2, 22, 0.0)],
            },
            zones: vec![zone(
                10,
                1.0,
                transform(-1.0, -1.0, 1.0, 2.0),
                PointerEventMask::BUTTON | PointerEventMask::MOTION,
            )],
            dividers: vec![divider(11, 10, 2.0, transform(-0.02, -1.0, 0.04, 2.0))],
            rest: None,
        };
        tree.submit(child, 1, child_fragment.clone(), None)?;

        let composed = tree.compose()?;
        let child_zone = composed
            .zones
            .iter()
            .find(|zone| zone.id.lease == child.id)
            .ok_or_else(|| test_error("composed child interaction zone is missing"))?;
        let expected_id = ComposedZoneId {
            lease: child.id,
            lease_generation: child.generation,
            local_item: LayoutItemId(10),
        };
        assert_eq!(child_zone.id, expected_id);
        assert_eq!(child_zone.transform, transform(0.0, -1.0, 0.5, 2.0));
        assert_eq!(child_zone.lease_viewport, transform(0.0, -1.0, 1.0, 2.0));

        let child_hit = composed
            .zone_hit_stack_for_event(LocalPoint { u: 0.25, v: 0.0 }, PointerEventMask::MOTION)
            .into_iter()
            .find(|hit| hit.zone == expected_id)
            .ok_or_else(|| test_error("composed child interaction zone was not hit"))?;
        assert_eq!(child_hit.local_position, LocalPoint { u: -0.5, v: 0.0 });
        assert!(
            composed
                .zone_hit_stack(LocalPoint { u: -0.25, v: 0.0 })
                .into_iter()
                .all(|hit| hit.zone != expected_id),
            "a child zone must be clipped to its rest viewport"
        );
        assert!(
            composed
                .zone_hit_stack(LocalPoint { u: 0.75, v: 0.0 })
                .into_iter()
                .all(|hit| hit.zone != expected_id),
            "the hit region must retain the zone's own bounds inside the lease"
        );

        let child_divider = composed
            .dividers
            .iter()
            .find(|divider| divider.id.lease == child.id)
            .ok_or_else(|| test_error("composed child divider is missing"))?;
        assert_eq!(child_divider.zone, expected_id);
        assert_eq!(child_divider.axis, DividerAxis::Vertical);
        assert_eq!(child_divider.role, DividerRole::Split);
        assert_eq!(
            child_divider.transform.offset,
            LocalPoint { u: 0.49, v: -1.0 }
        );
        assert!((child_divider.transform.size.u - 0.02).abs() < f64::EPSILON);
        assert!((child_divider.transform.size.v - 2.0).abs() < f64::EPSILON);

        tree.submit(child, 1, child_fragment, None)?;
        let recomposed = tree.compose()?;
        let resubmitted_id = recomposed
            .zones
            .into_iter()
            .find(|zone| zone.id.lease == child.id)
            .ok_or_else(|| test_error("resubmitted child interaction zone is missing"))?
            .id;
        assert_eq!(resubmitted_id, expected_id);
        Ok(())
    }

    #[test]
    fn zone_hit_order_respects_parent_rest_layering_and_event_masks() -> Result<(), LayoutLeaseError>
    {
        let mut tree = LayoutLeaseTree::default();
        tree.update_inventory(1, &[1, 2])?;
        let root = tree.root_token();
        let child = tree.grant_rest(root, &[2])?;
        tree.submit(
            root,
            1,
            LayoutFragment {
                layout: ResolvedLayout {
                    items: vec![pane_item(1, 1, -30.0)],
                },
                zones: vec![
                    zone(10, -20.0, full_transform(), PointerEventMask::MOTION),
                    zone(11, 20.0, full_transform(), PointerEventMask::BUTTON),
                ],
                dividers: Vec::new(),
                rest: Some(LayoutChildSlot {
                    z: 0.0,
                    transform: transform(0.0, -1.0, 1.0, 2.0),
                }),
            },
            None,
        )?;
        tree.submit(
            child,
            1,
            LayoutFragment {
                layout: ResolvedLayout {
                    items: vec![pane_item(2, 2, -1.0)],
                },
                zones: vec![zone(10, 1000.0, full_transform(), PointerEventMask::MOTION)],
                dividers: Vec::new(),
                rest: None,
            },
            None,
        )?;

        let composed = tree.compose()?;
        let point = LocalPoint { u: 0.5, v: 0.0 };
        let all = composed.zone_hit_stack(point);
        assert_eq!(
            all.into_iter()
                .map(|hit| (hit.zone.lease, hit.zone.local_item))
                .collect::<Vec<_>>(),
            vec![
                (root.id, LayoutItemId(11)),
                (child.id, LayoutItemId(10)),
                (root.id, LayoutItemId(10)),
            ]
        );

        let motion = composed.zone_hit_stack_for_event(point, PointerEventMask::MOTION);
        assert_eq!(
            motion
                .into_iter()
                .map(|hit| hit.zone.lease)
                .collect::<Vec<_>>(),
            vec![child.id, root.id]
        );
        let button = composed
            .top_zone_at(point, PointerEventMask::BUTTON)
            .ok_or_else(|| test_error("top button interaction zone is missing"))?;
        assert_eq!(button.zone.lease, root.id);
        assert_eq!(button.zone.local_item, LayoutItemId(11));
        Ok(())
    }

    #[test]
    fn invalid_zone_and_divider_data_is_rejected() -> Result<(), LayoutLeaseError> {
        fn rejection(fragment: LayoutFragment) -> Result<String, LayoutLeaseError> {
            let mut tree = LayoutLeaseTree::default();
            tree.update_inventory(1, &[])?;
            match tree.submit(tree.root_token(), 1, fragment, None) {
                Ok(()) => Err(test_error("invalid layout fragment was accepted")),
                Err(error) => Ok(error.to_string()),
            }
        }

        let mut zero_zone = empty_fragment();
        zero_zone
            .zones
            .push(zone(0, 0.0, full_transform(), PointerEventMask::BUTTON));
        assert!(rejection(zero_zone)?.contains("non-zero"));

        let mut duplicate_zone = empty_fragment();
        duplicate_zone.zones = vec![
            zone(1, 0.0, full_transform(), PointerEventMask::BUTTON),
            zone(1, 1.0, full_transform(), PointerEventMask::BUTTON),
        ];
        assert!(rejection(duplicate_zone)?.contains("appears more than once"));

        let mut non_finite_zone = empty_fragment();
        non_finite_zone.zones.push(zone(
            1,
            f64::NAN,
            full_transform(),
            PointerEventMask::BUTTON,
        ));
        assert!(rejection(non_finite_zone)?.contains("non-finite z"));

        let mut outside_zone = empty_fragment();
        outside_zone.zones.push(zone(
            1,
            0.0,
            transform(0.5, -1.0, 1.0, 2.0),
            PointerEventMask::BUTTON,
        ));
        assert!(rejection(outside_zone)?.contains("outside normalized"));

        let mut unsupported_events = empty_fragment();
        unsupported_events
            .zones
            .push(zone(1, 0.0, full_transform(), PointerEventMask(1 << 7)));
        assert!(rejection(unsupported_events)?.contains("unsupported pointer event mask"));

        let mut missing_zone = empty_fragment();
        missing_zone
            .dividers
            .push(divider(2, 1, 0.0, full_transform()));
        assert!(rejection(missing_zone)?.contains("unknown interaction zone"));

        let mut invalid_divider = empty_fragment();
        invalid_divider
            .zones
            .push(zone(1, 0.0, full_transform(), PointerEventMask::BUTTON));
        invalid_divider
            .dividers
            .push(divider(2, 1, f64::INFINITY, full_transform()));
        assert!(rejection(invalid_divider)?.contains("non-finite z"));
        Ok(())
    }

    #[test]
    fn waitable_panes_include_the_entire_lease_subtree_in_inventory_order()
    -> Result<(), LayoutLeaseError> {
        let mut tree = LayoutLeaseTree::default();
        tree.update_inventory(1, &[30, 10, 20])?;
        let root = tree.root_token();
        let child = tree.grant_rest(root, &[10, 20])?;
        let leaf = tree.grant_rest(child, &[20])?;

        assert_eq!(tree.owned_panes(root), Some(vec![30]));
        assert_eq!(tree.owned_panes(child), Some(vec![10]));
        assert_eq!(tree.owned_panes(leaf), Some(vec![20]));
        assert_eq!(tree.waitable_panes(root), Some(vec![30, 10, 20]));
        assert_eq!(tree.waitable_panes(child), Some(vec![10, 20]));
        assert_eq!(tree.waitable_panes(leaf), Some(vec![20]));
        assert_eq!(
            tree.waitable_panes(LayoutLeaseToken {
                generation: child.generation + 1,
                ..child
            }),
            None
        );
        Ok(())
    }
}
