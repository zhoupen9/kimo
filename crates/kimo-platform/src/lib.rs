//! The desktop side of Kimo: the session bus, the XDG file portal, the tray,
//! notifications, the event-loop wake channel and the machine's graphics
//! cards. This crate is below the UI: it may be called from it, never the
//! other way around.

pub use kimo_core::{error, storage};
pub use kimo_style::theme;

pub mod dbus;
pub mod gpus;
pub mod notify;
pub mod portal;
pub mod tray;
pub mod wake;
