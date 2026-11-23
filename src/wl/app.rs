//! Wayland wiring: one `wlr-layer-shell` surface on the OVERLAY layer, fed
//! by a single calloop event loop (PLAN.md §2).
//!
//! M0 scope: static screen. The surface is created once, rendered on frame
//! callbacks only when Slint reports dirty state, and left idle (no frame
//! callbacks, no timers) otherwise.

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

use crate::wl::platform::{Argb8888, OverlayWindow};

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

    layer: LayerSurface,
    pool: Option<SlotPool>,
    buffer: Option<Buffer>,

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
    /// The platform-independent state machine (PLAN.md §1). In M0 only
    /// `SurfaceConfigured` is fed to it; IPC/pad events arrive in M1/M2.
    state: crate::core::State,
}

/// Connect, bind protocols, create the overlay surface, install the Slint
/// platform and run the single event loop.
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

    // --- the overlay surface -------------------------------------------
    let surface = compositor.create_surface(&qh);
    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("overlayd"), None);
    layer.set_anchor(Anchor::TOP | Anchor::LEFT);
    layer.set_size(WIDTH, HEIGHT);
    // Never take keyboard focus (PLAN.md §1 Hud state).
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    // An *empty* input region makes the surface click-through: the default
    // input region is the whole surface (wayland.xml, wl_surface).
    let input_region = Region::new(&compositor).map_err(|e| format!("region: {e}"))?;
    layer.set_input_region(Some(input_region.wl_region()));
    drop(input_region); // copy semantics: safe to destroy right away
                        // Initial commit with no buffer: the compositor answers with configure.
    layer.commit();

    // --- Slint: embedded font, custom platform, then the UI --------------
    let window = crate::wl::platform::install()?;
    let ui = crate::Overlay::new()?;
    ui.window().show()?; // keeps the component alive while visible
    drop(ui);

    let mut app = App {
        _compositor: compositor,
        _layer_shell: layer_shell,
        shm,
        registry_state: RegistryState::new(&globals),
        output_state,
        layer,
        pool: None,
        buffer: None,
        window,
        configured: false,
        frame_pending: false,
        qh: qh.clone(),
        loop_handle,
        timer: None,
        signal,
        state: crate::core::State::new(),
    };
    app.sync_slint_timer();
    event_loop.run(None, &mut app, |_| {})?;
    Ok(())
}

impl App {
    /// Feed one event to the core state machine and execute the commands it
    /// returns. In M0 only `SurfaceConfigured` is ever fed; the Show/Hide
    /// arms for pad/IPC events are exercised once M1/M2 land.
    fn dispatch_core(&mut self, ev: crate::core::Event) {
        for cmd in self.state.handle(&ev) {
            match cmd {
                // M0: the surface exists from startup and is always in the
                // click-through Hud state; M1 creates/destroys it here.
                crate::core::Command::Show(_) | crate::core::Command::Redraw => {
                    self.request_frame_if_dirty();
                }
                // M1: destroy surface + shm pool (PLAN.md §2).
                crate::core::Command::Hide => {}
                // M3: EVIOCGRAB on the pads (PLAN.md §3).
                crate::core::Command::GrabPads(_) => {}
            }
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
            let surface = self.layer.wl_surface().clone();
            surface.frame(&self.qh, FrameCallbackData(surface.clone()));
            self.frame_pending = true;
            // The frame request is double-buffered: commit applies it (and,
            // right after configure, the pending ack_configure).
            self.layer.commit();
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
                // The compositor still reads our only buffer (single-buffered,
                // PLAN.md: minimal memory). Ask for another frame callback.
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

                    let surface = self.layer.wl_surface().clone();
                    let drew = self.window.draw_if_needed(|renderer| {
                        // Only dirty regions are rendered (partial redraw)
                        // and returned as damage.
                        let region = renderer.render(pixels, stride);
                        for (pos, sz) in region.iter() {
                            surface.damage_buffer(pos.x, pos.y, sz.width as i32, sz.height as i32);
                        }
                    });
                    if drew {
                        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
                        self.frame_pending = true;
                        // Available canvas implies the slot is inactive, so
                        // this cannot fail with AlreadyActive.
                        buffer.attach_to(&surface)?;
                        self.layer.commit();
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
            let surface = self.layer.wl_surface().clone();
            surface.frame(&self.qh, FrameCallbackData(surface.clone()));
            self.frame_pending = true;
            self.layer.commit();
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
                // This source is dropped below; sync re-arms if more timers
                // remain.
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
        // Marks the view tree dirty; the actual render waits for a frame
        // callback (PLAN.md §2). Goes through `Window::set_size` so the
        // adapter receives a `WindowSize` and dispatches `Resized`.
        self.window.window().set_size(new_size);
        // `set_size` alone may not mark the window dirty if Slint had already
        // auto-sized to content; explicitly request a redraw so the first
        // frame renders at the requested size.
        self.window.window().request_redraw();
        self.dispatch_core(crate::core::Event::SurfaceConfigured);
        // Kick off the initial render directly: KWin (and some compositors)
        // only deliver frame callbacks once a buffer has been attached, so
        // waiting for one would deadlock the first paint. Subsequent
        // redraws are driven by frame callbacks via `CompositorHandler`.
        let _ = self.draw();
        self.slint_tick();
        // sctk has already sent ack_configure; commit applies it even when
        // nothing else is pending.
        self.layer.commit();
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    // Output hotplug/scale tracking is M2 (PLAN.md §3); for M0 the layer
    // surface just sits at its anchored position.
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
    // No runtime global hotplug handling yet (seat/output hotplug is M2).
    smithay_client_toolkit::registry_handlers![OutputState];
}

// One delegate for all sctk protocol objects (registry is separate).
smithay_client_toolkit::delegate_dispatch2!(App);
smithay_client_toolkit::delegate_registry!(App);
