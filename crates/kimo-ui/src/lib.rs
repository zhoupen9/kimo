//! Kimo's egui application: `KimoApp` and the UI by concern. The lower
//! layers are re-exported under the names the app grew up with, so this crate
//! is the only place that has to know the split.

pub mod app;
pub mod glass;

pub use app::KimoApp;

pub(crate) use kimo_core::{models, storage};
pub(crate) use kimo_engine::{plugin, provider, sync};
pub(crate) use kimo_mail::{folders, mail, outgoing, threads};
pub(crate) use kimo_platform::{gpus, notify, tray, wake};
pub(crate) use kimo_protocol as protocol;
pub(crate) use kimo_render::{blitz_render, html};
pub(crate) use kimo_store::{db, search};
pub(crate) use kimo_style::{fonts, theme};
