//! Kimo's worker side: the event/Job enums, provider adapters, the plugin host
//! and every mailbox operation. The UI crate consumes this; the engine never
//! depends on the UI.

pub use kimo_core::{error, models, storage};
pub use kimo_mail::{folders, mail, outgoing};
pub use kimo_platform::{gpus, notify, portal, wake};
pub use kimo_protocol as protocol;
pub use kimo_render::blitz_render;
pub use kimo_render::http_get;
pub use kimo_store::{db, search};
pub use kimo_style::fonts;

pub mod plugin;
pub mod provider;
pub mod sync;
