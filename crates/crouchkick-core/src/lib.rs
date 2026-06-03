//! Idea: a crouch-kick needs crouch+jump within a few ms. The engine drops near-
//! simultaneous presses, so we **delay** each jump/crouch press up to `BUFFER_MS`.
//! If the *other* press lands within the window, both re-emit immediately in order
//! (the crouch-kick). Otherwise the per-frame `on_update` flush releases the held
//! press after the window so it still fires (just a hair late). Releases are held
//! too, so press/release ordering survives the buffering.
//!
//! This crate is FFI-free so the timing logic can be unit-tested on the host; the
//! plugin maps engine `PostEvent` args ↔ these abstract events and does re-emission.

/// FzzyMod `CROUCHKICK_BUFFERING` (milliseconds).
pub const BUFFER_MS: u64 = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Btn {
    Jump,
    Crouch,
}
impl Btn {
    fn idx(self) -> usize {
        match self {
            Btn::Jump => 0,
            Btn::Crouch => 1,
        }
    }
    fn other(self) -> Btn {
        match self {
            Btn::Jump => Btn::Crouch,
            Btn::Crouch => Btn::Jump,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    Press,
    Release,
}
impl Edge {
    fn idx(self) -> usize {
        match self {
            Edge::Press => 0,
            Edge::Release => 1,
        }
    }
}

/// What the plugin should do with the incoming jump/crouch event.
#[derive(PartialEq, Eq, Debug)]
pub enum Decision {
    /// Emit the current event normally (call the original `PostEvent`).
    Pass,
    /// Swallow the current event (return 0) and remember it as held — the plugin
    /// stores the raw `PostEvent` args for this `(Btn, Edge)` slot.
    Hold,
    /// Re-emit the named held event first, then emit the current event (crouch-kick).
    FlushThenPass(Btn, Edge),
}

/// Tracks which jump/crouch press/release events are currently held, and when.
#[derive(Default)]
pub struct Buffer {
    held: [[Option<u64>; 2]; 2], // [Btn::idx][Edge::idx] -> timestamp held (None = not held)
}

impl Buffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process an incoming jump/crouch event at `now_ms`.
    pub fn on_event(&mut self, btn: Btn, edge: Edge, now_ms: u64) -> Decision {
        // Already holding this slot -> let the new one pass (FzzyMod's `!waitingToSend` guard).
        if self.held[btn.idx()][edge.idx()].is_some() {
            return Decision::Pass;
        }
        if edge == Edge::Press {
            let other = btn.other();
            if let Some(ts) = self.held[other.idx()][Edge::Press.idx()] {
                if now_ms.saturating_sub(ts) <= BUFFER_MS {
                    // The other press arrived within the window -> crouch-kick.
                    self.held[other.idx()][Edge::Press.idx()] = None;
                    return Decision::FlushThenPass(other, Edge::Press);
                }
            }
        }
        // Hold this event (press or release) until the window elapses.
        self.held[btn.idx()][edge.idx()] = Some(now_ms);
        Decision::Hold
    }

    /// Per-Update tick: return the held events whose window has elapsed (the plugin
    /// re-emits each). Clears them from the held set.
    pub fn on_update(&mut self, now_ms: u64) -> Vec<(Btn, Edge)> {
        let mut out = Vec::new();
        for (btn, bi) in [(Btn::Jump, 0usize), (Btn::Crouch, 1)] {
            for (edge, ei) in [(Edge::Press, 0usize), (Edge::Release, 1)] {
                if let Some(ts) = self.held[bi][ei] {
                    if now_ms.saturating_sub(ts) > BUFFER_MS {
                        self.held[bi][ei] = None;
                        out.push((btn, edge));
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crouch_then_jump_within_window_is_a_kick() {
        let mut b = Buffer::new();
        assert_eq!(b.on_event(Btn::Crouch, Edge::Press, 0), Decision::Hold);
        // jump 5ms later -> flush the held crouch press, then pass the jump
        assert_eq!(
            b.on_event(Btn::Jump, Edge::Press, 5),
            Decision::FlushThenPass(Btn::Crouch, Edge::Press)
        );
        // nothing left to flush
        assert!(b.on_update(50).is_empty());
    }

    #[test]
    fn jump_then_crouch_within_window_is_a_kick() {
        let mut b = Buffer::new();
        assert_eq!(b.on_event(Btn::Jump, Edge::Press, 100), Decision::Hold);
        assert_eq!(
            b.on_event(Btn::Crouch, Edge::Press, 103),
            Decision::FlushThenPass(Btn::Jump, Edge::Press)
        );
    }

    #[test]
    fn lone_press_flushes_after_the_window() {
        let mut b = Buffer::new();
        assert_eq!(b.on_event(Btn::Jump, Edge::Press, 0), Decision::Hold);
        assert!(b.on_update(5).is_empty()); // still within window
        assert_eq!(b.on_update(20), vec![(Btn::Jump, Edge::Press)]); // window elapsed -> flush
        assert!(b.on_update(40).is_empty()); // already flushed
    }

    #[test]
    fn second_press_outside_window_is_not_a_kick() {
        let mut b = Buffer::new();
        assert_eq!(b.on_event(Btn::Crouch, Edge::Press, 0), Decision::Hold);
        let _ = b.on_update(20); // crouch flushed by the tick
        // jump much later -> just held on its own, not a kick
        assert_eq!(b.on_event(Btn::Jump, Edge::Press, 100), Decision::Hold);
    }

    #[test]
    fn releases_are_held_then_flushed() {
        let mut b = Buffer::new();
        assert_eq!(b.on_event(Btn::Jump, Edge::Release, 0), Decision::Hold);
        assert_eq!(b.on_update(20), vec![(Btn::Jump, Edge::Release)]);
    }

    #[test]
    fn duplicate_hold_passes_through() {
        let mut b = Buffer::new();
        assert_eq!(b.on_event(Btn::Jump, Edge::Press, 0), Decision::Hold);
        // a second jump press while one is held -> pass (don't double-hold)
        assert_eq!(b.on_event(Btn::Jump, Edge::Press, 2), Decision::Pass);
    }
}
