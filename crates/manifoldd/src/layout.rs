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
}

impl Default for LayoutState {
    fn default() -> Self {
        Self {
            left_dock_visible: false,
            right_dock_visible: false,
            bottom_dock_visible: false,
            center_visible: true,
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
                    key: Keycode::new(38),
                    ctrl: false,
                    action: LayoutAction::SplitActiveRight,
                },
                LayoutKeyBinding {
                    key: Keycode::new(44),
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
            keymap.action_for_key(Keycode::new(44), true),
            Some(LayoutAction::SplitActiveDown)
        );
    }
}
