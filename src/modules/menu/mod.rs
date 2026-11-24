//! Menu module (M4, feature-gated by `mod-menu`).
//!
//! A simple vertical list navigable with D-pad. Demonstrates the Module
//! trait pattern: a Slint component plus Rust state.

use crate::core::{self, Event};
use crate::modules::{Handled, Module, ModuleCommand};

/// Menu module state.
pub struct MenuModule {
    /// Currently selected item index.
    selected: usize,
    /// Number of items.
    item_count: usize,
}

impl MenuModule {
    pub fn new() -> Self {
        Self {
            selected: 0,
            item_count: 3,
        }
    }

    fn set_selection(&mut self, index: usize) {
        self.selected = index.min(self.item_count.saturating_sub(1));
    }
}

impl Module for MenuModule {
    fn id(&self) -> &'static str {
        "menu"
    }

    fn on_event(&mut self, ev: &Event) -> (Handled, ModuleCommand) {
        match ev {
            Event::Pad(pad) => match pad.kind {
                core::PadEventKind::ButtonPressed(code) => match code {
                    // BTN_SOUTH → select / activate (hide overlay)
                    0x130 => (Handled::Yes, ModuleCommand::Hide),
                    // BTN_EAST → back (hide overlay)
                    0x134 => (Handled::Yes, ModuleCommand::Hide),
                    _ => (Handled::No, ModuleCommand::None),
                },
                // Button releases are ignored for menu navigation.
                core::PadEventKind::ButtonReleased(_) => (Handled::No, ModuleCommand::None),
                core::PadEventKind::AxisMoved(code, value) => match code {
                    0x11 => {
                        // ABS_HAT0Y: up/down
                        if value < 0 {
                            self.set_selection(self.selected.saturating_sub(1));
                        } else if value > 0 {
                            self.set_selection(self.selected + 1);
                        }
                        (Handled::Yes, ModuleCommand::None)
                    }
                    _ => (Handled::No, ModuleCommand::None),
                },
            },
            _ => (Handled::No, ModuleCommand::None),
        }
    }
}

impl Default for MenuModule {
    fn default() -> Self {
        Self::new()
    }
}
