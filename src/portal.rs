//! The desktop's own file dialog, asked for over D-Bus.
//!
//! Kimo draws its window with winit and egui and links no toolkit, so there is
//! no `GtkFileChooser` to call — and a file dialog is not something a program
//! can draw for itself. The XDG desktop portals are what the Linux desktops
//! agreed on instead: the dialog belongs to the *compositor*, and a program
//! asks for it with `org.freedesktop.portal.FileChooser.Open`. That keeps this
//! a plain D-Bus conversation (`crate::dbus`) with nothing linked in, and it is
//! the one way to choose a file that works on Wayland — where winit 0.30 has no
//! drag-and-drop at all, so a drop never reaches a Wayland window.
//!
//! Deliberately the smallest part of the interface that does the job: `Open`,
//! with the answer read off the request object's `Response` signal. No filters,
//! no parent-window handle (the dialog is then the desktop's own, unowned, which
//! every portal accepts), no `Save` and no directory picker.

use std::path::PathBuf;

use crate::dbus::{self, Value};
use crate::error::{MailError, MailResult};

/// Where the portal lives on the session bus.
const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const FILE_CHOOSER: &str = "org.freedesktop.portal.FileChooser";
/// What the bus answers when the method is not one the interface has: the one
/// error that means "try the other name" rather than "no".
const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
/// The signal the answer arrives on: the portal sends `Response` on the request
/// object the reply to the call named.
const RESPONSE: &str = "Response";

/// Ask the desktop to choose files, and answer with the ones chosen. What comes
/// back is empty when the dialog was cancelled — which is not an error, and not
/// something to say anything about — and an error when there is no portal to
/// ask: the card that called this says so, and has a box that takes a path for
/// exactly that session.
pub fn open_files() -> MailResult<Vec<PathBuf>> {
    let mut bus = dbus::Bus::connect()
        .map_err(|error| MailError::Protocol(format!("no session bus: {error}")))?;
    // A bus wants `Hello` before anything else and closes a connection that
    // sends something else first: it is the one message every D-Bus client owes
    // the daemon before it may call anybody. `Bus::connect` is the socket and
    // the authentication, not the registration, so `crate::notify` and
    // `crate::tray` say it on their own connections too — and so does this one.
    let hello = bus
        .call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "Hello",
            &[],
        )
        .map_err(|error| {
            MailError::Protocol(format!("the session bus refused a greeting: {error}"))
        })?;
    if hello.kind == dbus::MSG_ERROR {
        return Err(MailError::Protocol(format!(
            "the session bus refused a greeting: {}",
            hello.error_name.as_deref().unwrap_or("error")
        )));
    }
    // The answer is a signal *directed* at this connection, which the bus
    // delivers on its own; the match rule is one call and takes the doubt out
    // of that reading of the specification. It is scoped by the member alone
    // rather than by `{REQUEST}` as well: a portal that answered under an
    // interface this code does not know would otherwise be waited for forever,
    // and the signal is only ours either way.
    let rule = format!("type='signal',member='{RESPONSE}'");
    let _ = bus.call(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "AddMatch",
        &[Value::Str(rule)],
    );
    // The options the call takes. The token names the request object the answer
    // will come on — the reply says which one it is — and `multiple` is the
    // whole of what this asks for beyond the dialog itself.
    let token = format!("kimo{}", std::process::id());
    match ask(&mut bus, OPEN_FILE, &token) {
        // The method was renamed when the interface grew a second kind of
        // choice (`OpenFile`, `SaveFile`), and a portal that speaks the older
        // interface says so this way rather than by a version this code would
        // have to ask for: the older name is the same call.
        Answered::UnknownMethod => match ask(&mut bus, OPEN, &token) {
            Answered::Files(files) => Ok(files),
            Answered::UnknownMethod => Err(MailError::Protocol(
                "this desktop's file dialog answers to neither of the names Kimo knows".to_string(),
            )),
            Answered::Failed(error) => Err(error),
        },
        Answered::Files(files) => Ok(files),
        Answered::Failed(error) => Err(error),
    }
}

/// The two names the portal's file chooser has had. The interface is at
/// `OpenFile` now (`version` 4 on the desktops that have it) and was `Open`
/// before; both take and answer the same way.
const OPEN_FILE: &str = "OpenFile";
const OPEN: &str = "Open";

/// What one call to the portal came to. `UnknownMethod` is kept apart from
/// every other failure because it is the one a caller can do something about:
/// it is how a portal that predates the method's rename answers, and the reply
/// to that is the method's older name.
enum Answered {
    Files(Vec<PathBuf>),
    UnknownMethod,
    Failed(MailError),
}

/// Make one chooser call and wait for what becomes of it.
///
/// The reply and the answer can arrive in either order — a portal that refuses
/// at once sends the response before the reply to the call — so both are read
/// here, whichever comes first: the reply says which request the answer will be
/// tagged with, and the `Response` signal *is* the answer.
fn ask(bus: &mut dbus::Bus, method: &str, token: &str) -> Answered {
    let serial = match bus.call_no_reply(
        PORTAL_BUS,
        PORTAL_PATH,
        FILE_CHOOSER,
        method,
        &open_args(token),
    ) {
        Ok(serial) => serial,
        Err(error) => {
            return Answered::Failed(MailError::Protocol(format!(
                "the file dialog could not be asked for: {error}"
            )))
        }
    };
    loop {
        let message = match bus.read_message() {
            Ok(message) => message,
            Err(error) => {
                return Answered::Failed(MailError::Protocol(format!(
                    "the file dialog went quiet: {error}"
                )))
            }
        };
        if message.reply_serial == Some(serial) {
            if message.kind == dbus::MSG_ERROR {
                let name = message.error_name.clone().unwrap_or_default();
                if name == UNKNOWN_METHOD {
                    return Answered::UnknownMethod;
                }
                return Answered::Failed(MailError::Protocol(format!(
                    "this desktop has no file dialog to open ({name}): {}",
                    first_string(&message.body).unwrap_or_default()
                )));
            }
            // The request object: the answer comes on it, as a signal.
            continue;
        }
        if message.member.as_deref() != Some(RESPONSE) {
            // Something else on the bus: a name change, another program's
            // signal, this connection's own greetings.
            continue;
        }
        return match read_response(&message.body) {
            Ok(files) => Answered::Files(files),
            Err(error) => Answered::Failed(error),
        };
    }
}

/// The arguments `FileChooser.Open` takes, as the portal declares them —
/// `(ssa{sv})`: no parent window, a title, and the options. The parent is empty
/// because the handle a Wayland client would give is an export of its surface,
/// and an empty string is what a portal takes to mean "no parent" rather than
/// refusing the call. The options are the token the answer will be tagged with
/// and whether several files may be chosen at once.
fn open_args(token: &str) -> Vec<Value> {
    vec![
        Value::Str(String::new()),
        Value::Str("Attach files".to_string()),
        Value::Array {
            signature: "{sv}".to_string(),
            values: vec![
                entry("handle_token", Value::Str(token.to_string())),
                entry("multiple", Value::Bool(true)),
            ],
        },
    ]
}

/// One `a{sv}` entry: the key, and its value as a variant.
fn entry(key: &str, value: Value) -> Value {
    Value::Struct(vec![
        Value::Str(key.to_string()),
        Value::Variant(Box::new(value)),
    ])
}

/// The `(u response, a{sv} results)` of a `Response` signal as the files it
/// names. `0` is "chosen", `1` is the user closing the dialog, and `2` is
/// anything else the portal did to it — all of which end the same way here: no
/// files, and nothing to say about it.
fn read_response(body: &[u8]) -> MailResult<Vec<PathBuf>> {
    let mut reader = dbus::Unmarshal::new(body);
    let response = reader.uint32().unwrap_or(1);
    if response != 0 {
        return Ok(Vec::new());
    }
    let mut uris: Vec<String> = Vec::new();
    // A D-Bus array says how many *bytes* it holds rather than how many
    // entries, so the walk is by the reader's own position: a dictionary may
    // hold values of any size, and only the reader knows how far each one went.
    let len = reader.uint32().ok_or_else(malformed)? as usize;
    let start = reader.pos;
    while reader.pos - start < len {
        reader.align(8);
        let Some(key) = reader.string() else {
            return Err(malformed());
        };
        let Some(signature) = reader.signature() else {
            return Err(malformed());
        };
        if key == "uris" && signature == "as" {
            let bytes = reader.uint32().ok_or_else(malformed)? as usize;
            let strings = reader.pos;
            while reader.pos - strings < bytes {
                let Some(uri) = reader.string() else {
                    return Err(malformed());
                };
                uris.push(uri);
            }
        } else if reader.skip_value(&signature).is_none() {
            return Err(malformed());
        }
    }
    Ok(uris.iter().filter_map(|uri| file_uri(uri)).collect())
}

fn malformed() -> MailError {
    MailError::Protocol("the file dialog's answer is malformed".to_string())
}

/// A `file://` URI as the path it names. The portal answers in URIs, which is
/// what a desktop hands around; a path is what the rest of Kimo speaks.
fn file_uri(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?;
    // `file:///home/you/a b.txt`: everything after the empty authority, with the
    // percent escapes a URI uses for whatever a path may hold.
    let path = path.strip_prefix("localhost").unwrap_or(path);
    Some(PathBuf::from(percent_decoded(path)))
}

/// Undo the percent-encoding of a URI path: `%20` is a space, `%2F` a slash,
/// and anything that is not an escape is itself. Bytes rather than characters —
/// a URI escapes UTF-8 — so what comes back is built from the bytes and read as
/// UTF-8, losing nothing a file name may hold.
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The first string in a body, for an error message that says what the bus
/// said rather than only that it failed.
fn first_string(body: &[u8]) -> Option<String> {
    dbus::Unmarshal::new(body).string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Response` body as the portal sends it: the response code, then the
    /// results dictionary. Built with the same marshaller the app talks to the
    /// bus with, so what is parsed here is what the wire holds.
    fn response(code: u32, uris: &[&str]) -> Vec<u8> {
        let list = Value::Array {
            signature: "s".to_string(),
            values: uris.iter().map(|uri| Value::Str(uri.to_string())).collect(),
        };
        let results = Value::Array {
            signature: "{sv}".to_string(),
            values: vec![entry("uris", list)],
        };
        let (_, body) = dbus::build_body(&[Value::UInt32(code), results]);
        body
    }

    /// Developer tool: the whole path a click takes, against the real portal on
    /// this session — connect, greet the bus, ask for the dialog, and close the
    /// request as soon as the reply names it. The dialog appears for as long as
    /// that takes (nothing at all, where the portal answers at once) and the
    /// response that comes back is the cancellation it asked for.
    ///
    /// Ignored because it needs a desktop: there is no portal in a test run, and
    /// the one place this can be checked is a session that has one.
    ///
    ///     cargo test -- --ignored the_desktop_answers_a_chooser_call
    #[test]
    #[ignore = "developer tool: needs a session with a portal"]
    fn the_desktop_answers_a_chooser_call() {
        let mut bus = dbus::Bus::connect().expect("a session bus");
        let hello = bus
            .call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "Hello",
                &[],
            )
            .expect("Hello");
        assert_eq!(hello.kind, dbus::MSG_METHOD_RETURN, "the bus greets us");
        let rule = format!("type='signal',member='{RESPONSE}'");
        bus.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "AddMatch",
            &[Value::Str(rule)],
        )
        .expect("AddMatch");

        let token = "kimotest".to_string();
        let serial = bus
            .call_no_reply(
                PORTAL_BUS,
                PORTAL_PATH,
                FILE_CHOOSER,
                OPEN_FILE,
                &open_args(&token),
            )
            .expect("the chooser call goes out");
        let mut handle = None;
        let mut response = None;
        while response.is_none() {
            let message = bus.read_message().expect("the portal answers");
            if message.reply_serial == Some(serial) {
                if message.kind == dbus::MSG_ERROR {
                    panic!(
                        "the portal refused the call: {} {}",
                        message.error_name.as_deref().unwrap_or("error"),
                        first_string(&message.body).unwrap_or_default()
                    );
                }
                handle = dbus::Unmarshal::new(&message.body).string();
                // Closed the moment it exists: this is a check of the wire, not
                // a request for a file. The destination is the portal's own bus
                // name and the *path* is the request object — a path in the
                // destination field is a malformed message, which the daemon
                // answers by dropping the connection rather than by an error.
                let request = handle.clone().expect("the reply names the request");
                let close = bus.call_no_reply(
                    PORTAL_BUS,
                    &request,
                    "org.freedesktop.portal.Request",
                    "Close",
                    &[],
                );
                assert!(close.is_ok(), "the request can be closed: {close:?}");
                continue;
            }
            if message.member.as_deref() == Some(RESPONSE) {
                response = Some(dbus::Unmarshal::new(&message.body).uint32());
            }
        }
        assert!(handle.is_some(), "the call named its request object");
        // What the response is matters less than that there is one: 1 is the
        // cancellation the Close asks for, and 0 is a choice — which this can be
        // if the dialog was answered before the Close landed. Either means the
        // portal read the call, made a request, and answered it.
        let response = response.expect("the portal answered").expect("with a code");
        eprintln!("the dialog answered with {response}");
        assert!(
            response <= 1,
            "the portal answered something Kimo cannot read as files: {response}"
        );
    }

    /// The call itself is the shape the portal declares — `(ssa{sv})` — with the
    /// options the interface names. It is pinned here because it is the one part
    /// of this that no test can watch a desktop answer: a wrong signature is a
    /// call the daemon refuses, and the only other place it would show is the
    /// user's screen.
    #[test]
    fn the_open_call_is_the_shape_the_portal_declares() {
        let args = open_args("kimo4242");
        let (signature, _) = dbus::build_body(&args);
        assert_eq!(signature, "ssa{sv}");
        assert_eq!(args[0], Value::Str(String::new()), "no parent window");
        assert_eq!(args[1], Value::Str("Attach files".to_string()));
        let Value::Array { signature, values } = &args[2] else {
            panic!("the options are a dictionary: {:?}", args[2]);
        };
        assert_eq!(signature, "{sv}");
        let keys: Vec<&str> = values
            .iter()
            .filter_map(|entry| match entry {
                Value::Struct(fields) => match fields.first() {
                    Some(Value::Str(key)) => Some(key.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(keys, ["handle_token", "multiple"]);
    }

    /// The answer is the files it names: the response code decides whether
    /// there are any — `1` is the user closing the dialog, and `2` is anything
    /// else the portal did to it, neither of which is a failure — and the URIs
    /// are turned into the paths the rest of Kimo speaks.
    #[test]
    fn the_answer_is_the_files_it_names() {
        let chosen = read_response(&response(
            0,
            &[
                "file:///home/you/a%20report.pdf",
                "file://localhost/home/you/%E5%8F%91%E7%A5%A8.txt",
            ],
        ))
        .expect("the answer parses");
        assert_eq!(
            chosen,
            vec![
                PathBuf::from("/home/you/a report.pdf"),
                PathBuf::from("/home/you/发票.txt"),
            ]
        );

        // Cancelled, and ended some other way: no files, and nothing to say.
        assert!(read_response(&response(1, &[])).unwrap().is_empty());
        assert!(read_response(&response(2, &[])).unwrap().is_empty());
        // A dictionary with nothing this cares about is empty rather than
        // malformed: the portal is free to answer with more than `uris`.
        let (_, body) = dbus::build_body(&[
            Value::UInt32(0),
            Value::Array {
                signature: "{sv}".to_string(),
                values: vec![entry(
                    "choices",
                    Value::Array {
                        signature: "s".to_string(),
                        values: Vec::new(),
                    },
                )],
            },
        ]);
        assert!(read_response(&body).unwrap().is_empty());
    }

    /// The URIs the portal answers with are `file://` ones, and a path is what
    /// comes back out: the authority is empty (or `localhost`), and whatever the
    /// path holds is percent-encoded.
    #[test]
    fn a_uri_becomes_the_path_it_names() {
        assert_eq!(
            file_uri("file:///tmp/a.txt"),
            Some(PathBuf::from("/tmp/a.txt"))
        );
        assert_eq!(
            file_uri("file://localhost/tmp/a.txt"),
            Some(PathBuf::from("/tmp/a.txt"))
        );
        assert_eq!(
            file_uri("file:///tmp/100%25%20done"),
            Some(PathBuf::from("/tmp/100% done"))
        );
        assert_eq!(
            file_uri("file:///tmp/a+b.txt"),
            Some(PathBuf::from("/tmp/a+b.txt"))
        );
        assert_eq!(file_uri("https://example.com/a.txt"), None);
        assert_eq!(file_uri("/tmp/a.txt"), None);
        // A stray escape is left as it is rather than eating the rest of the
        // name.
        assert_eq!(
            file_uri("file:///tmp/100%"),
            Some(PathBuf::from("/tmp/100%"))
        );
    }
}
