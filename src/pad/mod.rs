//! evdev gamepad layer (M2/M3, PLAN.md §3).
//!
//! Enumerates `/dev/input/event*` devices, opens gamepads, normalizes
//! evdev codes into `core::PadEvent`s, and feeds them into the core state
//! machine via calloop. Supports grab (EVIOCGRAB) while Interactive.
//!
//! Each device FD is `dup()`ed: the `evdev::Device` owns the original FD
//! (used for `fetch_events()` and `grab()`/`ungrab()`), while calloop's
//! `Generic` source owns the duplicate FD (used only for poll notification).

use std::collections::HashMap;
use std::os::fd::BorrowedFd;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction};
use evdev::{enumerate, Device, EventSummary, KeyCode};
use nix::fcntl::{self, OFlag};
use nix::sys::stat::Mode as StatMode;
use nix::unistd::dup;

use crate::core::{self, PadEventKind};

/// Normalized gamepad state, tracked per device for summon-detection.
#[derive(Debug, Default, Clone, Copy)]
struct PadState {
    /// Currently held buttons (bitmask over evdev codes mod 64).
    buttons: u64,
}

/// Detector for the summon combo (Select + Start held simultaneously).
struct SummonDetector {
    state: PadState,
    /// True once both buttons seen pressed; prevents re-firing until release.
    combo_armed: bool,
}

impl SummonDetector {
    fn new() -> Self {
        Self {
            state: PadState::default(),
            combo_armed: false,
        }
    }

    /// Feed a button event; returns `true` when the summon combo fires.
    fn button(&mut self, code: u16, pressed: bool) -> bool {
        let bit = 1u64 << (code % 64);
        if pressed {
            self.state.buttons |= bit;
        } else {
            self.state.buttons &= !bit;
        }

        let select = KeyCode::BTN_SELECT.0;
        let start = KeyCode::BTN_START.0;
        let hold_select = self.state.buttons & (1u64 << (select % 64)) != 0;
        let hold_start = self.state.buttons & (1u64 << (start % 64)) != 0;

        if hold_select && hold_start {
            if !self.combo_armed {
                self.combo_armed = true;
                return true; // summon fires once per press
            }
        } else {
            self.combo_armed = false;
        }
        false
    }
}

/// Per-device transient state (the evdev `Device` is stored separately in
/// `PadManager.devices`).
struct PadDeviceState {
    summon: SummonDetector,
}

/// Manages all gamepad devices: enumeration, open/close, dispatch.
pub struct PadManager {
    /// One entry per open device. Used for `fetch_events()` and
    /// `grab()`/`ungrab()` from the calloop callback.
    devices: HashMap<u8, Device>,
    /// Device paths (used for duplicate detection since `Device` doesn't
    /// expose its path).
    paths: HashMap<u8, PathBuf>,
    state: HashMap<u8, PadDeviceState>,
    /// calloop source tokens, keyed by device index, for removal on hotplug.
    tokens: HashMap<u8, calloop::RegistrationToken>,
    next_index: u8,
}

impl PadManager {
    pub fn new() -> Self {
        Self {
            devices: HashMap::new(),
            paths: HashMap::new(),
            state: HashMap::new(),
            tokens: HashMap::new(),
            next_index: 0,
        }
    }

    /// Enumerate `/dev/input` and open any gamepad-like devices not already
    /// tracked. Returns the list of newly opened device paths.
    pub fn enumerate(
        &mut self,
        loop_handle: &calloop::LoopHandle<'static, crate::wl::app::App>,
    ) -> Vec<PathBuf> {
        let mut opened = Vec::new();
        for (path, _raw) in enumerate() {
            if self.paths.values().any(|p| p == &path) {
                continue;
            }
            match self.open_device(&path, loop_handle) {
                Ok(idx) => {
                    opened.push(path.clone());
                    eprintln!("[overlayd] pad: opened {path:?} as device {idx}");
                }
                Err(e) => {
                    eprintln!("[overlayd] pad: skip {path:?}: {e}");
                }
            }
        }
        opened
    }

    fn open_device(
        &mut self,
        path: &std::path::Path,
        loop_handle: &calloop::LoopHandle<'static, crate::wl::app::App>,
    ) -> Result<u8, Box<dyn std::error::Error>> {
        // Open non-blocking so calloop's poll never blocks on read.
        let fd = fcntl::open(
            path,
            OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            StatMode::empty(),
        )?;
        let device = Device::from_fd(fd)?;

        // Only track devices that look like gamepads (have a key or abs axis).
        let has_keys = device
            .supported_keys()
            .is_some_and(|k| k.into_iter().next().is_some());
        let has_abs = device
            .supported_absolute_axes()
            .is_some_and(|a| a.into_iter().next().is_some());
        if !has_keys && !has_abs {
            return Err("not an input device with keys/axes".into());
        }

        let index = self.next_index;
        self.next_index += 1;

        // Dup the FD so calloop can poll it independently of the Device.
        let borrowed = unsafe { BorrowedFd::borrow_raw(device.as_raw_fd()) };
        let dup_fd = dup(borrowed)?;
        let summon = SummonDetector::new();

        let token = loop_handle.insert_source(
            Generic::new(dup_fd, Interest::READ, Mode::Level),
            move |_readiness, _raw_fd, app: &mut crate::wl::app::App| {
                // Collect events first, releasing the device borrow before
                // calling back into App (which borrows pad_manager again).
                let events = {
                    let Some(device_state) = app.pad_manager_mut().devices.get_mut(&index) else {
                        return Ok(PostAction::Continue);
                    };
                    match device_state.fetch_events() {
                        Ok(events) => events.collect::<Vec<_>>(),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            return Ok(PostAction::Continue);
                        }
                        Err(e) => {
                            eprintln!("[overlayd] pad device {index} error: {e}");
                            return Ok(PostAction::Continue);
                        }
                    }
                };

                for ev in events {
                    match ev.destructure() {
                        EventSummary::Key(_, code, value) => {
                            let pressed = value == 1;
                            let released = value == 0;
                            if pressed || released {
                                let code = code.0;
                                let kind = if pressed {
                                    PadEventKind::ButtonPressed(code)
                                } else {
                                    PadEventKind::ButtonReleased(code)
                                };
                                let ev = core::Event::Pad(core::PadEvent {
                                    device_index: index,
                                    kind,
                                });
                                let fired = app
                                    .pad_manager_mut()
                                    .state
                                    .get_mut(&index)
                                    .map(|s| s.summon.button(code, pressed))
                                    .unwrap_or(false);
                                if fired {
                                    app.handle_pad_event(core::Event::Toggle);
                                } else {
                                    app.handle_pad_event(ev);
                                }
                            }
                        }
                        EventSummary::AbsoluteAxis(_, code, value) => {
                            let code = code.0;
                            let kind = PadEventKind::AxisMoved(code, value.clamp(-127, 127) as i8);
                            let ev = core::Event::Pad(core::PadEvent {
                                device_index: index,
                                kind,
                            });
                            app.handle_pad_event(ev);
                        }
                        _ => {}
                    }
                }
                Ok(PostAction::Continue)
            },
        )?;
        self.tokens.insert(index, token);

        self.devices.insert(index, device);
        self.paths.insert(index, path.to_path_buf());
        self.state.insert(index, PadDeviceState { summon });
        Ok(index)
    }

    /// Remove a device on hotplug disconnect.
    #[allow(dead_code)]
    pub fn remove_device(
        &mut self,
        loop_handle: &calloop::LoopHandle<'static, crate::wl::app::App>,
        index: u8,
    ) {
        if let Some(token) = self.tokens.remove(&index) {
            loop_handle.remove(token);
        }
        self.devices.remove(&index);
        self.paths.remove(&index);
        self.state.remove(&index);
    }

    #[allow(dead_code)]
    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    /// Grab all pads (EVIOCGRAB). Called when entering Interactive (M3).
    pub fn grab_all(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        for device in self.devices.values_mut() {
            device.grab()?;
        }
        Ok(())
    }

    /// Ungrab all pads. Called when leaving Interactive (M3).
    pub fn ungrab_all(&mut self) {
        for device in self.devices.values_mut() {
            let _ = device.ungrab();
        }
    }
}

impl Default for PadManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summon_select_start() {
        let mut d = SummonDetector::new();
        assert!(!d.button(KeyCode::BTN_SELECT.0, true));
        assert!(d.button(KeyCode::BTN_START.0, true));
        assert!(!d.button(KeyCode::BTN_SELECT.0, false));
        assert!(!d.button(KeyCode::BTN_START.0, false));
        assert!(!d.button(KeyCode::BTN_SELECT.0, true));
        assert!(d.button(KeyCode::BTN_START.0, true));
    }

    #[test]
    fn summon_single_button_no_fire() {
        let mut d = SummonDetector::new();
        assert!(!d.button(KeyCode::BTN_START.0, true));
        assert!(!d.button(KeyCode::BTN_SOUTH.0, true));
        assert!(!d.button(KeyCode::BTN_START.0, false));
    }
}
