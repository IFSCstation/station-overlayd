//! Custom Slint `Platform` rendering into `wl_shm` (PLAN.md "Core decision").
//!
//! No winit, no GL: Slint's `SoftwareRenderer` draws straight into the
//! shared-memory buffer the compositor reads.

use std::cell::Cell;
use std::rc::Rc;

use slint::platform::software_renderer::{
    PremultipliedRgbaColor, RepaintBufferType, SoftwareRenderer, TargetPixel,
};
use slint::platform::{Key, Platform, PlatformError, Renderer, WindowAdapter, WindowEvent};
use slint::{PhysicalSize, Window, WindowSize};

/// One pixel in Wayland `wl_shm` `Argb8888` format.
///
/// On little-endian the four bytes in memory are B,G,R,A, i.e. the `u32`
/// value is `0xAARRGGBB` — the same layout Slint's premultiplied colors use,
/// so there is no byte swapping to do; only the blending math below matters.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Argb8888(u32);

impl TargetPixel for Argb8888 {
    fn blend(&mut self, src: PremultipliedRgbaColor) {
        // dst = src + dst * (1 - src.a), same u16 math as Slint's own
        // PremultipliedRgbaColor::blend (u16 cannot overflow: 255 * 255).
        let inv = (u8::MAX - src.alpha) as u16;
        let r = ((self.0 >> 16) & 0xff) as u16;
        let g = ((self.0 >> 8) & 0xff) as u16;
        let b = (self.0 & 0xff) as u16;
        let a = (self.0 >> 24) as u16;
        self.0 = ((src.alpha as u16 + a - a * src.alpha as u16 / 255) as u32) << 24
            | ((r * inv / 255 + src.red as u16) as u32) << 16
            | ((g * inv / 255 + src.green as u16) as u32) << 8
            | (b * inv / 255 + src.blue as u16) as u32;
    }

    fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(0xff00_0000 | (red as u32) << 16 | (green as u32) << 8 | blue as u32)
    }

    /// The overlay root must stay transparent everywhere the UI doesn't
    /// paint, so the desktop shows through.
    fn background() -> Self {
        Self(0)
    }
}

/// Window adapter for the single overlay window: owns the `SoftwareRenderer`
/// and mirrors the compositor-provided size into Slint.
pub struct OverlayWindow {
    window: Window,
    renderer: SoftwareRenderer,
    size: Cell<PhysicalSize>,
    needs_redraw: Cell<bool>,
}

impl OverlayWindow {
    pub fn new() -> Rc<Self> {
        Rc::new_cyclic(|weak: &std::rc::Weak<Self>| {
            // `Weak<Self>` coerces to `Weak<dyn WindowAdapter>` (CoerceUnsized).
            let window_weak: std::rc::Weak<dyn WindowAdapter> = weak.clone();
            Self {
                window: Window::new(window_weak),
                // `NewBuffer` makes Slint fully repaint the dirty region on
                // every render() call. Combined with `needs_redraw` gating
                // (draw() only runs when Slint reports dirty state), this
                // gives the same "only repaint when something changed"
                // behavior as ReusedBuffer, but avoids ReusedBuffer's
                // dirty-tracking quirk where transparent root areas are
                // considered already-correct and skipped on the first paint
                // (PLAN.md §2 partial redraw; deviation noted in M0 report).
                renderer: SoftwareRenderer::new_with_repaint_buffer_type(
                    RepaintBufferType::NewBuffer,
                ),
                size: Cell::new(PhysicalSize::new(0, 0)),
                needs_redraw: Cell::new(false),
            }
        })
    }

    /// True when Slint has new content that nothing has rendered yet.
    pub fn needs_redraw(&self) -> bool {
        self.needs_redraw.get()
    }

    /// Render if there is something to draw; returns whether it drew.
    pub fn draw_if_needed(&self, render: impl FnOnce(&SoftwareRenderer)) -> bool {
        if self.needs_redraw.replace(false) {
            render(&self.renderer);
            true
        } else {
            false
        }
    }
}

impl WindowAdapter for OverlayWindow {
    fn window(&self) -> &Window {
        &self.window
    }

    fn size(&self) -> PhysicalSize {
        self.size.get()
    }

    fn set_size(&self, size: WindowSize) {
        let scale = self.window.scale_factor();
        self.size.set(size.to_physical(scale));
        // Propagate to the view tree; this is what marks the window dirty.
        self.window.dispatch_event(WindowEvent::Resized {
            size: size.to_logical(scale),
        });
    }

    fn request_redraw(&self) {
        // Only a flag: Slint forbids querying render state here. The Wayland
        // side converts it into a frame callback (wl layer request).
        self.needs_redraw.set(true);
    }

    fn renderer(&self) -> &dyn Renderer {
        &self.renderer
    }
}

/// Dispatch a key event into Slint's view tree (M3 nav translation).
impl OverlayWindow {
    pub fn dispatch_key(&self, key: Key, pressed: bool) {
        let event = if pressed {
            WindowEvent::KeyPressed { text: key.into() }
        } else {
            WindowEvent::KeyReleased { text: key.into() }
        };
        self.window.dispatch_event(event);
    }
}

/// The Slint platform: hands out the one overlay window adapter.
pub struct OverlayPlatform(Rc<OverlayWindow>);

impl Platform for OverlayPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        // M0: one window. Later milestones may return per-mode windows.
        Ok(self.0.clone())
    }
}

/// Install the custom platform; must run before any component is created.
pub fn install() -> Result<Rc<OverlayWindow>, PlatformError> {
    supply_fonts()?;
    let window = OverlayWindow::new();
    slint::platform::set_platform(Box::new(OverlayPlatform(window.clone())))
        .map_err(PlatformError::SetPlatformError)?;
    Ok(window)
}

/// Give Slint a font collection that contains only our embedded fonts.
///
/// PLAN.md: "Embed a single font; do not depend on fontconfig." With
/// `system_fonts: false` the runtime never performs system font discovery,
/// so fontconfig is never even dlopened. Imported `.slint` fonts are added
/// by Slint on top of this collection.
///
/// Two fonts are embedded:
///   - Roboto-Regular: the UI text font (mapped to the SansSerif generic family)
///   - MaterialSymbolsRounded: a subsetted icon font (3 glyphs: home, speaker,
///     close) used by the bottom bar buttons. Referenced by name from Slint.
fn supply_fonts() -> Result<(), PlatformError> {
    use slint::fontique_012::fontique;

    let mut collection = fontique::Collection::new(fontique::CollectionOptions {
        system_fonts: false,
        ..Default::default()
    });

    // Roboto — the UI text font.
    let roboto_data = include_bytes!("../../ui/fonts/Roboto-Regular.ttf");
    let roboto_fonts = collection.register_fonts(
        fontique::Blob::new(std::sync::Arc::new(roboto_data.to_vec())),
        None,
    );
    collection.set_generic_families(
        fontique::GenericFamily::SansSerif,
        roboto_fonts.iter().map(|(family_id, _)| *family_id),
    );

    // Material Symbols Rounded — subsetted icon font (home, speaker, close).
    // Registered by name so Slint can resolve `font-family: "Material Symbols Rounded"`.
    let icons_data = include_bytes!("../../ui/fonts/MaterialSymbolsRounded.ttf");
    collection.register_fonts(
        fontique::Blob::new(std::sync::Arc::new(icons_data.to_vec())),
        None,
    );

    // Only fails if a Slint context already exists — impossible this early.
    slint::fontique_012::set_shared_collection(collection)
}
