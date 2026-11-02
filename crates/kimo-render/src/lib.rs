//! Mail HTML rendering. `html` is the built-in egui widget renderer;
//! `blitz_render` is the rasterized Blitz page. Both are pure Rust and neither
//! knows about the app or the window.

pub use kimo_core::storage;
pub use kimo_style::{fonts, theme};

pub mod blitz_render;
pub mod css;
pub mod http;
pub mod html;

pub use http::http_get;

#[cfg(test)]
mod render_bench;
