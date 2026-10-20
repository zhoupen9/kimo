//! The event loop's own wake channel.
//!
//! egui's repaint callback is enough while a window is drawing, but not while
//! one is hidden: an immediate request it considers already served sends no
//! callback, and the window that was going to serve it no longer exists. The
//! tray's Show, a worker finishing while the window is hidden, and the hidden
//! auto-sync clock therefore wake the loop through here as well.

use std::sync::OnceLock;
use std::time::Duration;

use winit::event_loop::EventLoopProxy;

use crate::Wake;

static PROXY: OnceLock<EventLoopProxy<Wake>> = OnceLock::new();

/// Install the proxy the rest of the app wakes. Called once, from `main`.
pub(crate) fn set(proxy: EventLoopProxy<Wake>) {
    let _ = PROXY.set(proxy);
}

/// Wake the event loop now.
pub(crate) fn now() {
    send(Duration::ZERO);
}

/// Wake the event loop after `delay`.
pub(crate) fn after(delay: Duration) {
    send(delay);
}

fn send(delay: Duration) {
    if let Some(proxy) = PROXY.get() {
        let _ = proxy.send_event(Wake::Repaint(delay));
    }
}
