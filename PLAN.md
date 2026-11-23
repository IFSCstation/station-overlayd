# overlayd: Overlay Daemon Architecture

Rust + Slint overlay daemon for simple Wayland compositors (labwc), targeting arm64 Linux (Raspberry Pi 4), with gamepad support. Goal: as lightweight as humanly possible, with an architecture that is simple but expandable.

## Goals and non-goals

**Goals**
- Minimal binary size, idle RSS, and CPU. Hidden state should be ~0% CPU.
- Runs on labwc and other wlroots-style compositors that support `wlr-layer-shell`.
- Gamepad-driven navigation and a gamepad summon combo.
- Simple core, with new features added as compiled-in modules.

**Non-goals**
- GPU rendering (EGL, Mesa, Skia, femtovg).
- winit or linuxkms backends.
- Async runtimes (tokio), plugin dlopen, or multi-threaded UI.
- GNOME/KDE support (no layer-shell on GNOME).

## Core decision: custom Slint platform, no winit, no GL

Slint's stock backends don't fit the constraints:

- **winit** can't create `wlr-layer-shell` surfaces and is heavy.
- **linuxkms** takes over the display directly, so it can't coexist with labwc.
- **GPU renderers** (femtovg/skia) need EGL and are overkill for an overlay on a Pi 4.

Implement `slint::platform::Platform` ourselves:

- **Surface:** `smithay-client-toolkit` (sctk) for a `zwlr_layer_shell_v1` surface on the `OVERLAY` layer.
- **Rendering:** Slint `SoftwareRenderer` into a `wl_shm` buffer (ARGB8888). No EGL, no GPU context.
- **Partial redraws:** the software renderer reports dirty regions, so a mostly static menu costs almost nothing per frame.
- **Pixel format:** a custom `TargetPixel` type or a line-by-line conversion so Slint's premultiplied RGBA matches Wayland's byte order. This is the one fiddly bit in the rendering path.

## High-level structure

```
                ┌──────────────────────────────────────────┐
                │          calloop (single thread)         │
                │  epoll over: wayland fd, evdev fds,      │
                │  inotify (hotplug), IPC socket, timers   │
                └───────┬──────────┬──────────┬────────────┘
                        │          │          │
              ┌─────────▼──┐ ┌─────▼─────┐ ┌──▼─────────┐
              │  wayland   │ │  gamepad  │ │    ipc     │
              │ layer-shell│ │ evdev +   │ │ unix sock  │
              │ shm + slint│ │ hotplug   │ │ show/hide/ │
              │ platform   │ │ grab ctl  │ │ notify     │
              └─────▲──────┘ └─────┬─────┘ └──┬─────────┘
                    │              │          │
                    │         Event enum      │
                    │              ▼          │
                ┌───┴──────────────────────────▼───┐
                │              core                │
                │  state machine + module registry │
                │  Hidden ⇄ Hud ⇄ Interactive      │
                └───────┬──────────────────────────┘
                        │ Command enum
              ┌─────────▼─────────┐
              │ modules (features)│  menu, perf HUD, notifications...
              │ each = Rust impl  │
              │ + .slint component│
              └───────────────────┘
```

**Everything runs on one thread with one event loop.** No tokio, no async runtime, no mutexes. Worker threads only appear if a module needs blocking I/O, and they talk back through a `calloop::channel`.

## Components

### 1. Core (pure Rust, zero platform deps)
- `enum Event { Pad(PadEvent), Ipc(IpcMsg), Tick, SurfaceConfigured, ... }`
- `enum Command { Show(Mode), Hide, GrabPads(bool), Redraw, ... }`
- Three-state machine:
  - **Hidden**: no surface, no buffers, no pad grab. Process sleeps in epoll at ~0% CPU.
  - **Hud**: passive, click-through surface (empty input region, no keyboard interactivity) for things like an FPS counter.
  - **Interactive**: full menu, exclusive keyboard interactivity, gamepad grabbed.
- No platform dependencies, so it is unit-testable on a desktop.

### 2. Wayland platform layer
- Layer-shell on `OVERLAY`, which sits above fullscreen windows in wlroots compositors (verify on the target labwc version).
- Destroy the surface and free the shm pool when hidden instead of just hiding. This keeps idle RSS minimal.
- Render only on a frame callback and only when Slint reports dirty regions. Slint animation timers run only while visible.
- Start with integer scale. Add `wp-fractional-scale` and `wp-viewporter` later if needed.

### 3. Gamepad layer
- Use the `evdev` crate directly rather than gilrs. It is lighter, and the kernel already standardizes gamepad codes (`BTN_SOUTH`, `BTN_START`, `ABS_HAT0X`, ...), so a small normalization table covers common pads.
- **Hotplug:** watch `/dev/input` with inotify (or udev if the extra dependency is acceptable).
- **Summon hotkey:** the daemon reads pads continuously so it can detect a combo (e.g. hold Guide, or Select+Start) even when the overlay is unfocused. Wayland won't deliver gamepad input to an unfocused client, which is why this goes through evdev.
- **Grab:** while Interactive, call `EVIOCGRAB` on the pad so the game stops receiving input; release on hide.
- **Permissions:** the user needs the `input` group or logind/seatd ACLs. Document this early; it is the most common "why doesn't it work" problem.
- **Later option:** grab always and forward via `uinput` for precise filtering. More complexity, so defer.

### 4. Slint UI and navigation
Slint has no built-in spatial gamepad navigation, so translate at the platform boundary:
- Map D-pad/stick to key events via `window.dispatch_event(WindowEvent::KeyPressed)`: arrows, Tab/Backtab, Return (A), Escape (B).
- Also expose a `global Nav { callback up(); callback accept(); ... }` for custom components that want semantic actions.
- Embed one font and avoid fontconfig. Use `default-features = false` on Slint with only the software renderer enabled (check exact feature names for the pinned version).

### 5. IPC and CLI
- Unix socket at `$XDG_RUNTIME_DIR/overlayd.sock`, line-based text protocol (`show menu`, `hide`, `toggle`, `notify "text"`).
- The same binary doubles as the CLI client (`overlayd ctl toggle`), so labwc keybinds and scripts can drive it.
- This is also the out-of-process extension point: external tools push notifications or data in without plugin loading.

### 6. Modules (the expansion mechanism)

```rust
trait Module {
    fn id(&self) -> &'static str;
    fn on_event(&mut self, ev: &Event, ctx: &mut Ctx) -> Handled;
    fn ui(&self) -> ModuleView; // which Slint component/page to show
}
```

- Modules are compiled in and gated by Cargo features (`mod-menu`, `mod-perf-hud`, `mod-notify`). You only pay for what you ship.
- Each module owns a `.slint` component plus a Rust struct. The core routes events to the topmost active module.
- No dynamic plugin loading (dlopen). IPC covers out-of-process extension.

## Crate layout

Start as a **single crate with modules**. Split into a workspace only when a boundary hurts.

```
overlayd/
├─ src/
│  ├─ main.rs         // calloop setup, wiring
│  ├─ core/           // state machine, Event/Command
│  ├─ wl/             // sctk, layer-shell, shm, slint Platform
│  ├─ pad/            // evdev, hotplug, grab, normalization
│  ├─ ipc.rs
│  └─ modules/        // menu/, perf_hud/, notify/
└─ ui/                // .slint files
```

**Dependency budget:** `smithay-client-toolkit` (with calloop), `wayland-protocols-wlr`, `evdev`, `slint` (minimal), `rustix` or `nix`, optionally `toml` for config. That's about it.

## Size and performance settings

```toml
[profile.release]
opt-level = "s"      # try "z" and "3" and measure
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
```

- Build with `target-cpu=cortex-a72`.
- Cross-compile with `cargo-zigbuild` or `cross` for `aarch64-unknown-linux-gnu`.
- Track: binary size, idle RSS, CPU while visible, CPU while hidden (should be 0%).

## Roadmap

| Phase | Goal | Done when |
|---|---|---|
| M0 | Layer-shell surface + static Slint screen via shm | A box renders over a fullscreen app on labwc |
| M1 | calloop loop + IPC show/hide/toggle | Keybind in labwc toggles it with 0% CPU idle |
| M2 | Gamepad read + summon combo | Hotplug works, combo toggles overlay |
| M3 | Pad grab + nav translation | You can navigate a menu with D-pad and A/B |
| M4 | Module trait + 2 real modules (menu, notification) | A third module takes under an hour to add |
| M5 | HUD mode, systemd user unit, config file | Starts on boot, survives pad unplug/replug |

## Risks to verify early (M0 spike)

1. **Overlay above fullscreen on labwc:** test with the actual target games or emulators.
2. **Slint custom platform API:** stable in 1.x, but confirm feature flags and `TargetPixel` handling against the pinned version.
3. **Software render cost at 1080p/4K on a Pi 4:** partial rendering should keep it fine; if a 4K overlay is full-screen and animated, constrain it to a smaller panel.
4. **Evdev permissions** on the target distro image.
5. **Existing crates:** community projects wrap Slint with layer-shell (e.g. layer-shika) but likely pull in GL rendering. Read for reference, but go custom for the lightest result.

## Open questions
- Primary use case: emulation frontend, game launcher overlay, or system menu? This decides the first two modules and the summon combo design.
