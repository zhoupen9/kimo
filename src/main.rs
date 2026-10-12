//! Kimo — a pure-Rust email client (IMAP & POP3) with an egui interface.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod blitz_render;
mod css;
mod error;
mod folders;
mod fonts;
mod html;
mod mail;
mod models;
mod protocol;
// Test-only measurement harness for the two HTML renderers; see the file.
#[cfg(test)]
mod render_bench;
mod storage;
mod sync;
mod tray;

fn main() -> eframe::Result {
    // Pure-Rust TLS: install rustls' `ring` crypto provider process-wide so
    // ClientConfig::builder() picks it up (the default provider, aws-lc-rs,
    // is a C library and intentionally not compiled in).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Kimo")
            // Wayland app_id. winit only calls xdg_toplevel.set_app_id when
            // this is set, so without it compositors (niri, sway, …) report an
            // empty App ID and window rules cannot match. Matches the XDG
            // directory names (and a future kimo.desktop basename).
            .with_app_id("kimo")
            .with_decorations(false) // custom titlebar drawn in app.rs
            .with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Kimo",
        options,
        Box::new(|cc| Ok(Box::new(app::KimoApp::new(cc)))),
    )
}
