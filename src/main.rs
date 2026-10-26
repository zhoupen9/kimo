//! Kimo — a pure-Rust email client (IMAP & POP3 in, SMTP out) with an egui
//! interface.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod blitz_render;
mod css;
mod db;
mod dbus;
mod error;
mod folders;
mod fonts;
mod glass;
mod gpu;
mod gpus;
mod html;
mod mail;
mod models;
mod notify;
mod outgoing;
mod portal;
mod protocol;
mod threads;
// Test-only measurement harness for the two HTML renderers; see the file.
#[cfg(test)]
mod render_bench;
mod search;
mod storage;
mod sync;
mod theme;
mod tray;
mod trim;
mod wake;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
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

/// The window, its GPU context, egui's input state and the app. The app and
/// the GPU renderer outlive the window: Wayland cannot hide a toplevel, so
/// minimize- or close-to-tray drops the window (and its surface) and a tray
/// Show opens a new one, keeping the mail state, the compose draft and every
/// uploaded texture.
struct Kimo {
    egui_ctx: egui::Context,
    window: Option<Arc<Window>>,
    state: Option<egui_winit::State>,
    gpu: Option<gpu::Gpu>,
    app: Option<app::KimoApp>,
    /// The window egui's repaint callback may ask for a redraw on. It is
    /// cleared when the window is dropped, so the callback does not keep a
    /// hidden window alive.
    window_slot: Arc<Mutex<Option<Arc<Window>>>>,
    /// The size the next window opens at: the one the last window had when it
    /// was hidden, or the startup default.
    window_size: LogicalSize<f64>,
    /// What the loop knows about the window: `egui_winit` fills it in every
    /// frame, reads commands back into it, and egui gets it as this frame's
    /// viewport input.
    viewport: egui::ViewportInfo,
    /// Clipboard actions egui asked for, which go back to it as input events.
    actions: Vec<egui_winit::ActionRequested>,
    /// When egui asked for the next frame, if it is not now.
    redraw_at: Option<Instant>,
}

fn main() -> Result<(), winit::error::EventLoopError> {
    // Pure-Rust TLS: install Kimo's crypto provider process-wide so
    // ClientConfig::builder() picks it up — rustls' `ring` provider plus the
    // finite-field DH groups and `DHE-RSA` suites of `protocol::ffdhe`, which
    // servers like Sina's need (the default provider, aws-lc-rs, is a C
    // library and intentionally not compiled in).
    let _ = protocol::ffdhe::crypto_provider().install_default();

    // Before the first allocation that could raise the allocator's mmap
    // threshold; see `crate::trim`.
    trim::init();

    // Which Vulkan drivers this process may load, decided before the event loop
    // exists: the loader reads the list as it builds the first instance, and a
    // driver it has opened stays open (`crate::gpus`, Preferences → Graphics).
    gpus::choose_drivers();

    let event_loop = EventLoop::<Wake>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    wake::set(proxy.clone());
    let egui_ctx = egui::Context::default();
    let window_slot: Arc<Mutex<Option<Arc<Window>>>> = Arc::new(Mutex::new(None));
    // Any thread may ask egui for a frame, so the request has to reach the
    // event loop: a redraw is immediate, a delay becomes a timer. With no
    // window — a hidden Wayland one — the wake still has to arrive, because a
    // tray Show is what reopens it.
    let repaint_slot = Arc::clone(&window_slot);
    let repaint_proxy = proxy.clone();
    egui_ctx.set_request_repaint_callback(move |info| {
        let scheduled =
            !info.delay.is_zero() && repaint_proxy.send_event(Wake::Repaint(info.delay)).is_ok();
        if scheduled {
            return;
        }
        let window = repaint_slot.lock().ok().and_then(|slot| slot.clone());
        if let Some(window) = window {
            window.request_redraw();
        } else {
            let _ = repaint_proxy.send_event(Wake::Repaint(Duration::ZERO));
        }
    });
    let mut kimo = Kimo {
        egui_ctx,
        window: None,
        state: None,
        gpu: None,
        app: None,
        window_slot,
        window_size: LogicalSize::new(1280.0, 800.0),
        viewport: egui::ViewportInfo::default(),
        actions: Vec::new(),
        redraw_at: None,
    };
    event_loop.run_app(&mut kimo)
}

impl ApplicationHandler<Wake> for Kimo {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            self.open_window(event_loop);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Wake) {
        if crate::tray::take_quit_request() {
            event_loop.exit();
            return;
        }
        match event {
            Wake::Repaint(delay) if delay.is_zero() => {
                let show_requested = crate::tray::take_show_request();
                if self.window.is_some() {
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                } else if show_requested {
                    self.open_window(event_loop);
                } else {
                    // A sync result, a notification, the auto-sync clock: the
                    // app keeps running with no window, but only the work that
                    // does not need pixels.
                    self.pump_hidden();
                }
            }
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
            // The compositor's close takes the same route as the title bar's
            // cross: with close-to-tray on and a tray to bring the window back,
            // the close is a hide. Only the tray's Quit ends the process then.
            WindowEvent::CloseRequested => {
                let hide = self.app.as_ref().is_some_and(|app| app.close_to_tray())
                    && crate::tray::available();
                if hide {
                    self.hide_to_tray(&window);
                } else {
                    event_loop.exit();
                }
            }
            WindowEvent::Resized(size) => {
                if size.width > 0 && size.height > 0 {
                    self.window_size = size.to_logical(window.scale_factor());
                }
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
                } else {
                    // No window to draw, but the timer may still be the
                    // auto-sync clock: pump the app's hidden work.
                    self.pump_hidden();
                }
                ControlFlow::Wait
            }
            None => ControlFlow::Wait,
        };
        event_loop.set_control_flow(control);
    }
}

impl Kimo {
    /// Make the window — at startup, or again after a Wayland hide. The app
    /// and the GPU device and renderer survive a hide; only the egui input
    /// state and the window's surface are remade.
    fn open_window(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(window_attributes(self.window_size))
                .expect("kimo: cannot open a window"),
        );
        let state = egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        match self.gpu.as_mut() {
            Some(gpu) => gpu.attach_surface(window.clone()),
            None => {
                self.gpu = Some(gpu::Gpu::new(
                    window.clone(),
                    event_loop.owned_display_handle(),
                ));
            }
        }
        if self.app.is_none() {
            self.app = Some(app::KimoApp::new(&self.egui_ctx));
        }
        self.state = Some(state);
        self.window = Some(window.clone());
        *self.window_slot.lock().unwrap() = Some(window.clone());
        window.request_redraw();
    }

    /// Drop the window without ending the process. This is Wayland's only way
    /// to hide a toplevel: the compositor has no unmap request, and niri, for
    /// one, ignores `set_minimized` outright. The tray's Show opens a new
    /// window through `open_window`.
    fn hide_window(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let size = window.inner_size();
        if size.width > 0 && size.height > 0 {
            self.window_size = size.to_logical(window.scale_factor());
        }
        // Drop the callback's strong reference first, then the surface, then
        // the window itself. Keeping the slot set would keep the hidden window
        // alive through the repaint callback.
        *self.window_slot.lock().unwrap() = None;
        if let Some(gpu) = &mut self.gpu {
            gpu.detach_surface();
        }
        self.state = None;
        self.window = None;
        self.actions.clear();
        self.redraw_at = None;
        // The deadline just cleared was the only timer a windowless Kimo had,
        // and the compositor's close reaches here without a frame to set
        // another. Pumping now puts the next auto-sync wait on the wake
        // channel (`pump_hidden`): without it the loop sleeps through every
        // interval until the window is opened again.
        self.pump_hidden();
    }

    /// Hide the window for the tray. X11 can unmap it and keep it for a later
    /// Show; Wayland has no unmap request, so the toplevel is dropped and a
    /// tray Show builds a new one through `open_window`.
    fn hide_to_tray(&mut self, window: &Window) {
        if is_wayland(window) {
            self.hide_window();
        } else {
            window.set_visible(false);
        }
    }

    /// The app's work that needs no pixels: drain what the workers sent and
    /// keep the automatic sync clock turning. The UI itself is not run, so a
    /// hidden Kimo costs no frames.
    fn pump_hidden(&mut self) {
        if let Some(app) = &mut self.app {
            app.pump_hidden();
        }
    }

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
        // What egui's own input says about the window — `i.viewport().maximized`,
        // which the title bar's maximize/restore glyph is drawn from — is
        // something `egui-winit` leaves for the caller to fill in: its
        // `take_egui_input` writes nothing into `viewports` but the scale
        // factor, and its doc says to call `update_viewport_info` first. That is
        // what eframe does every frame, and the info is also where a close is
        // recorded, so the same struct is fed to egui as this frame's input.
        egui_winit::update_viewport_info(&mut self.viewport, &self.egui_ctx, &window, false);
        state
            .egui_input_mut()
            .viewports
            .insert(egui::ViewportId::ROOT, self.viewport.clone());

        let viewports = gpu.render(&window, state, app, event_loop);
        // Minimize-to-tray asks for `Visible(false)`. X11 can unmap the
        // window; Wayland cannot, so the window is dropped after this frame.
        let visible_false = viewports.values().any(|output| {
            output
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::Visible(false)))
        });
        let hide_on_wayland = is_wayland(&window) && visible_false;
        // A Show that arrived while the window was still visible has been
        // honoured by this frame. While hiding, leave the request for the wake
        // handler: a Show that raced the hide should reopen the window, not be
        // swallowed here.
        if !hide_on_wayland {
            crate::tray::take_show_request();
        }

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
        // The one window command `egui_winit` leaves to the caller is the
        // close: it records a `ViewportEvent::Close` in the info it is handed
        // rather than ending anything, because in eframe that recording is the
        // app's chance to refuse the close (`CancelClose`). Kimo has no such
        // chance to offer, and only the loop can end itself, so the recording is
        // read here — this is the path the title bar's close button takes when
        // close-to-tray is off, while the compositor's own close arrives as
        // `WindowEvent::CloseRequested`. The tray's Quit does not send this
        // command: close-to-tray would hide the window instead of ending it,
        // and a hidden window has no viewport to receive a close anyway.
        let close_requested = self.viewport.close_requested();
        self.viewport.events.clear();
        if close_requested {
            event_loop.exit();
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
        if hide_on_wayland {
            self.hide_window();
        }
    }
}

/// The window Kimo draws its own title bar into: undecorated, 1280×800 by
/// default, and named so the compositor can match window rules against it.
/// Undecorated is also why the app draws the window's resize border
/// (`window_resize_grips`): there is no system frame left to resize it with.
/// `size` is the one the last window had when it was hidden, so reopening it
/// from the tray does not snap back to the startup size.
fn window_attributes(size: LogicalSize<f64>) -> winit::window::WindowAttributes {
    #[allow(unused_mut)]
    let mut attributes = Window::default_attributes()
        .with_title("Kimo")
        .with_decorations(false) // the custom title bar is drawn in app.rs
        // The window's corners are cut out of the frame (see
        // `gpu::CornerMask`), so the surface has to carry an alpha channel: on
        // Wayland this flag is also what stops winit from marking the whole
        // surface opaque, and on X11 it is what picks a 32-bit visual.
        .with_transparent(true)
        .with_inner_size(size);
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

/// Whether this window is a Wayland toplevel. On X11 `set_visible(false)` is
/// enough to hide, so the window is kept; on Wayland it has to be dropped.
fn is_wayland(window: &Window) -> bool {
    #[cfg(target_os = "linux")]
    {
        use winit::platform::wayland::WindowExtWayland;
        window.xdg_toplevel().is_some()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = window;
        false
    }
}
