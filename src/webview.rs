//! Optional WebKitGTK "web view" renderer for HTML mail.
//!
//! Only compiled with the `webview` feature (see `Cargo.toml`): the GTK and
//! WebKitGTK crates link C/C++ libraries, so a plain `cargo build` stays pure
//! Rust (hard rule 1). The web view cannot be embedded as a pane in the egui
//! window — WebKitGTK renders through GTK, and a Wayland compositor offers no
//! way to position a foreign child window over the eframe surface — so this
//! module owns a separate top-level window with its own GTK main loop on a
//! dedicated thread. The app talks to it over a channel; closing the window
//! only hides it, and dropping the handle shuts the thread down.
//!
//! `document()` expands `cid:` references into `data:` URIs, because the web
//! view has no access to the app's decoded inline-image textures.

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use webkit2gtk::{LoadEvent, SettingsExt, WebView as WebViewWidget, WebViewExt};

use crate::models::InlineImage;

/// How long `spawn` waits for the GTK thread to come up.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the GTK thread checks for a document to display.
const POLL_INTERVAL: Duration = Duration::from_millis(120);

enum Command {
    Show { title: String, html: String },
    Hide,
    Quit,
}

/// Handle to the web view window.
pub struct WebView {
    tx: Sender<Command>,
}

impl WebView {
    /// Start the GTK thread and create its (hidden) window. Returns `None`
    /// when GTK cannot be initialised — no display, or the GTK libraries are
    /// missing — so the caller can fall back to the simple renderer.
    pub fn spawn() -> Option<WebView> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("jimao-webview".to_string())
            .spawn(move || match gtk::init() {
                Ok(()) => {
                    let _ = ready_tx.send(true);
                    run(rx);
                }
                Err(e) => {
                    eprintln!("jimao webview: gtk init failed: {e}");
                    let _ = ready_tx.send(false);
                }
            });
        if let Err(e) = spawned {
            eprintln!("jimao webview: could not spawn the GTK thread: {e}");
            return None;
        }
        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(true) => Some(WebView { tx }),
            Ok(false) => None,
            Err(_) => {
                eprintln!("jimao webview: GTK did not start within {START_TIMEOUT:?}");
                None
            }
        }
    }

    /// Load `html` into the window, retitle it and raise it.
    pub fn show(&self, title: &str, html: &str) {
        let _ = self.tx.send(Command::Show {
            title: title.to_string(),
            html: html.to_string(),
        });
    }

    /// Hide the window; the next `show` reuses it.
    pub fn hide(&self) {
        let _ = self.tx.send(Command::Hide);
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Quit);
    }
}

/// Set once a document has finished loading; only read by the smoke test.
#[cfg(test)]
static LOAD_FINISHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// GTK thread body: build the window, then serve commands until the app drops
/// the handle.
fn run(rx: Receiver<Command>) {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Jimao");
    window.set_default_size(1000, 800);
    let view = WebViewWidget::new();
    // Mail HTML never needs scripts; the simple renderer has none either.
    if let Some(settings) = WebViewExt::settings(&view) {
        settings.set_enable_javascript(false);
    }
    view.connect_load_changed(|_, event| {
        if event == LoadEvent::Finished {
            #[cfg(test)]
            LOAD_FINISHED.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });
    window.add(&view);
    // Closing the window hides it; the app decides when to bring it back.
    window.connect_delete_event(|window, _| {
        window.hide();
        glib::Propagation::Stop
    });

    // Poll the channel here: no GTK call ever happens on the app's thread.
    let rx = RefCell::new(rx);
    glib::timeout_add_local(POLL_INTERVAL, move || loop {
        match rx.borrow().try_recv() {
            Ok(Command::Show { title, html }) => {
                window.set_title(&title);
                view.load_html(&html, Some("about:blank"));
                window.show_all();
                window.present();
            }
            Ok(Command::Hide) => window.hide(),
            Ok(Command::Quit) | Err(TryRecvError::Disconnected) => {
                gtk::main_quit();
                return glib::ControlFlow::Break;
            }
            Err(TryRecvError::Empty) => return glib::ControlFlow::Continue,
        }
    });

    gtk::main();
}

/// The document handed to the web view: inline `cid:` images expanded and a
/// small style block so large images cannot overflow the window.
pub fn document(html: &str, inline: &[InlineImage]) -> String {
    inject_style(
        &inline_cid_images(html, inline),
        "img{max-width:100%;height:auto}",
    )
}

/// Rewrite `cid:` references to this message's inline attachments as `data:`
/// URIs. References to unknown ids — or where a known id is only a prefix of
/// the reference — are left untouched rather than corrupted.
fn inline_cid_images(html: &str, inline: &[InlineImage]) -> String {
    if inline.is_empty() {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find("cid:") {
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        let after_marker = &from["cid:".len()..];
        let (open, after_open) = match after_marker.strip_prefix('<') {
            Some(stripped) => (true, stripped),
            None => (false, after_marker),
        };
        let matched = inline.iter().find(|image| {
            after_open.starts_with(&image.cid) && !continues_cid(&after_open[image.cid.len()..])
        });
        match matched {
            Some(image) => {
                let closing = usize::from(open && after_open[image.cid.len()..].starts_with('>'));
                out.push_str("data:");
                out.push_str(&image.content_type);
                out.push_str(";base64,");
                out.push_str(&image.data_base64);
                rest = &from["cid:".len() + usize::from(open) + image.cid.len() + closing..];
            }
            None => {
                out.push_str("cid:");
                rest = after_marker;
            }
        }
    }
    out.push_str(rest);
    out
}

/// True when `rest` continues a Content-ID, i.e. the matched id was only a
/// prefix of the reference. Quotes and angle brackets are attribute
/// delimiters rather than id characters, so they end a reference.
fn continues_cid(rest: &str) -> bool {
    rest.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || "-_.@%+=!$&()*,;:~".contains(c))
}

/// Insert `<style>css</style>` where it cannot break the document: right
/// after `<head…>`, else after a leading doctype, else at the very front
/// (the parser moves a leading style element into the head).
fn inject_style(html: &str, css: &str) -> String {
    let style = format!("<style>{css}</style>");
    let at = head_end(html).or_else(|| doctype_end(html)).unwrap_or(0);
    let mut out = String::with_capacity(html.len() + style.len());
    out.push_str(&html[..at]);
    out.push_str(&style);
    out.push_str(&html[at..]);
    out
}

/// Offset just past the first `<head …>` start tag, if the document has one.
fn head_end(html: &str) -> Option<usize> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<head")?;
    let after = lower[start + "<head".len()..].chars().next()?;
    // <header> and friends are not <head>.
    if after != '>' && !after.is_whitespace() {
        return None;
    }
    lower[start..].find('>').map(|end| start + end + 1)
}

/// Offset just past a leading `<!DOCTYPE …>`, if there is one.
fn doctype_end(html: &str) -> Option<usize> {
    let trimmed = html.trim_start();
    let skipped = html.len() - trimmed.len();
    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("<!doctype") {
        return None;
    }
    trimmed.find('>').map(|end| skipped + end + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(cid: &str, data: &str) -> InlineImage {
        InlineImage {
            cid: cid.to_string(),
            content_type: "image/png".to_string(),
            data_base64: data.to_string(),
        }
    }

    #[test]
    fn cid_references_become_data_uris() {
        let out = inline_cid_images(
            r#"<img src="cid:logo@example.com"><img src='cid:other' alt="x">"#,
            &[image("logo@example.com", "AAA"), image("other", "BBB")],
        );
        assert_eq!(
            out,
            r#"<img src="data:image/png;base64,AAA"><img src='data:image/png;base64,BBB' alt="x">"#
        );
    }

    #[test]
    fn unknown_or_longer_cids_are_left_alone() {
        let known = [image("logo", "AAA")];
        // A known id that is only a prefix of the reference must not be
        // rewritten — that would corrupt the longer id.
        assert_eq!(inline_cid_images("cid:logo2", &known), "cid:logo2");
        assert_eq!(inline_cid_images("cid:missing", &known), "cid:missing");
        assert_eq!(inline_cid_images("no images here", &[]), "no images here");
    }

    #[test]
    fn angle_bracketed_cids_are_rewritten() {
        assert_eq!(
            inline_cid_images(r#"src="cid:<a@b>""#, &[image("a@b", "ZZ")]),
            r#"src="data:image/png;base64,ZZ""#
        );
    }

    #[test]
    fn style_lands_somewhere_safe() {
        assert_eq!(
            inject_style("<html><head><title>x</title></head></html>", "s"),
            "<html><head><style>s</style><title>x</title></head></html>"
        );
        assert_eq!(
            inject_style("<!DOCTYPE html><p>hi</p>", "s"),
            "<!DOCTYPE html><style>s</style><p>hi</p>"
        );
        assert_eq!(inject_style("<p>hi</p>", "s"), "<style>s</style><p>hi</p>");
        // <header> must not be mistaken for <head>.
        assert_eq!(
            inject_style("<header>x</header>", "s"),
            "<style>s</style><header>x</header>"
        );
    }

    /// End-to-end smoke test for the GTK thread: spawn, load, wait for the
    /// load-finished signal. Skips (rather than fails) without a display.
    #[test]
    fn window_loads_a_document() {
        LOAD_FINISHED.store(false, std::sync::atomic::Ordering::SeqCst);
        let Some(view) = WebView::spawn() else {
            eprintln!("jimao webview: no GTK display, skipping the smoke test");
            return;
        };
        view.show("jimao test", "<html><body><p>hello</p></body></html>");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !LOAD_FINISHED.load(std::sync::atomic::Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        view.hide();
        assert!(
            LOAD_FINISHED.load(std::sync::atomic::Ordering::SeqCst),
            "WebKit never reported a finished load"
        );
    }
}
