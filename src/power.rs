//! Best-effort system power actions (shutdown/reboot).
//!
//! In debug builds (`cargo build`) and whenever `OVERLAYD_DEBUG` is set, the
//! requested action is only logged so testing the overlay never powers the
//! machine off. Release builds execute the action through `systemctl`.
//!
//! Every operation is best-effort: failures are logged and ignored, matching
//! the audio module.

use std::process::Command;

/// A system power action offered by the overlay's power submenu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Power the system off.
    Shutdown,
    /// Restart the system.
    Reboot,
}

impl Action {
    /// The `systemctl` verb that performs this action.
    fn systemctl_verb(self) -> &'static str {
        match self {
            Action::Shutdown => "poweroff",
            Action::Reboot => "reboot",
        }
    }
}

/// True when the daemon should only log power actions instead of performing
/// them: debug builds (`cfg!(debug_assertions)`) or `OVERLAYD_DEBUG` set.
fn debug_mode() -> bool {
    cfg!(debug_assertions) || std::env::var_os("OVERLAYD_DEBUG").is_some()
}

/// Perform `action`, or only log it in debug mode.
pub fn perform(action: Action) {
    let verb = action.systemctl_verb();
    if debug_mode() {
        eprintln!("overlayd: [debug] power: would run `systemctl {verb}` (not executed)");
        return;
    }

    match Command::new("systemctl").arg(verb).status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("overlayd: power: systemctl {verb} exited {s:?}"),
        Err(e) => eprintln!("overlayd: power: systemctl {verb} error: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::Action;

    #[test]
    fn verbs_map_to_systemctl_commands() {
        assert_eq!(Action::Shutdown.systemctl_verb(), "poweroff");
        assert_eq!(Action::Reboot.systemctl_verb(), "reboot");
    }
}
