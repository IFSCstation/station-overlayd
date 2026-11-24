//! Wayland wiring: one `wlr-layer-shell` surface on the OVERLAY layer, fed
//! by a single calloop event loop (PLAN.md §2).
//!
//! M1 scope: the surface is created lazily on `Show` and destroyed on `Hide`
//! (idle RSS ≈ 0). A Unix-socket IPC source feeds show/hide/toggle commands
//! into the core state machine.

use std::error::Error;
use std::rc::Rc;
use std::time::Duration;

use slint::platform::Key;
use slint::platform::WindowAdapter;
use slint::ComponentHandle;
use smithay_client_toolkit::compositor::{
    CompositorHandler, CompositorState, FrameCallbackData, Region,
};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{
    EventLoop, LoopHandle, LoopSignal, RegistrationToken,
};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};

use crate::core::{self, Command, Event, Mode as CoreMode};
use crate::ipc;
use crate::wl::platform::{Argb8888, OverlayWindow};
use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction};

pub struct App {
    // sctk state. Kept alive for M1, which destroys and recreates the
    // surface on hide/show (PLAN.md §2).
    _compositor: CompositorState,
    /// Kept for M1, which destroys and recreates the surface on hide/show.
    _layer_shell: LayerShell,
    shm: Shm,
    registry_state: RegistryState,
    /// Needed because `wl_surface` events carry `wl_output` enter/leave;
    /// output hotplug handling arrives in M2 (PLAN.md).
    output_state: OutputState,
    /// Configuration loaded from disk (M5).
    config: crate::config::Config,
    /// The output the visible surface was last seen on (`surface_enter`).
    /// Used to answer "how big is the screen?" with the output the overlay
    /// is actually displayed on.
    active_output: Option<wl_output::WlOutput>,
    /// Size last requested from the compositor via `set_size`. The
    /// configure fallback ("your requested size wins") uses exactly this.
    requested_size: (u32, u32),

    /// `None` when hidden (surface + buffers freed for ~0% CPU / minimal RSS).
    layer: Option<LayerSurface>,
    pool: Option<SlotPool>,
    buffer: Option<Buffer>,

    /// Slint component, kept alive across show/hide cycles. `None` only
    /// transiently during construction.
    ui: Option<crate::Overlay>,
    /// The Slint window adapter (shared with the platform).
    window: Rc<OverlayWindow>,
    /// Set once the compositor has configured the surface; drawing before
    /// that has no buffer size to work with.
    configured: bool,
    /// A `wl_surface.frame` request is in flight; it clears when the
    /// callback fires. Prevents piling up callbacks.
    frame_pending: bool,

    qh: QueueHandle<App>,
    loop_handle: LoopHandle<'static, App>,
    /// calloop source for Slint's timers (`duration_until_next_timer_update`).
    timer: Option<RegistrationToken>,
    signal: LoopSignal,
    /// calloop registration token for the IPC socket source.
    ipc_token: Option<RegistrationToken>,
    /// calloop registration token for periodic pad re-enumeration.
    pad_timer: Option<RegistrationToken>,
    /// Gamepad manager (M2). Enumerates /dev/input, opens gamepads, dispatches
    /// pad events into the core state machine.
    pad_manager: crate::pad::PadManager,
    /// Compiled-in overlay modules (M4). Gated by Cargo features.
    modules: crate::modules::ModuleRegistry,
    /// The platform-independent state machine (PLAN.md §1).
    state: core::State,
}

/// Connect, bind protocols, install the Slint platform, bind the IPC socket
/// and run the single event loop. Starts in Hidden state (no surface).
pub fn run() -> Result<(), Box<dyn Error>> {
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    let signal = event_loop.get_signal();

    // Fall back to the standard socket name when WAYLAND_DISPLAY is unset or
    // empty (e.g. started from systemd/SSH without the session environment).
    if std::env::var_os("WAYLAND_DISPLAY").is_none_or(|v| v.is_empty()) {
        std::env::set_var("WAYLAND_DISPLAY", "wayland-0");
    }
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init::<App>(&conn)?;
    let qh = event_queue.handle();
    WaylandSource::new(conn.clone(), event_queue).insert(loop_handle.clone())?;

    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| format!("wl_compositor unavailable: {e}"))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|e| format!("zwlr_layer_shell_v1 unavailable: {e}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| format!("wl_shm unavailable: {e}"))?;
    let output_state = OutputState::new(&globals, &qh);

    // --- Slint: embedded font, custom platform, component ----------------
    let window = crate::wl::platform::install()?;
    // Do NOT show yet: we start Hidden. `show()` is called on first Show.
    let ui = Some(crate::Overlay::new()?);

    // --- IPC socket ------------------------------------------------------
    let listener = ipc::bind_socket()?;
    // Non-blocking so accept() in the calloop callback never blocks.
    listener.set_nonblocking(true)?;

    let config = crate::config::load().unwrap_or_default();
    let requested_size = (config.panel.width, config.panel.height);
    let mut app = App {
        _compositor: compositor,
        _layer_shell: layer_shell,
        shm,
        registry_state: RegistryState::new(&globals),
        output_state,
        config,
        active_output: None,
        requested_size,
        layer: None,
        pool: None,
        buffer: None,
        ui,
        window,
        configured: false,
        frame_pending: false,
        qh: qh.clone(),
        loop_handle: loop_handle.clone(),
        timer: None,
        signal,
        ipc_token: None,
        pad_timer: None,
        pad_manager: crate::pad::PadManager::new(),
        modules: crate::modules::ModuleRegistry::new(),
        state: core::State::new(),
    };

    app.sync_slint_timer();

    // --- Register compiled-in modules (M4) -------------------------------
    #[cfg(any(feature = "mod-menu", feature = "mod-notify"))]
    crate::modules::register_modules(&mut app.modules);

    // --- IPC source: accept connections, read lines, dispatch commands -----
    let ipc_token = loop_handle.insert_source(
        Generic::new(listener, Interest::READ, Mode::Level),
        move |_readiness, listener, app: &mut App| {
            // Accept all pending connections (non-blocking listener).
            loop {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        if let Err(e) = ipc::handle_client(stream, app) {
                            eprintln!("overlayd: ipc client error: {e}");
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        eprintln!("overlayd: ipc accept error: {e}");
                        break;
                    }
                }
            }
            Ok(PostAction::Continue)
        },
    )?;
    app.ipc_token = Some(ipc_token);

    // --- Gamepad enumeration (M2) ---------------------------------------
    let opened = app.pad_manager.enumerate(&loop_handle);
    if !opened.is_empty() {
        eprintln!("[overlayd] pad: {} gamepad(s) opened", opened.len());
    }

    // Periodic re-enumeration timer: detects reconnected gamepads so the
    // overlay keeps working even when no pad is connected at startup.
    let pad_timer = loop_handle.insert_source(
        Timer::from_duration(std::time::Duration::from_secs(2)),
        move |_, _, app: &mut App| {
            let opened = app.pad_manager.enumerate(&app.loop_handle);
            if !opened.is_empty() {
                eprintln!("[overlayd] pad: {} gamepad(s) opened", opened.len());
            }
            TimeoutAction::ToDuration(std::time::Duration::from_secs(2))
        },
    )?;
    app.pad_timer = Some(pad_timer);

    event_loop.run(None, &mut app, |_| {})?;

    // Cleanup: remove socket file on shutdown.
    let _ = ipc::cleanup_socket();
    Ok(())
}

impl App {
    /// Handle an IPC message: route state changes to the core state machine,
    /// handle platform-level actions directly.
    pub(crate) fn handle_ipc(&mut self, msg: ipc::IpcMsg) {
        match msg {
            ipc::IpcMsg::ShowMenu
            | ipc::IpcMsg::ShowHud
            | ipc::IpcMsg::Hide
            | ipc::IpcMsg::Toggle => {
                let ev = match msg {
                    ipc::IpcMsg::ShowMenu => {
                        // Activate the menu module if compiled in.
                        self.modules.activate("menu");
                        Event::Ipc(core::IpcMsg::ShowMenu)
                    }
                    ipc::IpcMsg::ShowHud => Event::Ipc(core::IpcMsg::ShowHud),
                    ipc::IpcMsg::Hide => Event::Ipc(core::IpcMsg::Hide),
                    ipc::IpcMsg::Toggle => Event::Ipc(core::IpcMsg::Toggle),
                    _ => unreachable!(),
                };
                self.dispatch_core(ev);
            }
            ipc::IpcMsg::Notify(_text) => {
                // Handled by the notify module (M4). For now just ack.
            }
            ipc::IpcMsg::Quit => {
                self.signal.stop();
            }
        }
    }

    /// Handle a normalized pad event: route through active module first (M4),
    /// then to the core state machine. When in Interactive mode and a module
    /// is active, the module gets first crack at the event.
    pub(crate) fn handle_pad_event(&mut self, ev: core::Event) {
        let is_interactive = self.state.mode() == core::Mode::Interactive;

        // Give the active module first crack at pad events when Interactive.
        if is_interactive {
            if let core::Event::Pad(_pad) = &ev {
                if let Some(module) = self.modules.active_module() {
                    let (handled, cmd) = module.on_event(&ev);
                    if handled == crate::modules::Handled::Yes {
                        self.execute_module_command(cmd);
                        return; // module consumed the event
                    }
                }
            }
        }

        // Translate pad events to Slint key events when Interactive.
        if is_interactive {
            if let core::Event::Pad(pad) = &ev {
                match pad.kind {
                    core::PadEventKind::ButtonPressed(code)
                    | core::PadEventKind::ButtonReleased(code) => {
                        let pressed = matches!(pad.kind, core::PadEventKind::ButtonPressed(_));
                        self.translate_button(code, pressed);
                    }
                    core::PadEventKind::AxisMoved(code, value) => {
                        self.translate_axis(code, value, pad.device_index);
                    }
                }
            }
        }

        self.dispatch_core(ev);
    }

    /// Execute a command returned by a module.
    fn execute_module_command(&mut self, cmd: crate::modules::ModuleCommand) {
        match cmd {
            crate::modules::ModuleCommand::SwitchTo(id) => {
                self.switch_module(id);
            }
            crate::modules::ModuleCommand::Hide => {
                self.hide_overlay();
            }
            crate::modules::ModuleCommand::None => {}
        }
    }

    /// Translate an evdev button code to a Slint key event.
    fn translate_button(&self, code: u16, pressed: bool) {
        let key = match code {
            // Xbox/PlayStation cross button → Return (activate)
            0x130 => Key::Return, // BTN_SOUTH
            // Circle/B → Escape (back)
            0x134 => Key::Escape, // BTN_EAST
            // Start → Return
            0x13b => Key::Return, // BTN_START
            _ => return,
        };
        self.window.dispatch_key(key, pressed);
    }

    /// Translate an evdev absolute axis to directional key events.
    fn translate_axis(&mut self, code: u16, value: i8, device_index: u8) {
        let (key, _active) = match code {
            0x10 => {
                // ABS_HAT0X
                if value < 0 {
                    (Key::LeftArrow, true)
                } else if value > 0 {
                    (Key::RightArrow, true)
                } else {
                    (Key::LeftArrow, false)
                }
            }
            0x11 => {
                // ABS_HAT0Y
                if value < 0 {
                    (Key::UpArrow, true)
                } else if value > 0 {
                    (Key::DownArrow, true)
                } else {
                    (Key::UpArrow, false)
                }
            }
            _ => return,
        };
        // Only send press/release on state transitions.
        if let Some(pressed) = self
            .pad_manager
            .check_axis_transition(device_index, code, value)
        {
            if pressed {
                self.window.dispatch_key(key, true);
            } else {
                self.window.dispatch_key(key, false);
            }
        }
    }

    pub(crate) fn pad_manager_mut(&mut self) -> &mut crate::pad::PadManager {
        &mut self.pad_manager
    }

    /// Switch to a module by ID. Used by Ctx from Module::on_event.
    pub(crate) fn switch_module(&mut self, id: &str) {
        if self.modules.activate(id) {
            // Show the overlay if hidden.
            if self.state.mode() == core::Mode::Hidden {
                let _ = self.show_surface(core::Mode::Interactive);
            }
        }
    }

    /// Hide the overlay (back to Hidden state). Used by Ctx from Module::on_event.
    pub(crate) fn hide_overlay(&mut self) {
        self.modules.deactivate();
        self.handle_ipc(ipc::IpcMsg::Hide);
    }

    /// Feed one event to the core state machine and execute the commands it
    /// returns. IPC drives Show/Hide/Toggle (M1); pad events arrive in M2.
    /// Pad grab/release arrives in M3.
    fn dispatch_core(&mut self, ev: core::Event) {
        for cmd in self.state.handle(&ev) {
            match cmd {
                Command::Show(mode) => {
                    let _ = self.show_surface(mode);
                }
                Command::Hide => {
                    self.hide_surface();
                    // Release pads when hiding (M3).
                    self.pad_manager.ungrab_all();
                }
                Command::Redraw => {
                    self.request_frame_if_dirty();
                }
                Command::GrabPads(grab) => {
                    if grab {
                        let _ = self.pad_manager.grab_all();
                    } else {
                        self.pad_manager.ungrab_all();
                    }
                }
            }
        }
    }

    /// Size of the output the overlay is (or will be) shown on, in logical
    /// pixels. Prefers the output the surface has actually entered; falls
    /// back to the first output the compositor advertised.
    fn current_output_size(&self) -> Option<(u32, u32)> {
        let output = self
            .active_output
            .clone()
            .or_else(|| self.output_state.outputs().next())?;
        self.output_size(&output)
    }

    /// One output's size: xdg-output's `logical_size` when the compositor
    /// provides it, otherwise the current mode corrected for transform and
    /// scale factor. Queried on every show so a resolution change made while
    /// the overlay was hidden is picked up (PLAN.md fullscreen requirement).
    fn output_size(&self, output: &wl_output::WlOutput) -> Option<(u32, u32)> {
        let info = self.output_state.info(output)?;
        if let Some((w, h)) = info.logical_size {
            if w > 0 && h > 0 {
                return Some((w as u32, h as u32));
            }
        }
        let mode = info
            .modes
            .iter()
            .find(|m| m.current)
            .or_else(|| info.modes.first())?;
        Some(logical_size_from_mode(
            mode.dimensions,
            info.transform,
            info.scale_factor,
        ))
    }

    /// Re-query the target output's size and resize the visible surface if
    /// it changed (resolution switch while the overlay is already up).
    fn refit_surface(&mut self) {
        if self.layer.is_none() {
            return; // Hidden: the next show_surface() re-queries anyway.
        }
        let Some(size) = self.current_output_size() else {
            return;
        };
        if size == self.requested_size {
            return;
        }
        self.requested_size = size;
        let layer = self.layer.as_ref().expect("layer");
        layer.set_size(size.0, size.1);
        layer.commit();
    }

    /// Create the Wayland layer surface (if missing) and mark Slint visible.
    /// The actual render happens on the next `configure` callback.
    fn show_surface(&mut self, _mode: CoreMode) -> Result<(), Box<dyn Error>> {
        // Keep the Slint component alive while visible.
        self.ui
            .as_ref()
            .expect("ui component")
            .window()
            .show()
            .map_err(|e| format!("slint show: {e}"))?;

        if self.layer.is_some() {
            // Already visible: just request a redraw.
            self.request_frame_if_dirty();
            return Ok(());
        }

        // --- create the overlay surface ---------------------------------
        // Query the screen size right now (not from config): the user may
        // have changed resolution while the overlay was hidden.
        let size = self
            .current_output_size()
            .unwrap_or((self.config.panel.width, self.config.panel.height));
        self.requested_size = size;
        let surface = self._compositor.create_surface(&self.qh);
        let layer = self._layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some("overlayd"),
            None,
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        layer.set_size(size.0, size.1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        let input_region = Region::new(&self._compositor).map_err(|e| format!("region: {e}"))?;
        layer.set_input_region(Some(input_region.wl_region()));
        drop(input_region);
        layer.commit();

        self.layer = Some(layer);
        self.request_frame_if_dirty();
        Ok(())
    }

    /// Destroy the Wayland surface and free shm buffers (~0% CPU, minimal
    /// RSS while hidden, PLAN.md §1).
    fn hide_surface(&mut self) {
        self.layer = None;
        self.pool = None;
        self.buffer = None;
        self.configured = false;
        self.frame_pending = false;
        // Release Slint's extra strong reference on the component.
        if let Some(ui) = self.ui.as_ref() {
            let _ = ui.window().hide();
        }
    }

    /// Advance Slint's timers/animations and turn resulting dirtiness into a
    /// frame-callback request. Rendering itself only ever happens inside a
    /// frame callback (PLAN.md §2).
    fn slint_tick(&mut self) {
        slint::platform::update_timers_and_animations();
        self.request_frame_if_dirty();
        self.sync_slint_timer();
    }

    /// Ask the compositor for the next frame callback if Slint has new
    /// content and none is already in flight.
    fn request_frame_if_dirty(&mut self) {
        if self.configured && !self.frame_pending && self.window.needs_redraw() {
            let surface = self.layer.as_ref().expect("layer").wl_surface().clone();
            surface.frame(&self.qh, FrameCallbackData(surface.clone()));
            self.frame_pending = true;
            self.layer.as_ref().expect("layer").commit();
        }
    }

    /// Render the window if dirty. Called only from frame callbacks.
    fn draw(&mut self) -> Result<(), Box<dyn Error>> {
        if !self.window.needs_redraw() {
            return Ok(());
        }
        let size = self.window.size();
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        let (w, h) = (size.width as i32, size.height as i32);

        // (Re)create the shm pool + buffer when missing (size changes are
        // handled by dropping them in the configure handler).
        if self.buffer.is_none() {
            let stride = w * 4;
            let mut pool = SlotPool::new((stride * h) as usize, &self.shm)
                .map_err(|e| format!("wl_shm pool: {e}"))?;
            let (buffer, _) = pool
                .create_buffer(w, h, stride, wl_shm::Format::Argb8888)
                .map_err(|e| format!("wl_shm buffer: {e}"))?;
            self.pool = Some(pool);
            self.buffer = Some(buffer);
        }

        let mut retry_next_frame = false;
        {
            let buffer = self.buffer.as_ref().expect("buffer just created");
            let pool = self.pool.as_mut().expect("pool just created");
            match buffer.canvas(pool) {
                None => retry_next_frame = true,
                Some(canvas) => {
                    let stride = w as usize;
                    // SAFETY: SlotPool aligns slots for the buffer format
                    // (Argb8888 → 4 bytes) and the canvas is exactly
                    // w*h pixels of Argb8888 (repr(transparent) over u32).
                    let pixels = unsafe {
                        std::slice::from_raw_parts_mut(
                            canvas.as_mut_ptr().cast::<Argb8888>(),
                            stride * h as usize,
                        )
                    };

                    let surface = self.layer.as_ref().expect("layer").wl_surface().clone();
                    let drew = self.window.draw_if_needed(|renderer| {
                        let region = renderer.render(pixels, stride);
                        for (pos, sz) in region.iter() {
                            surface.damage_buffer(pos.x, pos.y, sz.width as i32, sz.height as i32);
                        }
                    });
                    if drew {
                        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
                        self.frame_pending = true;
                        buffer.attach_to(&surface)?;
                        self.layer.as_ref().expect("layer").commit();
                    }
                }
            }
        }
        if retry_next_frame {
            self.request_frame();
        }
        Ok(())
    }

    /// Request a frame callback without rendering (used when the draw must
    /// be deferred to the next compositor frame).
    fn request_frame(&mut self) {
        if !self.frame_pending {
            if let Some(layer) = &self.layer {
                let surface = layer.wl_surface().clone();
                surface.frame(&self.qh, FrameCallbackData(surface.clone()));
                self.frame_pending = true;
                layer.commit();
            }
        }
    }

    /// (Re)arm the calloop timer that drives Slint's `Timer`s.
    fn sync_slint_timer(&mut self) {
        if let Some(token) = self.timer.take() {
            self.loop_handle.remove(token);
        }
        let Some(next) = slint::platform::duration_until_next_timer_update() else {
            return;
        };
        let next = next.max(Duration::from_millis(1));
        let token = self
            .loop_handle
            .insert_source(Timer::from_duration(next), |_, _, app: &mut Self| {
                app.timer = None;
                app.slint_tick();
                TimeoutAction::Drop
            })
            .expect("insert Slint timer source");
        self.timer = Some(token);
    }
}

// --- sctk handler traits ---------------------------------------------------

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
        // Integer scale only for now (PLAN.md §2).
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        if !self.configured {
            return;
        }
        self.frame_pending = false;
        self.slint_tick();
        if let Err(e) = self.draw() {
            eprintln!("overlayd: draw failed: {e}");
            self.signal.stop();
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        // First time the surface lands on an output we know its real size;
        // re-fit in case the size was queried before outputs were bound.
        self.active_output = Some(output.clone());
        self.refit_surface();
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        // Compositor wants the surface gone (e.g. output removed).
        self.signal.stop();
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // (0, 0) means "your requested size wins" — the size we last asked
        // for via set_size (queried from the output at show/refit time).
        let (w, h) = if configure.new_size.0 == 0 || configure.new_size.1 == 0 {
            (self.requested_size.0 as i32, self.requested_size.1 as i32)
        } else {
            (configure.new_size.0 as i32, configure.new_size.1 as i32)
        };
        let new_size = slint::PhysicalSize::new(w as u32, h as u32);
        if self.window.size() != new_size {
            // Old buffers don't fit the new size; draw() recreates them.
            self.pool = None;
            self.buffer = None;
        }
        self.configured = true;
        self.window.window().set_size(new_size);
        self.window.window().request_redraw();
        // sctk has already sent ack_configure; commit applies it.
        self.layer.as_ref().expect("layer").commit();
        // Kick off the initial render directly: KWin (and some compositors)
        // only deliver frame callbacks once a buffer has been attached.
        let _ = self.draw();
        self.slint_tick();
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // Resolution/mode changed while visible: re-fit the surface.
        if self.active_output.as_ref() == Some(&output) {
            self.refit_surface();
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.active_output.as_ref() == Some(&output) {
            self.active_output = None;
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    smithay_client_toolkit::registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_dispatch2!(App);
smithay_client_toolkit::delegate_registry!(App);

/// Pure `wl_output` mode → logical size math (unit-tested): quarter-turn
/// transforms swap width/height, then the integer scale factor divides.
fn logical_size_from_mode(
    dimensions: (i32, i32),
    transform: wl_output::Transform,
    scale_factor: i32,
) -> (u32, u32) {
    use wl_output::Transform;
    let (mut w, mut h) = dimensions;
    if matches!(
        transform,
        Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
    ) {
        std::mem::swap(&mut w, &mut h);
    }
    let scale = scale_factor.max(1);
    ((w / scale).max(0) as u32, (h / scale).max(0) as u32)
}

#[cfg(test)]
mod tests {
    use super::logical_size_from_mode;
    use super::wl_output::Transform;

    #[test]
    fn plain_mode_is_logical_size() {
        assert_eq!(
            logical_size_from_mode((1920, 1080), Transform::Normal, 1),
            (1920, 1080)
        );
    }

    #[test]
    fn quarter_turns_swap_dimensions() {
        for t in [
            Transform::_90,
            Transform::_270,
            Transform::Flipped90,
            Transform::Flipped270,
        ] {
            assert_eq!(logical_size_from_mode((1920, 1080), t, 1), (1080, 1920));
        }
        // Half turn and plain flip keep the dimensions.
        for t in [Transform::_180, Transform::Flipped] {
            assert_eq!(logical_size_from_mode((1920, 1080), t, 1), (1920, 1080));
        }
    }

    #[test]
    fn scale_factor_divides() {
        assert_eq!(
            logical_size_from_mode((3840, 2160), Transform::Normal, 2),
            (1920, 1080)
        );
        assert_eq!(
            logical_size_from_mode((3840, 2160), Transform::_90, 2),
            (1080, 1920)
        );
    }

    #[test]
    fn zero_scale_does_not_divide_by_zero() {
        assert_eq!(
            logical_size_from_mode((1920, 1080), Transform::Normal, 0),
            (1920, 1080)
        );
    }
}
