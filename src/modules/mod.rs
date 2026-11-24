//! Compiled-in overlay modules (M4, PLAN.md §6).
//!
//! Modules are gated by Cargo features (`mod-menu`, `mod-notify`). Each
//! module owns a Rust struct implementing `Module`. The core routes events
//! to the topmost active module and shows its Slint UI.
//!
//! Module Slint components are included via `slint::include_modules!()` in
//! `src/main.rs` and looked up by ID from a static registry.

use crate::core::Event;

#[cfg(feature = "mod-menu")]
pub mod menu;
#[cfg(feature = "mod-notify")]
pub mod notify;

/// Command a module can request the core to execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModuleCommand {
    /// No command.
    #[default]
    None,
    /// Switch to a different module by ID.
    #[allow(dead_code)]
    SwitchTo(&'static str),
    /// Hide the overlay (back to Hidden state).
    Hide,
}

/// Result of a module's event handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// The module consumed the event; no further processing.
    Yes,
    /// The module did not consume the event; pass it on.
    No,
}

/// A compiled-in overlay module.
pub trait Module {
    /// Static module identifier (e.g. "menu", "notify").
    fn id(&self) -> &'static str;

    /// Handle an event. Returns whether it was consumed and any command
    /// for the core to execute.
    fn on_event(&mut self, ev: &Event) -> (Handled, ModuleCommand);
}

/// Module registry: holds all compiled-in modules and routes events.
pub struct ModuleRegistry {
    modules: Vec<Box<dyn Module>>,
    active: Option<usize>,
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self {
            modules: Vec::new(),
            active: None,
        }
    }

    /// Register a module (called once at startup).
    pub fn register(&mut self, module: Box<dyn Module>) {
        self.modules.push(module);
    }

    /// Activate a module by ID. Returns true if found and activated.
    pub fn activate(&mut self, id: &str) -> bool {
        if let Some(pos) = self.modules.iter().position(|m| m.id() == id) {
            self.active = Some(pos);
            true
        } else {
            false
        }
    }

    /// Deactivate the current module.
    pub fn deactivate(&mut self) {
        self.active = None;
    }

    /// Returns the currently active module, if any.
    pub fn active_module(&mut self) -> Option<&mut (dyn Module + 'static)> {
        self.active
            .and_then(|i| self.modules.get_mut(i).map(|m| m.as_mut()))
    }

    /// Returns the ID of the active module, if any.
    #[allow(dead_code)]
    pub fn active_id(&self) -> Option<&'static str> {
        self.active
            .and_then(|i| self.modules.get(i).map(|m| m.id()))
    }

    /// Returns the number of registered modules.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.modules.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }
}

impl Default for ModuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(feature = "mod-menu", feature = "mod-notify"))]
pub(crate) fn register_modules(reg: &mut ModuleRegistry) {
    #[cfg(feature = "mod-menu")]
    reg.register(Box::new(crate::modules::menu::MenuModule::new()));
    #[cfg(feature = "mod-notify")]
    reg.register(Box::new(crate::modules::notify::NotifyModule::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyModule;
    impl Module for DummyModule {
        fn id(&self) -> &'static str {
            "dummy"
        }
        fn on_event(&mut self, _ev: &Event) -> (Handled, ModuleCommand) {
            (Handled::No, ModuleCommand::None)
        }
    }

    #[test]
    fn activate_and_deactivate() {
        let mut reg = ModuleRegistry::new();
        reg.register(Box::new(DummyModule));
        assert!(reg.activate("dummy"));
        assert_eq!(reg.active_id(), Some("dummy"));
        reg.deactivate();
        assert_eq!(reg.active_id(), None);
    }

    #[test]
    fn activate_unknown_id() {
        let mut reg = ModuleRegistry::new();
        reg.register(Box::new(DummyModule));
        assert!(!reg.activate("nonexistent"));
    }
}
