//! Portable viewport layout primitives.
//!
//! This crate deliberately knows nothing about compositor surfaces, rendering,
//! transports, or plugin runtimes. Presentation hosts supply pane inventory and
//! execute the accepted layout.

use std::{collections::BTreeSet, fmt, str::FromStr};

use num_traits::ToPrimitive;

const MAX_RESOLVED_LAYOUT_ITEMS: usize = 512;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(transparent)]
pub struct LayoutItemId(pub u64);

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
        if point.u < self.offset.u
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
        reason = "legacy ABI tests still document numeric layout action mapping"
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
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "layout tests assert exact API results and tree positions"
)]
mod tests {
    use super::{
        InputBehavior, LayoutAxis, LayoutItemId, LayoutItemKind, LayoutRect, LayoutSize,
        LayoutState, LocalPoint, LocalTransform, ResolvedLayout, ResolvedLayoutItem,
    };

    #[test]
    fn split_tree_resolves_and_collapses_without_host_state() {
        let mut layout = LayoutState::default();
        let first_leaf = layout.active_leaf();
        let second_leaf = layout
            .split_active(LayoutAxis::Horizontal)
            .expect("split creates a second leaf");
        assert!(layout.set_active_leaf(second_leaf));

        let split = layout.resolve(LayoutSize { w: 1281, h: 800 });
        assert_eq!(split.len(), 2);
        assert_eq!(
            split[0].rect,
            LayoutRect {
                x: 0,
                y: 0,
                w: 640,
                h: 800
            }
        );
        assert_eq!(
            split[1].rect,
            LayoutRect {
                x: 640,
                y: 0,
                w: 641,
                h: 800,
            }
        );

        assert_eq!(layout.remove_active_leaf(), Some((second_leaf, first_leaf)));
        assert_eq!(
            layout.resolve(LayoutSize { w: 1281, h: 800 })[0].rect,
            LayoutRect {
                x: 0,
                y: 0,
                w: 1281,
                h: 800,
            }
        );
    }

    #[test]
    fn resolved_layout_validation_is_inventory_driven() {
        let valid = ResolvedLayout {
            items: vec![pane_item(7, 0.0, -1.0, -1.0, 2.0, 2.0)],
        };
        assert!(valid.clone().validate_for_panes(&[7]).is_ok());
        assert!(valid.validate_for_panes(&[8]).is_err());

        let duplicate = ResolvedLayout {
            items: vec![
                pane_item(7, 0.0, -1.0, -1.0, 1.0, 2.0),
                pane_item(7, 1.0, 0.0, -1.0, 1.0, 2.0),
            ],
        };
        assert!(duplicate.validate_for_panes(&[7]).is_err());
    }

    #[test]
    fn hit_testing_uses_layout_z_order_and_local_coordinates() {
        let layout = ResolvedLayout {
            items: vec![
                pane_item(7, 0.0, -1.0, -1.0, 2.0, 2.0),
                ResolvedLayoutItem {
                    id: LayoutItemId(99),
                    kind: LayoutItemKind::Handle,
                    z: 5.0,
                    transform: LocalTransform {
                        offset: LocalPoint { u: -0.2, v: -1.0 },
                        size: LocalPoint { u: 0.4, v: 2.0 },
                    },
                    input: InputBehavior::Drag,
                },
            ],
        }
        .validate_for_panes(&[7])
        .expect("layout is valid");

        let hits = layout.hit_stack(LocalPoint { u: 0.0, v: 0.0 });
        assert_eq!(hits[0].kind, LayoutItemKind::Handle);
        assert_eq!(hits[0].local_point, LocalPoint { u: 0.0, v: 0.0 });
        assert_eq!(hits[1].kind, LayoutItemKind::Pane { pane: 7 });
    }

    #[test]
    fn normalized_projection_preserves_fractional_and_empty_bounds() {
        let transform = LocalTransform {
            offset: LocalPoint { u: -0.5, v: -0.5 },
            size: LocalPoint { u: 0.5, v: 0.5 },
        };

        assert_eq!(
            transform.pixel_rect(LayoutSize { w: 5, h: 7 }),
            LayoutRect {
                x: 1,
                y: 2,
                w: 1,
                h: 2,
            }
        );
        assert_eq!(
            transform.pixel_rect(LayoutSize { w: 0, h: -4 }),
            LayoutRect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            }
        );
    }

    fn pane_item(
        pane: u64,
        z: f64,
        offset_u: f64,
        offset_v: f64,
        size_u: f64,
        size_v: f64,
    ) -> ResolvedLayoutItem {
        ResolvedLayoutItem {
            id: LayoutItemId(pane),
            kind: LayoutItemKind::Pane { pane },
            z,
            transform: LocalTransform {
                offset: LocalPoint {
                    u: offset_u,
                    v: offset_v,
                },
                size: LocalPoint {
                    u: size_u,
                    v: size_v,
                },
            },
            input: InputBehavior::Pane,
        }
    }
}
