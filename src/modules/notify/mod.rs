//! Notification module (M4, feature-gated by `mod-notify`).
//!
//! Displays a temporary notification message at the bottom of the screen.
//! Triggered via IPC `notify <text>`.

use crate::core::{self, Event};
use crate::modules::{Handled, Module, ModuleCommand};

/// Notification module state.
pub struct NotifyModule {
    /// Current notification text (empty = no notification).
    text: String,
}

impl NotifyModule {
    pub fn new() -> Self {
        Self {
            text: String::new(),
        }
    }
}

impl Module for NotifyModule {
    fn id(&self) -> &'static str {
        "notify"
    }

    fn on_event(&mut self, ev: &Event) -> (Handled, ModuleCommand) {
        match ev {
            Event::Ipc(core::IpcMsg::Notify) => {
                self.text = "Notification".to_string();
                (Handled::Yes, ModuleCommand::None)
            }
            _ => (Handled::No, ModuleCommand::None),
        }
    }
}

impl Default for NotifyModule {
    fn default() -> Self {
        Self::new()
    }
}
