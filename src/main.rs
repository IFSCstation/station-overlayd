//! `overlayd` — lightweight Wayland overlay daemon (PLAN.md).
//!
//! M0 spike: a static Slint screen rendered by the software renderer into a
//! `wl_shm` buffer of a `wlr-layer-shell` OVERLAY-layer surface.

mod core;
mod ipc;
mod modules;
mod pad;
mod wl;

slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    wl::app::run()
}
