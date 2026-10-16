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
//! Deliberately the smallest part of the interface that does the job: choosing
//! files to open and choosing where to save one, with the answer read off the
//! request object's `Response` signal. No filters, no parent-window handle (the
//! dialog is then the desktop's own, unowned, which every portal accepts), no
//! directory picker.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::dbus::{self, Value};
use crate::error::{MailError, MailResult};

/// Where the portal lives on the session bus.
const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const FILE_CHOOSER: &str = "org.freedesktop.portal.FileChooser";
/// The signal the answer arrives on: the portal sends `Response` on the request
/// object the reply to the call named.
const RESPONSE: &str = "Response";

/// Ask the desktop to choose files, and answer with the ones chosen. What comes
/// back is empty when the dialog was cancelled — which is not an error, and not
/// something to say anything about — and an error when there is no portal to
/// ask: the card that called this draws the reason under its own area.
pub fn open_files() -> MailResult<Vec<PathBuf>> {
    let mut bus = chooser()?;
    ask(&mut bus, OPEN_FILE, &open_args(&next_token()))
}

/// Ask the desktop where to save a file, suggesting `name` as what to call it
/// and `folder` as the directory to open the dialog on. Both are suggestions —
/// the interface says the dialog is free to ignore them — and the answer, when
/// there is one, is the file. `Ok(None)` is the user closing the dialog:
/// nothing to write, and nothing to say about it. An error is a session with no
/// dialog to ask at all, which the export that called this has an answer for.
pub fn save_file(name: &str, folder: &Path) -> MailResult<Option<PathBuf>> {
    let mut bus = chooser()?;
    let mut files = ask(&mut bus, SAVE_FILE, &save_args(&next_token(), name, folder))?;
    // A save answers with exactly one URI — the interface says so — and the
    // first is that file. More than one would be a portal contradicting itself,
    // and the first is still the file it was asked about.
    Ok(files.pop())
}

/// The two calls this makes. `OpenFile` and `SaveFile` are the names the
/// interface has used since it had options at all — every release of
/// xdg-desktop-portal back to 0.10 (2018) declares these two and not the bare
/// `Open`/`Save` an earlier reading of the documentation might expect — so a
/// desktop that refuses one is a desktop with no dialog to offer rather than
/// one waiting to be asked under another name.
const OPEN_FILE: &str = "OpenFile";
const SAVE_FILE: &str = "SaveFile";

/// The token the answer will be tagged with. It becomes the last element of the
/// request object's path, so it holds only what an object path element may, and
/// it counts rather than naming the process alone: two dialogs can be up at
/// once — a file chooser for an attachment and one for an export, or two
/// exports — and two requests under one token would be two requests for one
/// object path.
fn next_token() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    let count = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("kimo{}_{count}", std::process::id())
}

/// A connection to the session bus ready to ask the portal a question: the
/// socket, this connection's greeting, and the match rule that brings the
/// answer to *this* connection.
fn chooser() -> MailResult<dbus::Bus> {
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
    Ok(bus)
}

/// Make one chooser call and wait for what becomes of it.
///
/// The reply and the answer can arrive in either order — a portal that refuses
/// at once sends the response before the reply to the call — so both are read
/// here, whichever comes first: the reply says which request the answer will be
/// tagged with, and the `Response` signal *is* the answer.
fn ask(bus: &mut dbus::Bus, method: &str, args: &[Value]) -> MailResult<Vec<PathBuf>> {
    let serial = bus
        .call_no_reply(PORTAL_BUS, PORTAL_PATH, FILE_CHOOSER, method, args)
        .map_err(|error| {
            MailError::Protocol(format!("the file dialog could not be asked for: {error}"))
        })?;
    loop {
        let message = bus
            .read_message()
            .map_err(|error| MailError::Protocol(format!("the file dialog went quiet: {error}")))?;
        if message.reply_serial == Some(serial) {
            if message.kind == dbus::MSG_ERROR {
                let name = message.error_name.clone().unwrap_or_default();
                return Err(MailError::Protocol(format!(
                    "the desktop refused the file dialog ({name}): {}",
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
        return read_response(&message.body);
    }
}

/// The arguments `FileChooser.OpenFile` takes: choosing files, several of them
/// at once, for the card that puts them on a mail.
fn open_args(token: &str) -> Vec<Value> {
    args("Attach files", token, vec![("multiple", Value::Bool(true))])
}

/// The arguments `FileChooser.SaveFile` takes: the same call with the two
/// things a save dialog can be told before it is drawn — what to call the file,
/// and which directory to open on. What comes back is the name the *user* left
/// in it, which is why the export writes there rather than numbering a name of
/// its own.
fn save_args(token: &str, name: &str, folder: &Path) -> Vec<Value> {
    args(
        "Export mail",
        token,
        vec![
            ("current_name", Value::Str(name.to_string())),
            ("current_folder", Value::bytes(&folder_bytes(folder))),
        ],
    )
}

/// The call both of the above are, as the portal declares it — `(ssa{sv})`: no
/// parent window, a title, and the options. The parent is empty because the
/// handle a Wayland client would give is an export of its surface, and an empty
/// string is what a portal takes to mean "no parent" rather than refusing the
/// call. The options start with the token the answer will be tagged with, which
/// every chooser call has, and go on with whatever this one wants to say.
fn args(title: &str, token: &str, options: Vec<(&str, Value)>) -> Vec<Value> {
    let mut entries = vec![entry("handle_token", Value::Str(token.to_string()))];
    for (key, value) in options {
        entries.push(entry(key, value));
    }
    vec![
        Value::Str(String::new()),
        Value::Str(title.to_string()),
        Value::Array {
            signature: "{sv}".to_string(),
            values: entries,
        },
    ]
}

/// A directory as the `ay` the portal's `current_folder` is: the file system's
/// own bytes — a path is bytes, and not necessarily text — with the nul byte at
/// the end of them that the interface asks for, since nothing on the other side
/// counts them.
fn folder_bytes(folder: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = folder.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    bytes
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
    /// this session — connect, greet the bus, ask for each of the two dialogs the
    /// app has, and close each request as soon as the reply names it. Each dialog
    /// appears for as long as that takes (nothing at all, where the portal
    /// answers at once) and the response that comes back is the cancellation it
    /// asked for.
    ///
    /// Ignored because it needs a desktop: there is no portal in a test run, and
    /// the one place this can be checked is a session that has one.
    ///
    ///     cargo test -- --ignored the_desktop_answers_a_chooser_call
    #[test]
    #[ignore = "developer tool: needs a session with a portal"]
    fn the_desktop_answers_a_chooser_call() {
        let mut bus = chooser().expect("a session bus with a portal on it");
        // Both calls the app makes, as the app makes them. The save is the one
        // worth watching: it is the call with options of its own (a suggested
        // name and a folder as a byte array), and the one a byte in the wrong
        // place or a key the portal does not know would be refused for.
        let calls = [
            (OPEN_FILE, open_args("kimotest_open")),
            (
                SAVE_FILE,
                save_args("kimotest_save", "Kimo portal test.eml", Path::new("/tmp")),
            ),
        ];
        for (method, args) in calls {
            let serial = bus
                .call_no_reply(PORTAL_BUS, PORTAL_PATH, FILE_CHOOSER, method, &args)
                .expect("the chooser call goes out");
            let mut handle = None;
            let mut response = None;
            while response.is_none() {
                let message = bus.read_message().expect("the portal answers");
                if message.reply_serial == Some(serial) {
                    if message.kind == dbus::MSG_ERROR {
                        panic!(
                            "{method} was refused: {} {}",
                            message.error_name.as_deref().unwrap_or("error"),
                            first_string(&message.body).unwrap_or_default()
                        );
                    }
                    handle = dbus::Unmarshal::new(&message.body).string();
                    // Closed the moment it exists: this is a check of the wire,
                    // not a request for a file. The destination is the portal's
                    // own bus name and the *path* is the request object — a path
                    // in the destination field is a malformed message, which the
                    // daemon answers by dropping the connection rather than by
                    // an error.
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
            assert!(handle.is_some(), "{method} named its request object");
            // What the response is matters less than that there is one: 1 is the
            // cancellation the Close asks for, and 0 is a choice — which this can
            // be if the dialog was answered before the Close landed. Either means
            // the portal read the call, made a request, and answered it.
            let response = response.expect("the portal answered").expect("with a code");
            eprintln!("{method} answered with {response}");
            assert!(
                response <= 1,
                "{method} answered something Kimo cannot read as files: {response}"
            );
        }
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

    /// The save call is the same shape with the options a save is about: what
    /// the file is suggested to be called, and the directory the dialog opens
    /// on. The name is the one the export would have given the file itself, so a
    /// user who presses Save without touching it gets what they always got. The
    /// folder is an `ay` — a byte array rather than a string, because a path is
    /// bytes and not necessarily text — ended by the nul byte the interface asks
    /// for, since nothing on the other side counts them.
    #[test]
    fn the_save_call_is_the_shape_the_portal_declares() {
        let args = save_args("kimo4242_7", "Invoice 42.eml", Path::new("/home/you/下载"));
        let (signature, _) = dbus::build_body(&args);
        assert_eq!(signature, "ssa{sv}");
        assert_eq!(args[0], Value::Str(String::new()), "no parent window");
        assert_eq!(args[1], Value::Str("Export mail".to_string()));
        assert_eq!(
            option(&args, "handle_token"),
            Value::Str("kimo4242_7".into())
        );
        assert_eq!(
            option(&args, "current_name"),
            Value::Str("Invoice 42.eml".into())
        );
        let mut folder = b"/home/you/".to_vec();
        folder.extend_from_slice("下载".as_bytes());
        folder.push(0);
        assert_eq!(option(&args, "current_folder"), Value::bytes(&folder));
    }

    /// One option out of a call's `a{sv}` dictionary, as the value the portal
    /// would read it as: what these tests are about is what is *in* the call, and
    /// not only which keys it names.
    fn option(args: &[Value], key: &str) -> Value {
        let Value::Array { values, .. } = &args[2] else {
            panic!("the options are a dictionary: {:?}", args[2]);
        };
        for entry in values {
            if let Value::Struct(fields) = entry {
                if fields.first() == Some(&Value::Str(key.to_string())) {
                    return match fields.get(1) {
                        Some(Value::Variant(value)) => (**value).clone(),
                        other => panic!("{key} is not a variant: {other:?}"),
                    };
                }
            }
        }
        panic!("no {key} option among {values:?}");
    }

    /// The token becomes the last element of the request object's path, so it may
    /// only hold what an object path element may — and two dialogs at once must
    /// not name one request between them.
    #[test]
    fn the_token_is_an_object_path_element_and_no_two_calls_share_one() {
        let first = next_token();
        let second = next_token();
        assert_ne!(first, second);
        for token in [&first, &second] {
            assert!(
                token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{token} is not an object path element"
            );
            assert!(token.starts_with(&format!("kimo{}", std::process::id())));
        }
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
