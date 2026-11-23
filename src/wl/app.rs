//! Wayland wiring: one `wlr-layer-shell` surface on the OVERLAY layer, fed
//! by a single calloop event loop (PLAN.md §2).
//!
//! M1 scope: the surface is created lazily on `Show` and destroyed on `Hide`
//! (idle RSS ≈ 0). A Unix-socket IPC source feeds show/hide/toggle commands
//! into the core state machine.

use std::error::Error;
use std::rc::Rc;
use std::time::Duration;

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

/// Panel size in logical pixels (scale factor 1 for now, PLAN.md §2).
const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;

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
    /// The platform-independent state machine (PLAN.md §1).
    state: core::State,
}

/// Connect, bind protocols, install the Slint platform, bind the IPC socket
/// and run the single event loop. Starts in Hidden state (no surface).
pub fn run() -> Result<(), Box<dyn Error>> {
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    let signal = event_loop.get_signal();

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

    let mut app = App {
        _compositor: compositor,
        _layer_shell: layer_shell,
        shm,
        registry_state: RegistryState::new(&globals),
        output_state,
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
        state: core::State::new(),
    };

    app.sync_slint_timer();

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
                    ipc::IpcMsg::ShowMenu => Event::Ipc(core::IpcMsg::ShowMenu),
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

    /// Feed one event to the core state machine and execute the commands it
    /// returns. In M1, IPC drives Show/Hide/Toggle; pad events arrive in M2.
    fn dispatch_core(&mut self, ev: core::Event) {
        for cmd in self.state.handle(&ev) {
            match cmd {
                Command::Show(mode) => {
                    let _ = self.show_surface(mode);
                }
                Command::Hide => {
                    self.hide_surface();
                }
                Command::Redraw => {
                    self.request_frame_if_dirty();
                }
                Command::GrabPads(_) => {}
            }
        }
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
        let surface = self._compositor.create_surface(&self.qh);
        let layer = self._layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some("overlayd"),
            None,
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        layer.set_size(WIDTH, HEIGHT);
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
        _output: &wl_output::WlOutput,
    ) {
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
        // (0, 0) means "your requested size wins".
        let (w, h) = if configure.new_size.0 == 0 || configure.new_size.1 == 0 {
            (WIDTH as i32, HEIGHT as i32)
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
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
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
