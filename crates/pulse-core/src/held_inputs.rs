//! Fixed-size ledger of logically held keys and buttons.
//!
//! The ledger never allocates: keys are a bitset over the full evdev key-code
//! range and mouse buttons use a separate small bitmask. It is `Copy`, so
//! backends and the scheduler can each keep their own cross-checked copy.

use crate::input::{KEY_CODE_MAX, Key, KeyState, LogicalInput, MouseButton, Pressable};

const KEY_WORDS: usize = (KEY_CODE_MAX as usize / 64) + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct HeldInputs {
    keys: [u64; KEY_WORDS],
    buttons: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LedgerError {
    #[error("{0} is already held")]
    AlreadyHeld(Pressable),
    #[error("{0} is not held")]
    NotHeld(Pressable),
}

impl HeldInputs {
    pub const fn new() -> Self {
        HeldInputs {
            keys: [0; KEY_WORDS],
            buttons: 0,
        }
    }

    pub fn is_held(&self, p: Pressable) -> bool {
        match p {
            Pressable::Button(b) => self.buttons & (1 << b.index()) != 0,
            Pressable::Key(k) => {
                let c = k.code() as usize;
                self.keys[c / 64] & (1 << (c % 64)) != 0
            }
        }
    }

    fn set(&mut self, p: Pressable, held: bool) {
        match p {
            Pressable::Button(b) => {
                if held {
                    self.buttons |= 1 << b.index();
                } else {
                    self.buttons &= !(1 << b.index());
                }
            }
            Pressable::Key(k) => {
                let c = k.code() as usize;
                if held {
                    self.keys[c / 64] |= 1 << (c % 64);
                } else {
                    self.keys[c / 64] &= !(1 << (c % 64));
                }
            }
        }
    }

    pub fn press(&mut self, p: Pressable) -> Result<(), LedgerError> {
        if self.is_held(p) {
            return Err(LedgerError::AlreadyHeld(p));
        }
        self.set(p, true);
        Ok(())
    }

    pub fn release(&mut self, p: Pressable) -> Result<(), LedgerError> {
        if !self.is_held(p) {
            return Err(LedgerError::NotHeld(p));
        }
        self.set(p, false);
        Ok(())
    }

    /// Record the effect of an event.
    pub fn apply(&mut self, ev: &LogicalInput) -> Result<(), LedgerError> {
        match ev.pressable() {
            (p, KeyState::Pressed) => self.press(p),
            (p, KeyState::Released) => self.release(p),
        }
    }

    /// Record an event without enforcing transitions (used for conservative
    /// reconciliation after partial submission failures).
    pub fn apply_lenient(&mut self, ev: &LogicalInput) {
        let (p, st) = ev.pressable();
        self.set(p, st == KeyState::Pressed);
    }

    pub fn is_empty(&self) -> bool {
        self.buttons == 0 && self.keys.iter().all(|w| *w == 0)
    }

    pub fn len(&self) -> usize {
        self.buttons.count_ones() as usize + self.keys.iter().map(|w| w.count_ones() as usize).sum::<usize>()
    }

    pub fn clear(&mut self) {
        *self = HeldInputs::new();
    }

    /// Union of two ledgers.
    pub fn union(&self, other: &HeldInputs) -> HeldInputs {
        let mut out = *self;
        out.buttons |= other.buttons;
        for (a, b) in out.keys.iter_mut().zip(other.keys.iter()) {
            *a |= *b;
        }
        out
    }

    /// Iterate held inputs: keys first (in code order), then buttons.
    pub fn iter(&self) -> impl Iterator<Item = Pressable> + '_ {
        let keys = self.keys.iter().enumerate().flat_map(|(wi, &w)| {
            (0..64u16).filter_map(move |bit| {
                if w & (1 << bit) != 0 {
                    Key::from_code(wi as u16 * 64 + bit).map(Pressable::Key)
                } else {
                    None
                }
            })
        });
        let buttons = MouseButton::ALL
            .into_iter()
            .filter(move |b| self.buttons & (1 << b.index()) != 0)
            .map(Pressable::Button);
        keys.chain(buttons)
    }

    /// Release events for every held input, in deterministic order.
    pub fn releases(&self) -> impl Iterator<Item = LogicalInput> + '_ {
        self.iter().map(LogicalInput::release_of)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: &str) -> Pressable {
        Pressable::Key(Key::parse(n).expect("key"))
    }

    #[test]
    fn press_release_roundtrip() {
        let mut l = HeldInputs::new();
        assert!(l.is_empty());
        l.press(key("A")).expect("press");
        l.press(Pressable::Button(MouseButton::Left)).expect("press");
        assert_eq!(l.len(), 2);
        assert_eq!(l.press(key("A")), Err(LedgerError::AlreadyHeld(key("A"))));
        l.release(key("A")).expect("release");
        assert_eq!(l.release(key("A")), Err(LedgerError::NotHeld(key("A"))));
        assert!(!l.is_empty());
        let rel: Vec<_> = l.releases().collect();
        assert_eq!(rel, vec![LogicalInput::button(MouseButton::Left, KeyState::Released)]);
    }

    #[test]
    fn high_codes_are_tracked() {
        let mut l = HeldInputs::new();
        l.press(key("F24")).expect("press");
        assert!(l.is_held(key("F24")));
        assert_eq!(l.iter().collect::<Vec<_>>(), vec![key("F24")]);
    }

    #[test]
    fn union_and_lenient() {
        let mut a = HeldInputs::new();
        let mut b = HeldInputs::new();
        a.press(key("W")).expect("press");
        b.apply_lenient(&LogicalInput::button(MouseButton::Right, KeyState::Pressed));
        let u = a.union(&b);
        assert_eq!(u.len(), 2);
        b.apply_lenient(&LogicalInput::button(MouseButton::Right, KeyState::Released));
        b.apply_lenient(&LogicalInput::button(MouseButton::Right, KeyState::Released));
        assert!(b.is_empty());
    }
}
