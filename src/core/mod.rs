//! Platform-independent core (PLAN.md §1): the three-state machine and the
//! `Event`/`Command` types that cross the platform boundary.
//!
//! This module must stay free of Wayland, evdev and Slint dependencies so it
//! can be unit-tested on any desktop.
//!
//! The whole `Event`/`Command` surface is covered by the unit tests below;
//! the shipped binary only reaches a subset until M1 (IPC) and M2 (pads)
//! feed the remaining events, so don't flag those variants as dead code.
#![allow(dead_code)]

/// Visibility mode of the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No surface, no buffers, no pad grab: ~0% CPU (PLAN.md §1).
    Hidden,
    /// Passive, click-through surface (FPS counter, ...).
    Hud,
    /// Full menu: keyboard interactivity + gamepads grabbed.
    Interactive,
}

/// Gamepad input, normalized (M2 fills this in; PLAN.md §3).
#[allow(dead_code)] // constructed by M2; defined now so the boundary is fixed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadEvent;

/// IPC message from the unix socket (M1 fills this in; PLAN.md §5).
#[allow(dead_code)] // constructed by M1; defined now so the boundary is fixed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpcMsg;

/// Input into the core. The platform layers translate their native events
/// into this enum; the core never talks back in platform types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Pad(PadEvent),
    Ipc(IpcMsg),
    /// Periodic tick (timers, animation frames).
    Tick,
    /// The layer surface received its first configure.
    SurfaceConfigured,
    /// IPC `toggle`: switch between Hidden and the last visible mode.
    Toggle,
    /// IPC `show menu` / `show hud`.
    Show(Mode),
    /// IPC `hide`.
    Hide,
}

/// What the platform layer must do in response to events (PLAN.md §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Show(Mode),
    Hide,
    /// Grab (or release) the gamepads with `EVIOCGRAB` (M3).
    GrabPads(bool),
    /// Schedule a redraw.
    Redraw,
}

/// The Hidden ⇄ Hud ⇄ Interactive state machine.
#[derive(Debug)]
pub struct State {
    mode: Mode,
    /// Mode to return to on `toggle` while hidden.
    last_visible: Mode,
}

impl State {
    pub fn new() -> Self {
        Self {
            mode: Mode::Hidden,
            last_visible: Mode::Hud,
        }
    }

    pub const fn mode(&self) -> Mode {
        self.mode
    }

    /// Feed one event; returns the commands the platform layer must execute.
    /// Transitions are idempotent: repeating the current state yields no work.
    pub fn handle(&mut self, ev: &Event) -> Vec<Command> {
        let target = match ev {
            Event::Show(m) => Some(*m),
            Event::Hide => Some(Mode::Hidden),
            Event::Toggle => Some(if self.mode == Mode::Hidden {
                self.last_visible
            } else {
                Mode::Hidden
            }),
            Event::Tick | Event::SurfaceConfigured | Event::Pad(_) | Event::Ipc(_) => None,
        };
        let Some(target) = target else {
            return Vec::new();
        };
        if target == self.mode {
            return Vec::new();
        }
        if target != Mode::Hidden {
            self.last_visible = target;
        }
        self.mode = target;
        match target {
            Mode::Hidden => vec![Command::Hide],
            visible => vec![Command::Show(visible)],
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_hidden() {
        assert_eq!(State::new().mode(), Mode::Hidden);
    }

    #[test]
    fn show_hide() {
        let mut s = State::new();
        assert_eq!(
            s.handle(&Event::Show(Mode::Interactive)),
            vec![Command::Show(Mode::Interactive)]
        );
        assert_eq!(s.mode(), Mode::Interactive);
        // Idempotent: showing the current mode is a no-op.
        assert_eq!(s.handle(&Event::Show(Mode::Interactive)), vec![]);
        assert_eq!(s.handle(&Event::Hide), vec![Command::Hide]);
        assert_eq!(s.mode(), Mode::Hidden);
        // Hiding while hidden is a no-op.
        assert_eq!(s.handle(&Event::Hide), vec![]);
    }

    #[test]
    fn toggle_returns_to_last_visible_mode() {
        let mut s = State::new();
        s.handle(&Event::Show(Mode::Interactive));
        assert_eq!(s.handle(&Event::Toggle), vec![Command::Hide]);
        assert_eq!(s.mode(), Mode::Hidden);
        assert_eq!(
            s.handle(&Event::Toggle),
            vec![Command::Show(Mode::Interactive)]
        );

        s.handle(&Event::Hide);
        s.handle(&Event::Show(Mode::Hud));
        s.handle(&Event::Toggle); // back to hidden
        assert_eq!(
            s.handle(&Event::Toggle),
            vec![Command::Show(Mode::Hud)],
            "toggle must return to the most recent visible mode"
        );
    }

    #[test]
    fn mode_switch_when_visible() {
        let mut s = State::new();
        s.handle(&Event::Show(Mode::Hud));
        assert_eq!(
            s.handle(&Event::Show(Mode::Interactive)),
            vec![Command::Show(Mode::Interactive)]
        );
        assert_eq!(s.mode(), Mode::Interactive);
    }

    #[test]
    fn unhandled_events_produce_no_commands() {
        let mut s = State::new();
        for ev in [
            Event::Tick,
            Event::SurfaceConfigured,
            Event::Pad(PadEvent),
            Event::Ipc(IpcMsg),
        ] {
            assert_eq!(s.handle(&ev), vec![]);
        }
        assert_eq!(s.mode(), Mode::Hidden);
    }
}
