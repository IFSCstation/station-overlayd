//! Wayland + Slint glue layer (PLAN.md §2). The only place allowed to use
//! Wayland, calloop and Slint types; `src/core/` stays free of all three.

pub mod app;
pub mod platform;
