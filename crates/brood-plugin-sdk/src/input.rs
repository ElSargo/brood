//! Stable input constants shared by native plugin panes.

pub const MOD_CTRL: u32 = 1;
pub const MOD_ALT: u32 = 2;
pub const MOD_SHIFT: u32 = 4;
pub const MOD_SUPER: u32 = 8;

pub const BUTTON_PRIMARY: u32 = 0x110;
pub const BUTTON_SECONDARY: u32 = 0x111;
pub const BUTTON_MIDDLE: u32 = 0x112;

/// Named access to the modifier bitset carried by presentation events.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modifiers(u32);

impl Modifiers {
    #[must_use]
    pub const fn new(bits: u32) -> Self {
        Self(bits)
    }

    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    #[must_use]
    pub const fn control(self) -> bool {
        self.0 & MOD_CTRL != 0
    }

    #[must_use]
    pub const fn alt(self) -> bool {
        self.0 & MOD_ALT != 0
    }

    #[must_use]
    pub const fn shift(self) -> bool {
        self.0 & MOD_SHIFT != 0
    }

    #[must_use]
    pub const fn logo(self) -> bool {
        self.0 & MOD_SUPER != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_are_independent_bits() {
        let modifiers = Modifiers::new(MOD_CTRL | MOD_SHIFT);
        assert!(modifiers.control());
        assert!(modifiers.shift());
        assert!(!modifiers.alt());
        assert!(!modifiers.logo());
    }
}
