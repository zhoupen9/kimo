//! Jimao — a pure-Rust email client (IMAP & POP3) with an egui interface.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod error;
mod html;
mod mail;
mod models;
mod protocol;
mod storage;
mod sync;

fn main() -> eframe::Result {
    // Pure-Rust TLS: install rustls' `ring` crypto provider process-wide so
    // ClientConfig::builder() picks it up (the default provider, aws-lc-rs,
    // is a C library and intentionally not compiled in).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Jimao")
            .with_decorations(false) // custom titlebar drawn in app.rs
            .with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Jimao",
        options,
        Box::new(|cc| Ok(Box::new(app::JimaoApp::new(cc)))),
    )
}
