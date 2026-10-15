//! Desktop notifications: the freedesktop `org.freedesktop.Notifications`
//! interface, spoken over the same raw D-Bus wire layer as the tray
//! (`crate::dbus`) — no `zbus` (an async executor, hard rule 2) and no
//! libnotify (C, hard rule 1).
//!
//! Showing one is a blocking call: the daemon is asked and answers with the id
//! of the notification. It therefore never happens on the UI thread (hard rule
//! 3) — `show` only posts to one dedicated `std::thread` that owns the bus
//! connection and does the talking, whether it was called from the UI or from
//! a sync worker. A session whose notifications are switched off, or that
//! never gets new mail, never spawns the thread and never opens a connection.
//!
//! The icon is the app's own brand (`crate::tray::APP_PNG`, the 256 px
//! `assets/app.png` the tray serves and the title bar draws), scaled to
//! `ICON_SIZE` logical points at the screen's scale factor: 48 px on an
//! ordinary panel, 96 on a HiDPI one, where a daemon handed a fixed 48 px
//! bitmap would draw it small or upscale it into a blur. It travels as an
//! `image-path` hint to a PNG under the cache directory, not as `image-data`:
//! the inline form is the one hint daemons disagree about (the byte order of
//! its pixels), and every daemon reads a PNG itself.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::OnceLock;

use crate::dbus::{properties_dict, Bus, Value, MSG_ERROR};
use crate::storage;

/// What the daemon shows as the sender of the notification, and the name that
/// ties it to the app: the icon-theme name of the desktop file
/// (`packaging/linux/kimo.desktop`), which the `desktop-entry` hint resolves
/// against so a shell can file the notification under Kimo's own entry.
const APP_NAME: &str = "Kimo";
const APP_ID: &str = "kimo";

const NOTIFICATIONS_SERVICE: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const NOTIFICATIONS_INTERFACE: &str = "org.freedesktop.Notifications";

/// The icon's size in logical points — what a notification icon is drawn at.
/// The PNG written for the hint is this times the screen's scale factor (see
/// [`icon_pixels`]).
const ICON_SIZE: f32 = 48.0;
/// Where that scaling stops: the brand PNG is 256 px, and enlarging it past
/// its own size would only add blur.
const MAX_ICON_PIXELS: f32 = 256.0;

/// One queued notification.
struct Request {
    summary: String,
    body: String,
    /// The screen's scale factor when the notification was raised — the one
    /// thing about it only the window knows.
    scale: f32,
}

/// Why a notification did not go out.
enum Failed {
    /// The connection to the bus, or the bus itself: the next notification
    /// starts over with a new one.
    Bus(String),
    /// The daemon's own answer — most often that there is no daemon to answer
    /// at all. The connection is fine and is kept.
    Daemon(String),
}

/// The queue to the notify thread, made on the first notification.
static QUEUE: OnceLock<Sender<Request>> = OnceLock::new();

/// Show one desktop notification. Posting to the queue is all this does: the
/// blocking part is the notify thread's (see the module note).
pub(crate) fn show(summary: &str, body: &str, scale: f32) {
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("kimo-notify".to_string())
            .spawn(move || serve(rx));
        if let Err(e) = spawned {
            eprintln!("kimo notify: could not spawn thread: {e}");
        }
        tx
    });
    let _ = queue.send(Request {
        summary: summary.to_string(),
        body: body.to_string(),
        scale,
    });
}

/// The notify thread: one bus connection, one call per request.
fn serve(rx: Receiver<Request>) {
    let mut bus: Option<Bus> = None;
    // The icon file is written once per pixel size and never changes after
    // that, so the cache is only where the last one is.
    let mut icon: Option<(u32, PathBuf)> = None;
    // One line about a missing daemon, not one per mail.
    let mut reported = false;
    while let Ok(request) = rx.recv() {
        if bus.is_none() {
            match connect() {
                Ok(connected) => bus = Some(connected),
                Err(error) => {
                    if !reported {
                        eprintln!("kimo notify: {error}");
                        reported = true;
                    }
                    continue;
                }
            }
        }
        let path = icon_path(&mut icon, icon_pixels(request.scale));
        let outcome = match bus.as_mut() {
            Some(connected) => notify(connected, &request, path.as_deref()),
            None => continue,
        };
        if let Err(failure) = outcome {
            let (message, reconnect) = match failure {
                Failed::Bus(message) => (message, true),
                Failed::Daemon(message) => (message, false),
            };
            if !reported {
                eprintln!("kimo notify: {message}");
                reported = true;
            }
            if reconnect {
                bus = None;
            }
        }
    }
}

/// Connect and say hello: a bus wants `Hello` before anything else. The tray
/// has a connection of its own — it may not exist at all on a session with no
/// StatusNotifierWatcher — so notifications never ride on that one.
fn connect() -> Result<Bus, String> {
    let mut bus = Bus::connect()?;
    let hello = bus.call(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "Hello",
        &[],
    )?;
    if hello.kind == MSG_ERROR {
        return Err(format!("Hello failed: {:?}", hello.error_name));
    }
    Ok(bus)
}

/// The `Notify` call itself. The reply is the id the daemon assigned the
/// notification; nothing keeps it, because Kimo never replaces or closes one.
fn notify(bus: &mut Bus, request: &Request, icon: Option<&Path>) -> Result<(), Failed> {
    let reply = bus
        .call(
            NOTIFICATIONS_SERVICE,
            NOTIFICATIONS_PATH,
            NOTIFICATIONS_INTERFACE,
            "Notify",
            &notify_args(request, icon),
        )
        .map_err(Failed::Bus)?;
    if reply.kind == MSG_ERROR {
        return Err(Failed::Daemon(format!(
            "the notification daemon refused: {:?}",
            reply.error_name
        )));
    }
    Ok(())
}

/// The arguments of `Notify`: the app name and icon name, the two lines to
/// show, no actions, the icon file, and the daemon's own timeout for it.
fn notify_args(request: &Request, icon: Option<&Path>) -> Vec<Value> {
    let mut hints: Vec<(&str, Value)> = vec![("desktop-entry", Value::Str(APP_ID.to_string()))];
    if let Some(icon) = icon {
        hints.push((
            "image-path",
            Value::Str(icon.to_string_lossy().into_owned()),
        ));
    }
    vec![
        Value::Str(APP_NAME.to_string()),
        // replaces_id: every notification is new; Kimo replaces none.
        Value::UInt32(0),
        Value::Str(APP_ID.to_string()),
        Value::Str(request.summary.clone()),
        Value::Str(request.body.clone()),
        // actions: a notification here does nothing when it is clicked. The
        // default action would need the daemon's `ActionInvoked` signal, which
        // this client does not listen for (see the roadmap).
        Value::Array {
            signature: "s".to_string(),
            values: Vec::new(),
        },
        properties_dict(&hints),
        // expire_timeout: -1 is the daemon's own, which is what a mail
        // notification wants — long enough to read, gone when it is not.
        Value::Int32(-1),
    ]
}

/// The path of the PNG the `image-path` hint points at, written once per pixel
/// size: two Kimo processes, or a restart, reuse the file.
fn icon_path(cache: &mut Option<(u32, PathBuf)>, pixels: u32) -> Option<PathBuf> {
    if let Some((size, path)) = cache {
        if *size == pixels {
            return Some(path.clone());
        }
    }
    let written = write_icon(pixels);
    *cache = written.clone().map(|path| (pixels, path));
    written
}

/// Write the brand icon, `pixels` square, into the cache and answer its path.
/// `None` when it cannot be written: the notification then keeps the app's
/// icon name and no image, which is a smaller loss than no notification.
fn write_icon(pixels: u32) -> Option<PathBuf> {
    let path = storage::cache_dir()
        .join("notify")
        .join(format!("icon-{pixels}.png"));
    if path.exists() {
        return Some(path);
    }
    let png = encode_png(&icon_rgba(pixels)?)?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, png).ok()?;
    Some(path)
}

/// The icon's pixel size for one screen scale factor: 48 logical points times
/// the scale, rounded — 48 at 1.0, 96 at 2.0, 72 at 1.5 — and never past the
/// brand PNG's own 256 px. A factor that is not a number (a window that has
/// not seen a monitor yet, say) means 1.
fn icon_pixels(scale: f32) -> u32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    (ICON_SIZE * scale)
        .round()
        .clamp(ICON_SIZE, MAX_ICON_PIXELS) as u32
}

/// The brand icon as a `pixels`×`pixels` RGBA image: `assets/app.png` resized
/// with Lanczos, which is what keeps the circle's edge clean at 48 points and
/// at twice that.
fn icon_rgba(pixels: u32) -> Option<image::RgbaImage> {
    let decoded = image::load_from_memory(crate::tray::APP_PNG).ok()?;
    Some(image::imageops::resize(
        &decoded.to_rgba8(),
        pixels,
        pixels,
        image::imageops::FilterType::Lanczos3,
    ))
}

/// Encode an image as the PNG the hint's file holds. In memory, so the tests
/// exercise exactly what is written.
fn encode_png(image: &image::RgbaImage) -> Option<Vec<u8>> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::build_body;

    /// The hint names of an `a{sv}` built by [`properties_dict`], in order.
    fn hint_names(value: &Value) -> Vec<String> {
        let Value::Array { values, .. } = value else {
            panic!("hints must be a dict");
        };
        values
            .iter()
            .filter_map(|entry| match entry {
                Value::Struct(fields) => match fields.first() {
                    Some(Value::Str(name)) => Some(name.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_icon_is_48_points_at_the_screens_scale() {
        assert_eq!(icon_pixels(1.0), 48);
        assert_eq!(icon_pixels(2.0), 96);
        assert_eq!(icon_pixels(1.5), 72);
        assert_eq!(icon_pixels(1.25), 60);
        // A factor that came from nowhere is the ordinary one.
        assert_eq!(icon_pixels(0.0), 48);
        assert_eq!(icon_pixels(-2.0), 48);
        assert_eq!(icon_pixels(f32::NAN), 48);
        // Scaling never enlarges the brand PNG past its own size.
        assert_eq!(icon_pixels(16.0), 256);
    }

    #[test]
    fn the_brand_icon_is_resized_with_its_colors_and_transparency() {
        let image = icon_rgba(96).expect("the brand PNG decodes");
        assert_eq!((image.width(), image.height()), (96, 96));
        assert!(
            image.pixels().any(|px| px.0 == [131, 91, 225, 255]),
            "the purple circle must survive the resize"
        );
        assert_eq!(
            image.get_pixel(0, 0).0[3],
            0,
            "the corners are outside the circle"
        );
    }

    #[test]
    fn the_icon_written_for_the_hint_is_a_png_of_the_asked_size() {
        let png = encode_png(&icon_rgba(48).expect("the brand PNG decodes")).expect("encodes");
        let back = image::load_from_memory(&png).expect("decodes");
        assert_eq!((back.width(), back.height()), (48, 48));
    }

    /// The wire signature is the one the spec fixes, and the icon is the file
    /// the daemon reads — never inline image data.
    #[test]
    fn the_notify_call_is_the_one_the_spec_names() {
        let request = Request {
            summary: "Ada <ada@example.com>".to_string(),
            body: "Lunch?".to_string(),
            scale: 1.0,
        };
        let args = notify_args(&request, Some(Path::new("/cache/notify/icon-48.png")));
        // app_name, replaces_id, app_icon, summary, body, actions, hints,
        // expire_timeout.
        assert_eq!(build_body(&args).0, "susssasa{sv}i");
        assert_eq!(hint_names(&args[6]), vec!["desktop-entry", "image-path"]);
        assert_eq!(
            hint_names(&notify_args(&request, None)[6]),
            vec!["desktop-entry"],
            "no icon file means no image hint, not a broken one"
        );
        assert_eq!(args[3], Value::Str("Ada <ada@example.com>".to_string()));
        assert_eq!(args[7], Value::Int32(-1), "the daemon's own timeout");
    }
    /// The wire form, on a real bus, by hand: the marshalling here is the one
    /// part a unit test cannot see, and this is what shows it.
    ///
    /// ```sh
    /// dbus-run-session -- bash -c 'dbus-monitor \
    ///   "interface=org.freedesktop.Notifications" & \
    ///   cargo test notify_probe -- --ignored; sleep 1; kill %1'
    /// ```
    ///
    /// Run as a normal test it would notify the developer's own desktop, so it
    /// is ignored — and the three calls below are one of each shape: a mail at
    /// 2×, a mail at 1×, and a batch summary with several lines.
    #[test]
    #[ignore = "talks to the session bus"]
    fn notify_probe() {
        show("Ada <ada@example.com>", "Lunch?", 2.0);
        show("Bob <bob@example.com>", "Re: report", 1.0);
        show(
            "7 new messages in INBOX",
            "Ada: one\nBob: two\nCid: three",
            1.0,
        );
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}
