use smithay::backend::input::Keycode;
use std::{fmt, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutAction {
    FocusNext,
    FocusPrevious,
    CloseActivePane,
    ToggleManagedFullscreen,
    SplitActiveRight,
    SplitActiveDown,
    JoinActivePane,
    MoveActiveToLeftDock,
    MoveActiveToRightDock,
    MoveActiveToBottomDock,
    MoveActiveToCenter,
    ToggleLeftDock,
    ToggleRightDock,
    ToggleBottomDock,
}

impl LayoutAction {
    pub fn from_abi(value: i32) -> Option<Self> {
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
            _ => return None,
        })
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::FocusNext => "focus-next",
            Self::FocusPrevious => "focus-previous",
            Self::CloseActivePane => "close-active-pane",
            Self::ToggleManagedFullscreen => "toggle-managed-fullscreen",
            Self::SplitActiveRight => "split-active-right",
            Self::SplitActiveDown => "split-active-down",
            Self::JoinActivePane => "join-active-pane",
            Self::MoveActiveToLeftDock => "move-active-to-left-dock",
            Self::MoveActiveToRightDock => "move-active-to-right-dock",
            Self::MoveActiveToBottomDock => "move-active-to-bottom-dock",
            Self::MoveActiveToCenter => "move-active-to-center",
            Self::ToggleLeftDock => "toggle-left-dock",
            Self::ToggleRightDock => "toggle-right-dock",
            Self::ToggleBottomDock => "toggle-bottom-dock",
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
            "split-active-down" | "split-down" => Self::SplitActiveDown,
            "join-active-pane" | "join-pane" => Self::JoinActivePane,
            "move-active-to-left-dock" => Self::MoveActiveToLeftDock,
            "move-active-to-right-dock" => Self::MoveActiveToRightDock,
            "move-active-to-bottom-dock" => Self::MoveActiveToBottomDock,
            "move-active-to-center" => Self::MoveActiveToCenter,
            "toggle-left-dock" => Self::ToggleLeftDock,
            "toggle-right-dock" => Self::ToggleRightDock,
            "toggle-bottom-dock" => Self::ToggleBottomDock,
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
    pub fn split_active(&mut self, axis: LayoutAxis) -> bool {
        let new_leaf = LayoutLeaf {
            id: self.allocate_leaf(),
            kind: LayoutLeafKind::Placeholder,
        };
        self.root.split_leaf(self.active_leaf, axis, new_leaf)
    }

    pub fn active_leaf(&self) -> LayoutLeafId {
        self.active_leaf
    }

    pub fn occupy_active_leaf(&mut self) -> Option<LayoutLeafId> {
        self.root
            .set_leaf_kind(self.active_leaf, LayoutLeafKind::Client)
            .then_some(self.active_leaf)
    }

    pub fn active_rect(&self, size: LayoutSize) -> Option<LayoutRect> {
        self.leaf_rect(self.active_leaf, size)
    }

    pub fn leaf_rect(&self, leaf: LayoutLeafId, size: LayoutSize) -> Option<LayoutRect> {
        self.resolve(size)
            .into_iter()
            .find(|region| region.leaf == leaf)
            .map(|region| region.rect)
    }

    pub fn active_kind(&self) -> Option<LayoutLeafKind> {
        self.root.leaf_kind(self.active_leaf)
    }

    pub fn focus_next(&mut self) -> bool {
        self.focus_by_offset(1)
    }

    pub fn focus_previous(&mut self) -> bool {
        self.focus_by_offset(-1)
    }

    pub fn region_count(&self) -> usize {
        self.root.leaf_count()
    }

    pub fn placeholder_count(&self) -> usize {
        self.root.placeholder_count()
    }

    pub fn resolve(&self, size: LayoutSize) -> Vec<LayoutRegion> {
        let mut regions = Vec::new();
        self.root.resolve(
            LayoutRect {
                x: 0,
                y: 0,
                w: size.w.max(1),
                h: size.h.max(1),
            },
            self.active_leaf,
            &mut regions,
        );
        regions
    }

    fn allocate_leaf(&mut self) -> LayoutLeafId {
        let id = LayoutLeafId(self.next_leaf);
        self.next_leaf += 1;
        id
    }

    fn focus_by_offset(&mut self, offset: isize) -> bool {
        let mut leaves = Vec::new();
        self.root.collect_leaf_ids(&mut leaves);
        let len = leaves.len();
        if len == 0 {
            return false;
        }
        let current_ix = leaves
            .iter()
            .position(|leaf| *leaf == self.active_leaf)
            .unwrap_or(0);
        let next_ix = (current_ix as isize + offset).rem_euclid(len as isize) as usize;
        let next = leaves[next_ix];
        if next == self.active_leaf {
            return false;
        }
        self.active_leaf = next;
        true
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
    pub fn contains_f64(self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x)
            && y >= f64::from(self.y)
            && x < f64::from(self.x + self.w)
            && y < f64::from(self.y + self.h)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutLeafKind {
    Client,
    Placeholder,
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
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    fn collect_leaf_ids(&self, leaves: &mut Vec<LayoutLeafId>) {
        match self {
            Self::Leaf(leaf) => leaves.push(leaf.id),
            Self::Split { first, second, .. } => {
                first.collect_leaf_ids(leaves);
                second.collect_leaf_ids(leaves);
            }
        }
    }

    fn leaf_kind(&self, target: LayoutLeafId) -> Option<LayoutLeafKind> {
        match self {
            Self::Leaf(leaf) if leaf.id == target => Some(leaf.kind),
            Self::Leaf(_) => None,
            Self::Split { first, second, .. } => {
                first.leaf_kind(target).or_else(|| second.leaf_kind(target))
            }
        }
    }

    fn set_leaf_kind(&mut self, target: LayoutLeafId, kind: LayoutLeafKind) -> bool {
        match self {
            Self::Leaf(leaf) if leaf.id == target => {
                leaf.kind = kind;
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.set_leaf_kind(target, kind) || second.set_leaf_kind(target, kind)
            }
        }
    }

    fn leaf_count(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }

    fn placeholder_count(&self) -> usize {
        match self {
            Self::Leaf(leaf) => usize::from(leaf.kind == LayoutLeafKind::Placeholder),
            Self::Split { first, second, .. } => {
                first.placeholder_count() + second.placeholder_count()
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

fn split_rect(rect: LayoutRect, axis: LayoutAxis) -> (LayoutRect, LayoutRect) {
    match axis {
        LayoutAxis::Horizontal => {
            let first_w = rect.w / 2;
            let second_w = rect.w - first_w;
            (
                LayoutRect { w: first_w, ..rect },
                LayoutRect {
                    x: rect.x + first_w,
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
                    y: rect.y + first_h,
                    h: second_h,
                    ..rect
                },
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutKeyBinding {
    pub key: Keycode,
    pub ctrl: bool,
    pub action: LayoutAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutKeymap {
    bindings: Vec<LayoutKeyBinding>,
}

impl Default for LayoutKeymap {
    fn default() -> Self {
        Self {
            bindings: vec![
                LayoutKeyBinding {
                    key: Keycode::new(44),
                    ctrl: false,
                    action: LayoutAction::FocusNext,
                },
                LayoutKeyBinding {
                    key: Keycode::new(45),
                    ctrl: false,
                    action: LayoutAction::FocusPrevious,
                },
                LayoutKeyBinding {
                    key: Keycode::new(24),
                    ctrl: false,
                    action: LayoutAction::CloseActivePane,
                },
                LayoutKeyBinding {
                    key: Keycode::new(41),
                    ctrl: false,
                    action: LayoutAction::ToggleManagedFullscreen,
                },
                LayoutKeyBinding {
                    key: Keycode::new(46),
                    ctrl: false,
                    action: LayoutAction::SplitActiveRight,
                },
                LayoutKeyBinding {
                    key: Keycode::new(43),
                    ctrl: true,
                    action: LayoutAction::SplitActiveDown,
                },
                LayoutKeyBinding {
                    key: Keycode::new(23),
                    ctrl: false,
                    action: LayoutAction::ToggleLeftDock,
                },
                LayoutKeyBinding {
                    key: Keycode::new(26),
                    ctrl: false,
                    action: LayoutAction::ToggleRightDock,
                },
                LayoutKeyBinding {
                    key: Keycode::new(48),
                    ctrl: false,
                    action: LayoutAction::ToggleBottomDock,
                },
            ],
        }
    }
}

impl LayoutKeymap {
    pub fn action_for_key(&self, key: Keycode, ctrl: bool) -> Option<LayoutAction> {
        self.bindings
            .iter()
            .find(|binding| binding.key == key && binding.ctrl == ctrl)
            .map(|binding| binding.action)
    }
}

#[cfg(test)]
mod tests {
    use super::{LayoutAction, LayoutKeymap};
    use smithay::backend::input::Keycode;

    #[test]
    fn decodes_abi_actions() {
        assert_eq!(LayoutAction::from_abi(1), Some(LayoutAction::FocusNext));
        assert_eq!(
            LayoutAction::from_abi(14),
            Some(LayoutAction::ToggleBottomDock)
        );
        assert_eq!(LayoutAction::from_abi(99), None);
    }

    #[test]
    fn parses_layout_action_names() {
        assert_eq!(
            "split-right".parse::<LayoutAction>(),
            Ok(LayoutAction::SplitActiveRight)
        );
        assert!("bogus".parse::<LayoutAction>().is_err());
    }

    #[test]
    fn default_keymap_maps_normal_mode_actions() {
        let keymap = LayoutKeymap::default();

        assert_eq!(
            keymap.action_for_key(Keycode::new(44), false),
            Some(LayoutAction::FocusNext)
        );
        assert_eq!(
            keymap.action_for_key(Keycode::new(43), true),
            Some(LayoutAction::SplitActiveDown)
        );
    }

    #[test]
    fn split_active_creates_placeholder_region() {
        let mut layout = super::LayoutState::default();

        assert!(layout.split_active(super::LayoutAxis::Horizontal));
        let regions = layout.resolve(super::LayoutSize { w: 100, h: 80 });

        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].rect.w, 50);
        assert!(regions[0].active);
        assert_eq!(regions[1].kind, super::LayoutLeafKind::Placeholder);
        assert_eq!(layout.region_count(), 2);
        assert_eq!(layout.placeholder_count(), 1);
    }

    #[test]
    fn focus_moves_between_client_and_placeholder_regions() {
        let mut layout = super::LayoutState::default();
        layout.split_active(super::LayoutAxis::Horizontal);

        assert_eq!(layout.active_kind(), Some(super::LayoutLeafKind::Client));
        assert!(layout.focus_next());
        assert_eq!(
            layout.active_kind(),
            Some(super::LayoutLeafKind::Placeholder)
        );
        assert!(layout.focus_previous());
        assert_eq!(layout.active_kind(), Some(super::LayoutLeafKind::Client));
    }

    #[test]
    fn client_rect_stays_with_client_when_placeholder_is_focused() {
        let mut layout = super::LayoutState::default();
        layout.split_active(super::LayoutAxis::Horizontal);
        let size = super::LayoutSize { w: 100, h: 80 };
        let client_rect = layout
            .resolve(size)
            .into_iter()
            .find(|region| region.kind == super::LayoutLeafKind::Client)
            .map(|region| region.rect);

        assert!(layout.focus_next());
        assert_eq!(
            layout.active_kind(),
            Some(super::LayoutLeafKind::Placeholder)
        );
        assert_eq!(
            layout
                .resolve(size)
                .into_iter()
                .find(|region| region.kind == super::LayoutLeafKind::Client)
                .map(|region| region.rect),
            client_rect
        );
        assert_ne!(layout.active_rect(size), client_rect);
    }

    #[test]
    fn occupying_active_placeholder_turns_it_into_client_region() {
        let mut layout = super::LayoutState::default();
        layout.split_active(super::LayoutAxis::Horizontal);
        assert!(layout.focus_next());

        let occupied_leaf = layout.occupy_active_leaf();
        let regions = layout.resolve(super::LayoutSize { w: 100, h: 80 });

        assert_eq!(occupied_leaf, Some(layout.active_leaf()));
        assert_eq!(layout.active_kind(), Some(super::LayoutLeafKind::Client));
        assert_eq!(layout.placeholder_count(), 0);
        assert_eq!(regions.iter().filter(|region| region.active).count(), 1);
        assert_eq!(
            regions
                .iter()
                .filter(|region| region.kind == super::LayoutLeafKind::Client)
                .count(),
            2
        );
    }
}
