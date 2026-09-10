#![no_std]

pub const MOD_CTRL: i32 = 1;
pub const MOD_ALT: i32 = 2;
pub const MOD_SHIFT: i32 = 4;
pub const MOD_SUPER: i32 = 8;
pub const VALID_MODIFIERS: i32 = MOD_CTRL | MOD_ALT | MOD_SHIFT | MOD_SUPER;

pub const KEY_ESCAPE: i32 = 9;
pub const KEY_1: i32 = 10;
pub const KEY_2: i32 = 11;
pub const KEY_3: i32 = 12;
pub const KEY_4: i32 = 13;
pub const KEY_5: i32 = 14;
pub const KEY_ENTER: i32 = 36;
pub const KEY_Q: i32 = 24;
pub const KEY_W: i32 = 25;
pub const KEY_E: i32 = 26;
pub const KEY_T: i32 = 28;
pub const KEY_U: i32 = 30;
pub const KEY_I: i32 = 31;
pub const KEY_O: i32 = 32;
pub const KEY_P: i32 = 33;
pub const KEY_A: i32 = 38;
pub const KEY_S: i32 = 39;
pub const KEY_D: i32 = 40;
pub const KEY_F: i32 = 41;
pub const KEY_G: i32 = 42;
pub const KEY_H: i32 = 43;
pub const KEY_J: i32 = 44;
pub const KEY_K: i32 = 45;
pub const KEY_L: i32 = 46;
pub const KEY_B: i32 = 56;
pub const KEY_C: i32 = 54;
pub const KEY_N: i32 = 57;
pub const KEY_M: i32 = 58;
pub const KEY_COMMA: i32 = 59;
pub const KEY_DOT: i32 = 60;
pub const KEY_SLASH: i32 = 61;
pub const KEY_SPACE: i32 = 65;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Binding {
    pub key_code: i32,
    pub modifiers: i32,
}

impl Binding {
    #[must_use]
    pub const fn new(key_code: i32, modifiers: i32) -> Self {
        Self {
            key_code,
            modifiers,
        }
    }

    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.key_code > 0
            && self.key_code <= 255
            && self.modifiers >= 0
            && self.modifiers & !VALID_MODIFIERS == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scope {
    Always,
    NormalMode,
}

impl Scope {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Always => "Global",
            Self::NormalMode => "Workspace mode",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Action {
    pub id: &'static str,
    pub title: &'static str,
    pub category: &'static str,
    pub scope: Scope,
    pub default_binding: Binding,
}

impl Action {
    #[must_use]
    pub const fn is_enabled(self, normal_mode: bool) -> bool {
        matches!(self.scope, Scope::Always) || normal_mode
    }
}

const CTRL: i32 = MOD_CTRL;
const CTRL_ALT: i32 = MOD_CTRL | MOD_ALT;
const CTRL_SHIFT: i32 = MOD_CTRL | MOD_SHIFT;

pub const ACTIONS: &[Action] = &[
    Action {
        id: "keybindings.open",
        title: "Open keyboard shortcuts",
        category: "Brood",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_SLASH, CTRL_SHIFT),
    },
    Action {
        id: "shortcuts.passthrough",
        title: "Toggle shortcut passthrough",
        category: "Brood",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_ESCAPE, CTRL_SHIFT),
    },
    Action {
        id: "workspace.mode",
        title: "Toggle workspace input mode",
        category: "Workspace",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_B, CTRL),
    },
    Action {
        id: "theme.picker",
        title: "Choose theme",
        category: "Brood",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_H, CTRL_ALT),
    },
    Action {
        id: "agent.picker",
        title: "Open agent picker",
        category: "Applications",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_A, CTRL_ALT),
    },
    Action {
        id: "pane.close",
        title: "Close focused pane",
        category: "Panes",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_Q, CTRL_ALT),
    },
    Action {
        id: "terminal.new",
        title: "Open new terminal",
        category: "Applications",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_T, CTRL_ALT),
    },
    Action {
        id: "editor.open",
        title: "Open editor",
        category: "Applications",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_E, CTRL_ALT),
    },
    Action {
        id: "inbox.show",
        title: "Show inbox",
        category: "Applications",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_I, CTRL_ALT),
    },
    Action {
        id: "complaints.capture",
        title: "Capture a complaint",
        category: "Brood",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_C, CTRL_ALT),
    },
    Action {
        id: "sessions.toggle",
        title: "Toggle session switcher",
        category: "Sessions",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_M, CTRL_ALT),
    },
    Action {
        id: "sessions.picker",
        title: "Open session picker",
        category: "Sessions",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_P, CTRL_ALT),
    },
    Action {
        id: "diff.toggle",
        title: "Toggle project diff",
        category: "Applications",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_D, CTRL_ALT),
    },
    Action {
        id: "sessions.next",
        title: "Switch to next session",
        category: "Sessions",
        scope: Scope::NormalMode,
        default_binding: Binding::new(KEY_SPACE, MOD_ALT),
    },
    Action {
        id: "focus.next",
        title: "Focus next managed pane",
        category: "Focus",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_J, CTRL),
    },
    Action {
        id: "focus.previous",
        title: "Focus previous managed pane",
        category: "Focus",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_K, CTRL),
    },
    Action {
        id: "terminal.spawn",
        title: "Spawn foot terminal",
        category: "Applications",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_O, CTRL),
    },
    Action {
        id: "split.down",
        title: "Split active pane down",
        category: "Layout",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_H, CTRL),
    },
    Action {
        id: "split.right",
        title: "Split active pane right",
        category: "Layout",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_L, CTRL),
    },
    Action {
        id: "split.smart",
        title: "Split active pane automatically",
        category: "Layout",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_S, CTRL),
    },
    Action {
        id: "split.join",
        title: "Join active pane",
        category: "Layout",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_U, CTRL),
    },
    Action {
        id: "split.next",
        title: "Focus next split",
        category: "Focus",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_DOT, CTRL),
    },
    Action {
        id: "split.previous",
        title: "Focus previous split",
        category: "Focus",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_COMMA, CTRL),
    },
    Action {
        id: "lazygit.open",
        title: "Open lazygit",
        category: "Applications",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_G, CTRL),
    },
    Action {
        id: "pi.open",
        title: "Open pi terminal",
        category: "Applications",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_ENTER, CTRL),
    },
    Action {
        id: "terminal.1",
        title: "Open or focus terminal 1",
        category: "Applications",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_1, CTRL),
    },
    Action {
        id: "terminal.2",
        title: "Open or focus terminal 2",
        category: "Applications",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_2, CTRL),
    },
    Action {
        id: "terminal.3",
        title: "Open or focus terminal 3",
        category: "Applications",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_3, CTRL),
    },
    Action {
        id: "terminal.4",
        title: "Open or focus terminal 4",
        category: "Applications",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_4, CTRL),
    },
    Action {
        id: "terminal.5",
        title: "Open or focus terminal 5",
        category: "Applications",
        scope: Scope::Always,
        default_binding: Binding::new(KEY_5, CTRL),
    },
    Action {
        id: "session.new",
        title: "Create and attach session",
        category: "Sessions",
        scope: Scope::NormalMode,
        default_binding: Binding::new(KEY_N, CTRL),
    },
    Action {
        id: "wezterm.spawn",
        title: "Spawn WezTerm",
        category: "Applications",
        scope: Scope::NormalMode,
        default_binding: Binding::new(KEY_W, CTRL),
    },
    Action {
        id: "pane.close-workspace",
        title: "Close active workspace pane",
        category: "Panes",
        scope: Scope::NormalMode,
        default_binding: Binding::new(KEY_Q, CTRL),
    },
    Action {
        id: "pane.fullscreen",
        title: "Toggle managed fullscreen",
        category: "Panes",
        scope: Scope::NormalMode,
        default_binding: Binding::new(KEY_F, CTRL),
    },
    Action {
        id: "dock.bottom",
        title: "Toggle bottom dock",
        category: "Layout",
        scope: Scope::NormalMode,
        default_binding: Binding::new(KEY_D, CTRL),
    },
    Action {
        id: "dock.sides",
        title: "Toggle side docks",
        category: "Layout",
        scope: Scope::NormalMode,
        default_binding: Binding::new(KEY_P, CTRL),
    },
];

#[must_use]
pub fn action(id: &str) -> Option<&'static Action> {
    ACTIONS.iter().find(|action| action.id == id)
}

#[must_use]
pub const fn key_name(key_code: i32) -> &'static str {
    match key_code {
        KEY_ESCAPE => "Esc",
        KEY_1 => "1",
        KEY_2 => "2",
        KEY_3 => "3",
        KEY_4 => "4",
        KEY_5 => "5",
        KEY_ENTER => "Enter",
        KEY_Q => "Q",
        KEY_W => "W",
        KEY_E => "E",
        KEY_T => "T",
        KEY_U => "U",
        KEY_I => "I",
        KEY_O => "O",
        KEY_P => "P",
        KEY_A => "A",
        KEY_S => "S",
        KEY_D => "D",
        KEY_F => "F",
        KEY_G => "G",
        KEY_H => "H",
        KEY_J => "J",
        KEY_K => "K",
        KEY_L => "L",
        KEY_B => "B",
        KEY_N => "N",
        KEY_M => "M",
        KEY_COMMA => ",",
        KEY_DOT => ".",
        KEY_SLASH => "/",
        KEY_SPACE => "Space",
        _ => "Key",
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn action_ids_and_default_chords_are_unique() {
        let mut ids = BTreeSet::new();
        let mut chords = BTreeSet::new();
        for action in ACTIONS {
            assert!(ids.insert(action.id));
            assert!(chords.insert((
                action.default_binding.key_code,
                action.default_binding.modifiers
            )));
            assert!(action.default_binding.is_valid());
        }
    }
}
