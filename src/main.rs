//! Kimo — a pure-Rust email client (IMAP & POP3) with an egui interface.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod blitz_render;
mod css;
mod db;
mod error;
mod folders;
mod fonts;
mod glass;
mod gpu;
mod html;
mod mail;
mod models;
mod protocol;
// Test-only measurement harness for the two HTML renderers; see the file.
#[cfg(test)]
mod render_bench;
mod search;
mod storage;
mod sync;
mod theme;
mod tray;

use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// What asks the loop for another frame: egui's repaint requests, which can
/// come from any thread — the tray, a sync job, an animation.
#[derive(Debug)]
enum Wake {
    /// Draw again after this long, zero meaning as soon as the loop comes up
    /// for air.
    Repaint(Duration),
}

/// The window, its GPU context, egui's input state and the app. All of it is
/// made in `resumed`, because a window needs the event loop to be running.
struct Kimo {
    egui_ctx: egui::Context,
    window: Option<Arc<Window>>,
    state: Option<egui_winit::State>,
    gpu: Option<gpu::Gpu>,
    app: Option<app::KimoApp>,
    /// What egui knows about the window, for `process_viewport_commands`.
    viewport: egui::ViewportInfo,
    /// Clipboard actions egui asked for, which go back to it as input events.
    actions: Vec<egui_winit::ActionRequested>,
    /// When egui asked for the next frame, if it is not now.
    redraw_at: Option<Instant>,
    /// Wakes the loop for a repaint egui wants later than right now.
    proxy: winit::event_loop::EventLoopProxy<Wake>,
}

fn main() -> Result<(), winit::error::EventLoopError> {
    // Pure-Rust TLS: install Kimo's crypto provider process-wide so
    // ClientConfig::builder() picks it up — rustls' `ring` provider plus the
    // finite-field DH groups and `DHE-RSA` suites of `protocol::ffdhe`, which
    // servers like Sina's need (the default provider, aws-lc-rs, is a C
    // library and intentionally not compiled in).
    let _ = protocol::ffdhe::crypto_provider().install_default();

    let event_loop = EventLoop::<Wake>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut kimo = Kimo {
        egui_ctx: egui::Context::default(),
        window: None,
        state: None,
        gpu: None,
        app: None,
        viewport: egui::ViewportInfo::default(),
        actions: Vec::new(),
        redraw_at: None,
        proxy,
    };
    event_loop.run_app(&mut kimo)
}

impl ApplicationHandler<Wake> for Kimo {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(window_attributes())
                .expect("kimo: cannot open a window"),
        );
        // Any thread may ask egui for a frame, so the request has to reach the
        // event loop: a redraw is immediate, a delay becomes a timer.
        let proxy = self.proxy.clone();
        let waker = window.clone();
        self.egui_ctx.set_request_repaint_callback(move |info| {
            // An immediate repaint is a redraw; a delayed one becomes a timer
            // in `about_to_wait`. A loop that will not take the message (it is
            // shutting down) is woken directly rather than losing the frame.
            let scheduled =
                !info.delay.is_zero() && proxy.send_event(Wake::Repaint(info.delay)).is_ok();
            if !scheduled {
                waker.request_redraw();
            }
        });

        let state = egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let gpu = gpu::Gpu::new(window.clone(), event_loop.owned_display_handle());
        let app = app::KimoApp::new(&self.egui_ctx);

        self.state = Some(state);
        self.gpu = Some(gpu);
        self.app = Some(app);
        self.window = Some(window);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: Wake) {
        match event {
            Wake::Repaint(delay) => {
                let at = Instant::now() + delay;
                self.redraw_at = Some(self.redraw_at.map_or(at, |current| current.min(at)));
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::RedrawRequested) {
            self.redraw(event_loop);
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state.on_window_event(&window, &event).repaint {
            window.request_redraw();
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size);
                }
                window.request_redraw();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Every repaint egui wants goes through the request callback, so all
        // this loop has to do is honour the timer it was given: an animation
        // (a spinner, a fading title bar) keeps asking for frames and is drawn
        // at the pace of the vsynced presents, and an idle Kimo sleeps here
        // until the next event.
        // A deadline that has not come round yet is left in place: this runs
        // on every wake-up, and a loop that dropped the timer here would sleep
        // through the frame egui asked for.
        let control = match self.redraw_at {
            Some(at) if at > Instant::now() => ControlFlow::WaitUntil(at),
            Some(_) => {
                self.redraw_at = None;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                ControlFlow::Wait
            }
            None => ControlFlow::Wait,
        };
        event_loop.set_control_flow(control);
    }
}

impl Kimo {
    /// One frame: egui runs the app, the GPU draws what it drew (see
    /// `crate::gpu`), and what egui wants done to the window — resize it, drag
    /// it, close it — comes back as viewport commands.
    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(window), Some(state), Some(gpu), Some(app)) = (
            self.window.clone(),
            self.state.as_mut(),
            self.gpu.as_mut(),
            self.app.as_mut(),
        ) else {
            return;
        };
        let viewports = gpu.render(&window, state, app, event_loop);

        let mut actions = std::mem::take(&mut self.actions);
        for (_, output) in viewports {
            egui_winit::process_viewport_commands(
                &self.egui_ctx,
                &mut self.viewport,
                output.commands,
                &window,
                &mut actions,
            );
        }
        // Cut, copy and paste are egui's way of asking for the clipboard: this
        // side does the OS part and feeds the result back in as events.
        for action in actions.drain(..) {
            match action {
                egui_winit::ActionRequested::Cut => {
                    state.egui_input_mut().events.push(egui::Event::Cut);
                }
                egui_winit::ActionRequested::Copy => {
                    state.egui_input_mut().events.push(egui::Event::Copy);
                }
                egui_winit::ActionRequested::Paste => {
                    if let Some(text) = state.clipboard_text() {
                        // X11 and Windows clipboards end lines the other way.
                        let text = text.replace("\r\n", "\n");
                        if !text.is_empty() {
                            state.egui_input_mut().events.push(egui::Event::Paste(text));
                        }
                    }
                }
                // Nothing in the app asks for a screenshot; if something ever
                // does, this is where the frame would have to be read back.
                egui_winit::ActionRequested::Screenshot(_) => {}
            }
        }
        self.actions = actions;
    }
}

/// The window Kimo draws its own title bar into: undecorated, 1280×800, and
/// named so the compositor can match window rules against it.
fn window_attributes() -> winit::window::WindowAttributes {
    #[allow(unused_mut)]
    let mut attributes = Window::default_attributes()
        .with_title("Kimo")
        .with_decorations(false) // the custom title bar is drawn in app.rs
        .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));
    #[cfg(target_os = "linux")]
    {
        use winit::platform::wayland::WindowAttributesExtWayland;
        use winit::platform::x11::WindowAttributesExtX11;
        // Wayland app_id and X11 class. winit only calls
        // xdg_toplevel.set_app_id when this is set, so without it compositors
        // (niri, sway, …) report an empty App ID and window rules cannot match.
        // Matches the XDG directory names (and the kimo.desktop basename).
        attributes = WindowAttributesExtWayland::with_name(attributes, "kimo", "kimo");
        attributes = WindowAttributesExtX11::with_name(attributes, "kimo", "kimo");
    }
    attributes
}
