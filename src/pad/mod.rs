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
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::Instant;

use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction};
use evdev::{enumerate, AbsInfo, Device, EventSummary, KeyCode};
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

    /// Feed a button event; returns `true` when the summon button fires.
    ///
    /// The summon button is the PS/Xbox Guide button (`BTN_MODE`, evdev
    /// `0x13c`). A single press toggles the overlay.
    fn button(&mut self, code: u16, pressed: bool) -> bool {
        let bit = 1u64 << (code % 64);
        if pressed {
            self.state.buttons |= bit;
        } else {
            self.state.buttons &= !bit;
        }

        let mode = KeyCode::BTN_MODE.0;
        if pressed && code == mode && !self.combo_armed {
            self.combo_armed = true;
            return true; // summon fires once per press
        }
        if !pressed && code == mode {
            self.combo_armed = false;
        }
        false
    }
}

/// Per-device transient state (the evdev `Device` is stored separately in
/// `PadManager.devices`).
struct PadDeviceState {
    summon: SummonDetector,
    /// Last hat direction per axis code, used to send key press/release
    /// transitions instead of repeating on every event.
    last_hat: HashMap<u16, i8>,
    /// Kernel range for each absolute axis, captured at open. Used to
    /// normalize analog stick values around their real center (which is not
    /// always zero across controllers).
    axis_info: HashMap<u16, AbsInfo>,
    /// Time of the last error log for this device, used for rate limiting.
    last_error: Option<Instant>,
    /// Count of consecutive errors on this device.
    consecutive_errors: u32,
    /// True once we have detected ENODEV and removed this device from the
    /// poll loop; prevents further error logging for it.
    device_gone: bool,
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

        // Capture each axis's kernel range once, so analog sticks can be
        // normalized to a signed value around their actual center.
        let axis_info: HashMap<u16, AbsInfo> = device
            .get_absinfo()
            .map(|info| info.map(|(code, abs)| (code.0, abs)).collect())
            .unwrap_or_default();

        let index = self.next_index;
        self.next_index += 1;

        // Dup the FD so calloop can poll it independently of the Device.
        let borrowed = unsafe { BorrowedFd::borrow_raw(device.as_raw_fd()) };
        let dup_fd = dup(borrowed)?;
        let summon = SummonDetector::new();

        let path_clone = path.to_path_buf();
        let token = loop_handle.insert_source(
            Generic::new(dup_fd, Interest::READ, Mode::Level),
            move |_readiness, _raw_fd, app: &mut crate::wl::app::App| {
                // Try to fetch events. The Device borrow from fetch_events()
                // must be released before we can borrow app.pad_manager_mut()
                // again for error handling.
                let fetch_result: Result<Vec<evdev::InputEvent>, std::io::Error> = {
                    let Some(device_state) = app.pad_manager_mut().devices.get_mut(&index) else {
                        return Ok(PostAction::Continue);
                    };
                    match device_state.fetch_events() {
                        Ok(ev_iter) => Ok(ev_iter.collect::<Vec<_>>()),
                        Err(e) => Err(e),
                    }
                };

                let Ok(events) = fetch_result else {
                    let err = fetch_result.unwrap_err();
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        return Ok(PostAction::Continue);
                    }

                    let is_enodev = err.kind() == io::ErrorKind::NotFound;

                    // Update error tracking and rate-limited logging.
                    {
                        let state = app
                            .pad_manager_mut()
                            .state
                            .get_mut(&index)
                            .expect("device state exists");
                        state.consecutive_errors += 1;
                        let now = Instant::now();
                        let should_log = state.last_error.is_none_or(|last| {
                            now.duration_since(last).as_secs() >= 60
                        });

                        if should_log {
                            app
                                .pad_manager_mut()
                                .state
                                .get_mut(&index)
                                .expect("device state exists")
                                .last_error = Some(now);

                            if is_enodev {
                                eprintln!(
                                    "[overlayd] pad device {index} ({path_clone:?}) disconnected: {err}"
                                );
                            } else {
                                eprintln!(
                                    "[overlayd] pad device {index} ({path_clone:?}) error: {err}"
                                );
                            }
                        }
                    }

                    // On ENODEV, remove this device from tracking so we stop
                    // retrying a dead FD.
                    if is_enodev {
                        app.pad_manager_mut().tokens.remove(&index);
                        app.pad_manager_mut().devices.remove(&index);
                        app.pad_manager_mut().paths.remove(&index);
                        app.pad_manager_mut().state.remove(&index);
                        // Tell calloop to remove this source from the event loop.
                        return Ok(PostAction::Remove);
                    }

                    return Ok(PostAction::Continue);
                };

                // Device is alive again; reset error tracking and log
                // reconnection if we had previously marked it as gone.
                let was_gone = {
                    let state = app
                        .pad_manager_mut()
                        .state
                        .get(&index)
                        .expect("device state exists");
                    state.device_gone
                };
                if was_gone {
                    let old = app
                        .pad_manager_mut()
                        .state
                        .remove(&index)
                        .expect("device state exists");
                    app.pad_manager_mut().state.insert(
                        index,
                        PadDeviceState {
                            summon: old.summon,
                            last_hat: old.last_hat,
                            axis_info: old.axis_info,
                            last_error: None,
                            consecutive_errors: 0,
                            device_gone: false,
                        },
                    );
                    eprintln!(
                        "[overlayd] pad device {index} ({path_clone:?}) reconnected"
                    );
                }

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
                            // Left analog stick axes (ABS_X/ABS_Y) are
                            // normalized around the kernel-reported center so a
                            // controller whose range is e.g. 0..=255 still
                            // yields 0 at rest. Other axes (notably the D-pad
                            // hat) keep the plain clamp.
                            let normalized = if matches!(code, 0x00 | 0x01) {
                                let info = app
                                    .pad_manager_mut()
                                    .state
                                    .get(&index)
                                    .and_then(|s| s.axis_info.get(&code).copied());
                                normalize_axis(info, value)
                            } else {
                                value.clamp(-127, 127) as i8
                            };
                            let kind = PadEventKind::AxisMoved(code, normalized);
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
        self.state.insert(
            index,
            PadDeviceState {
                summon,
                last_hat: HashMap::new(),
                axis_info,
                last_error: None,
                consecutive_errors: 0,
                device_gone: false,
            },
        );
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

    /// Check if an axis reading represents a state transition (press or
    /// release). Returns `Some(true)` on press, `Some(false)` on release,
    /// `None` if no change.
    #[allow(dead_code)]
    pub fn check_axis_transition(&mut self, index: u8, code: u16, value: i8) -> Option<bool> {
        let state = self.state.get_mut(&index)?;
        let prev = state.last_hat.insert(code, value);
        match (prev, value) {
            // Neutral to direction = press (includes first call from None).
            (None, v) if v != 0 => Some(true),
            (Some(0), v) if v != 0 => Some(true),
            // Direction to neutral = release.
            (Some(-1), 0) => Some(false),
            (Some(1), 0) => Some(false),
            // Direct opposite direction = press new direction.
            (Some(-1), 1) => Some(true),
            (Some(1), -1) => Some(true),
            // Same direction held or neutral held = no change.
            _ => None,
        }
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

/// Map a raw absolute-axis value to −127..=127 around the axis's real center.
///
/// Analog sticks do not share one range: some report −32768..=32767 centered
/// at 0, others 0..=255 centered at 128. Using the kernel-provided range keeps
/// 0 meaning "centered" regardless of device. Axes without range info fall
/// back to the old plain clamp.
fn normalize_axis(info: Option<AbsInfo>, value: i32) -> i8 {
    let Some(info) = info else {
        return value.clamp(-127, 127) as i8;
    };
    let (min, max) = (info.minimum(), info.maximum());
    if max <= min {
        return value.clamp(-127, 127) as i8;
    }
    let center = (min + max) / 2;
    let half = (max - min) / 2;
    if half <= 0 {
        return 0;
    }
    let delta = value - center;
    ((delta * 127) / half).clamp(-127, 127) as i8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summon_mode_button() {
        let mut d = SummonDetector::new();
        // Pressing BTN_MODE fires once.
        assert!(d.button(KeyCode::BTN_MODE.0, true));
        // Holding it down doesn't fire again until release.
        assert!(!d.button(KeyCode::BTN_MODE.0, true));
        // Release resets.
        assert!(!d.button(KeyCode::BTN_MODE.0, false));
        // Press again fires again.
        assert!(d.button(KeyCode::BTN_MODE.0, true));
    }

    #[test]
    fn summon_other_button_no_fire() {
        let mut d = SummonDetector::new();
        assert!(!d.button(KeyCode::BTN_START.0, true));
        assert!(!d.button(KeyCode::BTN_SOUTH.0, true));
        assert!(!d.button(KeyCode::BTN_START.0, false));
        assert!(!d.button(KeyCode::BTN_MODE.0, false));
    }

    fn test_device_state() -> PadDeviceState {
        PadDeviceState {
            summon: SummonDetector::new(),
            last_hat: HashMap::new(),
            axis_info: HashMap::new(),
            last_error: None,
            consecutive_errors: 0,
            device_gone: false,
        }
    }

    #[test]
    fn hat_transition_neutral_to_direction() {
        let mut pm = PadManager::new();
        pm.state.insert(0, test_device_state());
        // Neutral to right = press
        assert_eq!(pm.check_axis_transition(0, 0x10, 1), Some(true));
        // Still right = no change
        assert_eq!(pm.check_axis_transition(0, 0x10, 1), None);
        // Right to neutral = release
        assert_eq!(pm.check_axis_transition(0, 0x10, 0), Some(false));
    }

    #[test]
    fn hat_transition_direct_opposite() {
        let mut pm = PadManager::new();
        pm.state.insert(0, test_device_state());
        // Left = press
        assert_eq!(pm.check_axis_transition(0, 0x10, -1), Some(true));
        // Direct left to right = press right
        assert_eq!(pm.check_axis_transition(0, 0x10, 1), Some(true));
        // Direct right to left = press left
        assert_eq!(pm.check_axis_transition(0, 0x10, -1), Some(true));
    }

    #[test]
    fn hat_transition_first_call() {
        let mut pm = PadManager::new();
        pm.state.insert(0, test_device_state());
        // First call with direction (no previous state)
        assert_eq!(pm.check_axis_transition(0, 0x10, 1), Some(true));
    }

    #[test]
    fn hat_transition_neutral_to_neutral() {
        let mut pm = PadManager::new();
        pm.state.insert(0, test_device_state());
        // Neutral to neutral = no change (was falsely Some(false) before fix)
        assert_eq!(pm.check_axis_transition(0, 0x10, 0), None);
    }

    #[test]
    fn normalize_axis_centers_signed_range() {
        // Typical Xbox/8BitDo range: -32768..=32767, centered at 0.
        let info = AbsInfo::new(0, -32768, 32767, 0, 0, 0);
        assert_eq!(normalize_axis(Some(info), 0), 0);
        assert_eq!(normalize_axis(Some(info), 32767), 127);
        assert_eq!(normalize_axis(Some(info), -32768), -127);
        assert_eq!(normalize_axis(Some(info), 16000), 62);
    }

    #[test]
    fn normalize_axis_centers_unsigned_range() {
        // DS-style range: 0..=255, centered at 128 (not a zero-centered axis).
        let info = AbsInfo::new(128, 0, 255, 0, 0, 0);
        assert_eq!(normalize_axis(Some(info), 127), 0);
        assert_eq!(normalize_axis(Some(info), 255), 127);
        assert_eq!(normalize_axis(Some(info), 0), -127);
        // Resting center must land inside the navigation dead zone.
        assert!(normalize_axis(Some(info), 128).abs() <= 30);
    }

    #[test]
    fn normalize_axis_without_info_falls_back_to_clamp() {
        assert_eq!(normalize_axis(None, 5000), 127);
        assert_eq!(normalize_axis(None, -5000), -127);
        assert_eq!(normalize_axis(None, 0), 0);
    }
}
