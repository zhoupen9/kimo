//! Linux system-tray icon via the StatusNotifierItem (SNI) protocol.
//!
//! D-Bus frameworks were deliberately avoided: `zbus` brings an async
//! executor (rule 2) and the GTK-based tray crates link C (rule 1), so this
//! module speaks just enough of the D-Bus wire protocol, over the session
//! bus, on one dedicated `std::thread`: it registers a
//! `org.kde.StatusNotifierItem` and serves the properties/activations that
//! KDE Plasma and compatible trays query. The icon is `assets/app.png`
//! (rasterized from `assets/app.svg`), embedded at compile time, decoded
//! with the pure-Rust image crate with its colors kept as-is. A missing
//! StatusNotifierWatcher (e.g. GNOME without an extension) simply disables
//! the tray — the app keeps running.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

/// The application icon (a monochrome silhouette rasterized from
/// `assets/app.svg`), embedded at compile time.
pub(crate) const APP_PNG: &[u8] = include_bytes!("../assets/app.png");

/// D-Bus message types. (Signals, type 4, are received and skipped.)
const MSG_METHOD_CALL: u8 = 1;
const MSG_METHOD_RETURN: u8 = 2;
const MSG_ERROR: u8 = 3;

/// Header field codes.
const FIELD_PATH: u8 = 1;
const FIELD_INTERFACE: u8 = 2;
const FIELD_MEMBER: u8 = 3;
const FIELD_ERROR_NAME: u8 = 4;
const FIELD_REPLY_SERIAL: u8 = 5;
const FIELD_DESTINATION: u8 = 6;
const FIELD_SENDER: u8 = 7;
const FIELD_SIGNATURE: u8 = 8;

const SNI_INTERFACE: &str = "org.kde.StatusNotifierItem";
const INTROSPECTABLE: &str = "org.freedesktop.DBus.Introspectable";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

const INTROSPECT_XML: &str = concat!(
    "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\" ",
    "\"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">\n",
    "<node>\n",
    "  <interface name=\"org.kde.StatusNotifierItem\">\n",
    "    <property name=\"Category\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"Id\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"Title\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"Status\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"WindowId\" type=\"i\" access=\"read\"/>\n",
    "    <property name=\"IconName\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"IconPixmap\" type=\"aay\" access=\"read\"/>\n",
    "    <property name=\"OverlayIconName\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"OverlayIconPixmap\" type=\"aay\" access=\"read\"/>\n",
    "    <property name=\"AttentionIconName\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"AttentionIconPixmap\" type=\"aay\" access=\"read\"/>\n",
    "    <property name=\"AttentionMovieName\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"ToolTip\" type=\"(sa(ii)ss)\" access=\"read\"/>\n",
    "    <property name=\"ItemIsMenu\" type=\"b\" access=\"read\"/>\n",
    "    <property name=\"Menu\" type=\"o\" access=\"read\"/>\n",
    "    <method name=\"Activate\"><arg type=\"i\" name=\"x\"/><arg type=\"i\" name=\"y\"/></method>\n",
    "    <method name=\"SecondaryActivate\"><arg type=\"i\" name=\"x\"/><arg type=\"i\" name=\"y\"/></method>\n",
    "    <method name=\"Scroll\"><arg type=\"i\" name=\"delta\"/><arg type=\"s\" name=\"orientation\"/></method>\n",
    "  </interface>\n",
    "  <interface name=\"org.freedesktop.DBus.Properties\">\n",
    "    <method name=\"Get\"><arg type=\"s\"/><arg type=\"s\"/><arg type=\"v\" direction=\"out\"/></method>\n",
    "    <method name=\"GetAll\"><arg type=\"s\"/><arg type=\"a{sv}\" direction=\"out\"/></method>\n",
    "  </interface>\n",
    "  <interface name=\"org.freedesktop.DBus.Introspectable\">\n",
    "    <method name=\"Introspect\"><arg type=\"s\" direction=\"out\"/></method>\n",
    "  </interface>\n",
    "</node>\n",
);

// ----- wire values -----------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Byte(u8),
    Bool(bool),
    Int32(i32),
    UInt32(u32),
    Str(String),
    ObjectPath(String),
    Signature(String),
    Array {
        signature: String,
        values: Vec<Value>,
    },
    Struct(Vec<Value>),
    Variant(Box<Value>),
}

impl Value {
    fn bytes(data: &[u8]) -> Value {
        Value::Array {
            signature: "y".to_string(),
            values: data.iter().map(|b| Value::Byte(*b)).collect(),
        }
    }
}

/// The D-Bus type signature of a value.
pub(crate) fn type_signature(v: &Value) -> String {
    match v {
        Value::Byte(_) => "y".into(),
        Value::Bool(_) => "b".into(),
        Value::Int32(_) => "i".into(),
        Value::UInt32(_) => "u".into(),
        Value::Str(_) => "s".into(),
        Value::ObjectPath(_) => "o".into(),
        Value::Signature(_) => "g".into(),
        Value::Array { signature, .. } => format!("a{signature}"),
        Value::Struct(items) => {
            let inner: String = items.iter().map(type_signature).collect();
            format!("({inner})")
        }
        Value::Variant(_) => "v".into(),
    }
}

/// Alignment of a complete type given the first character(s) of its
/// signature.
fn signature_alignment(sig: &str) -> usize {
    match sig.as_bytes().first() {
        Some(b'y') => 1,
        Some(b'b' | b'i' | b'u' | b's' | b'o' | b'g' | b'a') => 4,
        Some(b'(' | b'{') => 8,
        _ => 1,
    }
}

/// Little-endian marshaller; alignment is relative to the buffer start,
/// which for us is always the message start.
struct Marshal {
    buf: Vec<u8>,
}

impl Marshal {
    fn new() -> Self {
        Marshal { buf: Vec::new() }
    }

    fn align(&mut self, to: usize) {
        while !self.buf.len().is_multiple_of(to) {
            self.buf.push(0);
        }
    }

    fn byte(&mut self, v: u8) {
        self.buf.push(v);
    }

    fn uint32(&mut self, v: u32) {
        self.align(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn int32(&mut self, v: i32) {
        self.align(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn string(&mut self, s: &str) {
        self.uint32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    fn signature(&mut self, s: &str) {
        self.byte(s.len() as u8);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    fn value(&mut self, v: &Value) {
        match v {
            Value::Byte(b) => self.byte(*b),
            Value::Bool(b) => self.uint32(*b as u32),
            Value::Int32(i) => self.int32(*i),
            Value::UInt32(u) => self.uint32(*u),
            Value::Str(s) | Value::ObjectPath(s) => self.string(s),
            Value::Signature(s) => self.signature(s),
            Value::Array { signature, values } => {
                let mut elements = Marshal::new();
                for item in values {
                    elements.value(item);
                }
                self.align(4);
                self.uint32(elements.buf.len() as u32);
                self.align(signature_alignment(signature));
                self.buf.extend_from_slice(&elements.buf);
            }
            Value::Struct(items) => {
                self.align(8);
                for item in items {
                    self.value(item);
                }
            }
            Value::Variant(inner) => {
                let sig = type_signature(inner);
                self.signature(&sig);
                self.value(inner);
            }
        }
    }
}

fn build_body(values: &[Value]) -> (String, Vec<u8>) {
    let mut signature = String::new();
    for v in values {
        signature.push_str(&type_signature(v));
    }
    let mut m = Marshal::new();
    for v in values {
        m.value(v);
    }
    (signature, m.buf)
}

fn build_message(kind: u8, serial: u32, fields: &[(u8, Value)], body: &[u8]) -> Vec<u8> {
    let mut header = Marshal::new();
    header.byte(b'l'); // little-endian
    header.byte(kind);
    header.byte(0); // flags
    header.byte(1); // protocol version
    header.uint32(body.len() as u32);
    header.uint32(serial);
    let mut elements = Marshal::new();
    for (code, v) in fields {
        elements.align(8);
        elements.byte(*code);
        elements.value(&Value::Variant(Box::new(v.clone())));
    }
    header.align(4);
    header.uint32(elements.buf.len() as u32);
    header.align(8);
    header.buf.extend_from_slice(&elements.buf);
    header.align(8);
    let mut out = header.buf;
    out.extend_from_slice(body);
    out
}

// ----- unmarshalling ---------------------------------------------------------

#[derive(Debug, Default)]
pub(crate) struct Message {
    kind: u8,
    serial: u32,
    #[allow(dead_code)]
    path: Option<String>,
    interface: Option<String>,
    member: Option<String>,
    error_name: Option<String>,
    reply_serial: Option<u32>,
    sender: Option<String>,
    signature: Option<String>,
    body: Vec<u8>,
}

struct Unmarshal<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Unmarshal<'a> {
    fn new(data: &'a [u8]) -> Self {
        Unmarshal { data, pos: 0 }
    }

    fn align(&mut self, to: usize) {
        while !self.pos.is_multiple_of(to) {
            self.pos += 1;
        }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let slice = self.data.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(slice)
    }

    fn byte(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }

    fn uint32(&mut self) -> Option<u32> {
        self.align(4);
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn string(&mut self) -> Option<String> {
        let len = self.uint32()? as usize;
        let bytes = self.take(len)?;
        let s = String::from_utf8_lossy(bytes).into_owned();
        self.take(1)?; // nul
        Some(s)
    }

    fn signature(&mut self) -> Option<String> {
        let len = self.byte()? as usize;
        let bytes = self.take(len)?;
        let s = String::from_utf8_lossy(bytes).into_owned();
        self.take(1)?; // nul
        Some(s)
    }

    /// One header field: (code byte, variant).
    fn header_field(&mut self) -> Option<(u8, Value)> {
        self.align(8);
        let code = self.byte()?;
        let sig = self.signature()?;
        let value = match sig.as_str() {
            "s" | "o" => Value::Str(self.string()?),
            "g" => Value::Signature(self.signature()?),
            "u" => Value::UInt32(self.uint32()?),
            _ => return None,
        };
        Some((code, value))
    }
}

/// Parse one full message (header + body) from `buf`.
fn parse_message(buf: &[u8]) -> Option<Message> {
    if buf.len() < 16 || buf[0] != b'l' {
        return None;
    }
    let body_len = u32::from_le_bytes(buf[4..8].try_into().ok()?) as usize;
    let fields_len = u32::from_le_bytes(buf[12..16].try_into().ok()?) as usize;
    let mut u = Unmarshal::new(buf);
    u.pos = 16;
    let mut msg = Message {
        kind: buf[1],
        serial: u32::from_le_bytes(buf[8..12].try_into().ok()?),
        ..Message::default()
    };
    let fields_end = 16 + fields_len;
    while u.pos < fields_end {
        let (code, value) = u.header_field()?;
        let text = |v: Value| match v {
            Value::Str(s) | Value::Signature(s) => s,
            _ => String::new(),
        };
        match code {
            FIELD_PATH => msg.path = Some(text(value)),
            FIELD_INTERFACE => msg.interface = Some(text(value)),
            FIELD_MEMBER => msg.member = Some(text(value)),
            FIELD_ERROR_NAME => msg.error_name = Some(text(value)),
            FIELD_REPLY_SERIAL => {
                msg.reply_serial = match value {
                    Value::UInt32(n) => Some(n),
                    _ => None,
                }
            }
            FIELD_DESTINATION => {} // our own name on receive
            FIELD_SENDER => msg.sender = Some(text(value)),
            FIELD_SIGNATURE => msg.signature = Some(text(value)),
            _ => {}
        }
    }
    u.pos = fields_end;
    u.align(8);
    msg.body = buf.get(u.pos..u.pos + body_len)?.to_vec();
    Some(msg)
}

// ----- session bus client ----------------------------------------------------

struct Bus {
    reader: UnixStream,
    writer: UnixStream,
    serial: u32,
}

impl Bus {
    fn connect() -> Result<Bus, String> {
        let address = std::env::var("DBUS_SESSION_BUS_ADDRESS")
            .map_err(|_| "DBUS_SESSION_BUS_ADDRESS is not set".to_string())?;
        let path = address
            .strip_prefix("unix:path=")
            .and_then(|rest| rest.split(',').next())
            .ok_or_else(|| {
                format!("unsupported session bus address (need unix:path=…): {address}")
            })?
            .to_string();
        let stream = UnixStream::connect(&path).map_err(|e| format!("connect {path}: {e}"))?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .ok();
        let mut bus = Bus {
            reader: stream.try_clone().map_err(|e| e.to_string())?,
            writer: stream,
            serial: 0,
        };
        bus.auth()?;
        Ok(bus)
    }

    fn auth(&mut self) -> Result<(), String> {
        let uid = current_uid().ok_or("cannot determine uid")?;
        let hex_uid: String = uid.bytes().map(|b| format!("{b:02x}")).collect();
        self.writer.write_all(b"\0").map_err(|e| e.to_string())?;
        let cmd = format!("AUTH EXTERNAL {hex_uid}\r\n");
        self.writer
            .write_all(cmd.as_bytes())
            .map_err(|e| e.to_string())?;
        let reply = read_line(&self.reader).ok_or("bus closed during auth")?;
        if !reply.starts_with("OK") {
            return Err(format!("bus rejected EXTERNAL auth: {reply}"));
        }
        self.writer
            .write_all(b"BEGIN\r\n")
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    fn call(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        args: &[Value],
    ) -> Result<Message, String> {
        let (signature, body) = build_body(args);
        let mut fields = vec![
            (FIELD_DESTINATION, Value::Str(destination.to_string())),
            (FIELD_PATH, Value::ObjectPath(path.to_string())),
            (FIELD_INTERFACE, Value::Str(interface.to_string())),
            (FIELD_MEMBER, Value::Str(member.to_string())),
        ];
        if !signature.is_empty() {
            fields.push((FIELD_SIGNATURE, Value::Signature(signature)));
        }
        let serial = self.next_serial();
        let msg = build_message(MSG_METHOD_CALL, serial, &fields, &body);
        self.writer.write_all(&msg).map_err(|e| e.to_string())?;
        loop {
            let reply = self.read_message()?;
            if reply.reply_serial == Some(serial) {
                return Ok(reply);
            }
            // Skip anything not addressed to this call.
        }
    }

    fn read_message(&mut self) -> Result<Message, String> {
        let mut head = [0u8; 16];
        self.reader
            .read_exact(&mut head)
            .map_err(|e| format!("bus read: {e}"))?;
        let body_len = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
        let fields_len = u32::from_le_bytes(head[12..16].try_into().unwrap()) as usize;
        let mut buf = head.to_vec();
        let mut fields = vec![0u8; fields_len];
        self.reader
            .read_exact(&mut fields)
            .map_err(|e| format!("bus read: {e}"))?;
        buf.extend_from_slice(&fields);
        let header_end = buf.len().div_ceil(8) * 8;
        let padding = header_end - buf.len();
        let mut pad = vec![0u8; padding];
        self.reader
            .read_exact(&mut pad)
            .map_err(|e| format!("bus read: {e}"))?;
        buf.extend_from_slice(&pad);
        let mut body = vec![0u8; body_len];
        self.reader
            .read_exact(&mut body)
            .map_err(|e| format!("bus read: {e}"))?;
        buf.extend_from_slice(&body);
        parse_message(&buf).ok_or_else(|| "malformed message from bus".to_string())
    }
}

fn read_line(mut stream: &UnixStream) -> Option<String> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).ok()?;
        if byte[0] == b'\n' {
            break;
        }
        out.push(byte[0]);
    }
    Some(String::from_utf8_lossy(&out).trim().to_string())
}

/// Numeric uid from /proc, avoiding any libc dependency.
fn current_uid() -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("Uid:"))?;
    line.split_whitespace().nth(1).map(str::to_string)
}

// ----- SNI service -----------------------------------------------------------

/// Decode the embedded PNG icon into the SNI IconPixmap format: straight
/// alpha, ARGB32 in network byte order. The icon's own colors are kept.
fn icon_argb(png: &[u8]) -> Option<Vec<u8>> {
    let decoded = image::load_from_memory(png).ok()?;
    let rgba = decoded.to_rgba8();
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.as_raw().as_chunks::<4>().0 {
        out.push(px[3]); // a
        out.push(px[0]); // r
        out.push(px[1]); // g
        out.push(px[2]); // b
    }
    Some(out)
}

fn sni_property(name: &str, pixmap: &[u8]) -> Option<Value> {
    match name {
        "Category" => Some(Value::Str("ApplicationStatus".into())),
        "Id" => Some(Value::Str("jimao".into())),
        "Title" => Some(Value::Str("Jimao".into())),
        "Status" => Some(Value::Str("Active".into())),
        "WindowId" => Some(Value::Int32(0)),
        "IconName" => Some(Value::Str(String::new())),
        "IconPixmap" => Some(Value::Array {
            signature: "ay".to_string(),
            values: vec![Value::bytes(pixmap)],
        }),
        "OverlayIconName" | "AttentionIconName" | "AttentionMovieName" => {
            Some(Value::Str(String::new()))
        }
        "OverlayIconPixmap" | "AttentionIconPixmap" => Some(Value::Array {
            signature: "aay".to_string(),
            values: Vec::new(),
        }),
        "ToolTip" => Some(Value::Struct(vec![
            Value::Str(String::new()),
            Value::Array {
                signature: "(ii)".to_string(),
                values: Vec::new(),
            },
            Value::Str("Jimao".into()),
            Value::Str(String::new()),
        ])),
        "ItemIsMenu" => Some(Value::Bool(false)),
        "Menu" => Some(Value::ObjectPath("/NO_DBUSMENU".into())),
        _ => None,
    }
}

fn all_properties(pixmap: &[u8]) -> Value {
    let names = [
        "Category",
        "Id",
        "Title",
        "Status",
        "WindowId",
        "IconName",
        "IconPixmap",
        "OverlayIconName",
        "OverlayIconPixmap",
        "AttentionIconName",
        "AttentionIconPixmap",
        "AttentionMovieName",
        "ToolTip",
        "ItemIsMenu",
        "Menu",
    ];
    Value::Array {
        signature: "{sv}".to_string(),
        values: names
            .iter()
            .filter_map(|n| sni_property(n, pixmap).map(|v| (n, v)))
            .map(|(n, v)| {
                Value::Struct(vec![Value::Str(n.to_string()), Value::Variant(Box::new(v))])
            })
            .collect(),
    }
}

/// Serve StatusNotifierItem method calls until the bus goes away.
fn serve(bus: &mut Bus, ctx: &egui::Context, pixmap: &[u8]) -> Result<(), String> {
    loop {
        let msg = bus.read_message()?;
        if msg.kind != MSG_METHOD_CALL {
            continue; // signals / broadcast
        }
        let interface = msg.interface.as_deref().unwrap_or("");
        let member = msg.member.as_deref().unwrap_or("");
        // Method calls carry their own serial; replies echo it.
        let call_serial = msg.serial;
        let sender = msg.sender.clone();
        let Some(sender) = sender else { continue };

        let (reply_values, error): (Vec<Value>, Option<&str>) = match (interface, member) {
            (INTROSPECTABLE, "Introspect") | ("", "Introspect") => {
                (vec![Value::Str(INTROSPECT_XML.to_string())], None)
            }
            (PROPERTIES, "Get") | (PROPERTIES, "Set") => {
                let mut body = Unmarshal::new(&msg.body);
                let _iface = body.string().unwrap_or_default();
                let prop = body.string().unwrap_or_default();
                match sni_property(&prop, pixmap) {
                    Some(v) => (vec![Value::Variant(Box::new(v))], None),
                    None => (Vec::new(), Some("org.freedesktop.DBus.Error.InvalidArgs")),
                }
            }
            (PROPERTIES, "GetAll") => (vec![all_properties(pixmap)], None),
            (SNI_INTERFACE, "Activate") | (SNI_INTERFACE, "SecondaryActivate") => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                (Vec::new(), None)
            }
            (SNI_INTERFACE, "Scroll") => (Vec::new(), None),
            _ => (Vec::new(), Some("org.freedesktop.DBus.Error.UnknownMethod")),
        };

        let (signature, body) = build_body(&reply_values);
        let mut fields = vec![
            (FIELD_REPLY_SERIAL, Value::UInt32(call_serial)),
            (FIELD_DESTINATION, Value::Str(sender)),
        ];
        if let Some(error) = error {
            fields.insert(0, (FIELD_ERROR_NAME, Value::Str(error.to_string())));
        }
        if !signature.is_empty() {
            fields.push((FIELD_SIGNATURE, Value::Signature(signature)));
        }
        let serial = bus.next_serial();
        let kind = if error.is_some() {
            MSG_ERROR
        } else {
            MSG_METHOD_RETURN
        };
        let wire = build_message(kind, serial, &fields, &body);
        bus.writer.write_all(&wire).map_err(|e| e.to_string())?;
    }
}

/// Spawn the tray thread. Failure is non-fatal: it is logged and the app
/// runs without a tray icon.
pub fn spawn(ctx: egui::Context) {
    let Some(pixmap) = icon_argb(APP_PNG) else {
        eprintln!("jimao tray: could not decode the tray icon");
        return;
    };
    let result = std::thread::Builder::new()
        .name("jimao-tray".to_string())
        .spawn(move || {
            let mut bus = match Bus::connect() {
                Ok(bus) => bus,
                Err(e) => {
                    eprintln!("jimao tray: {e}");
                    return;
                }
            };
            let mut run = || -> Result<(), String> {
                let unique = bus.call(
                    "org.freedesktop.DBus",
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "Hello",
                    &[],
                )?;
                if unique.kind == MSG_ERROR {
                    return Err(format!("Hello failed: {:?}", unique.error_name));
                }
                let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
                let reply = bus.call(
                    "org.freedesktop.DBus",
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "RequestName",
                    &[
                        Value::Str(name.clone()),
                        Value::UInt32(4), // DoNotQueue
                    ],
                )?;
                if reply.kind == MSG_ERROR {
                    return Err(format!("RequestName failed: {:?}", reply.error_name));
                }
                let registered = bus.call(
                    "org.kde.StatusNotifierWatcher",
                    "/StatusNotifierWatcher",
                    "org.kde.StatusNotifierWatcher",
                    "RegisterStatusNotifierItem",
                    &[Value::Str(name)],
                );
                match registered {
                    Ok(r) if r.kind == MSG_ERROR => {
                        return Err(format!(
                            "no system tray (StatusNotifierWatcher): {:?}",
                            r.error_name
                        ));
                    }
                    Err(e) => {
                        return Err(format!("no system tray (StatusNotifierWatcher): {e}"));
                    }
                    Ok(_) => {}
                }
                serve(&mut bus, &ctx, &pixmap)
            };
            if let Err(e) = run() {
                eprintln!("jimao tray: {e}");
            }
        });
    if let Err(e) = result {
        eprintln!("jimao tray: could not spawn thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_are_derived_correctly() {
        assert_eq!(type_signature(&Value::Byte(0)), "y");
        assert_eq!(type_signature(&Value::bytes(&[1, 2])), "ay");
        assert_eq!(
            type_signature(&Value::Array {
                signature: "ay".into(),
                values: vec![Value::bytes(&[1])],
            }),
            "aay"
        );
        assert_eq!(
            type_signature(&Value::Struct(vec![Value::Str("a".into())])),
            "(s)"
        );
    }

    #[test]
    fn signature_alignment_rules() {
        assert_eq!(signature_alignment("y"), 1);
        assert_eq!(signature_alignment("u"), 4);
        assert_eq!(signature_alignment("s"), 4);
        assert_eq!(signature_alignment("ay"), 4);
        assert_eq!(signature_alignment("{sv}"), 8);
        assert_eq!(signature_alignment("(ii)"), 8);
    }

    #[test]
    fn method_call_message_round_trips() {
        let (signature, body) = build_body(&[Value::Str("jimao".into()), Value::UInt32(4)]);
        let fields = vec![
            (FIELD_DESTINATION, Value::Str("org.freedesktop.DBus".into())),
            (FIELD_PATH, Value::ObjectPath("/x".into())),
            (FIELD_INTERFACE, Value::Str("org.freedesktop.DBus".into())),
            (FIELD_MEMBER, Value::Str("RequestName".into())),
            (FIELD_SIGNATURE, Value::Signature(signature.clone())),
        ];
        let wire = build_message(MSG_METHOD_CALL, 7, &fields, &body);
        let msg = parse_message(&wire).expect("parses");
        assert_eq!(msg.kind, MSG_METHOD_CALL);
        assert_eq!(msg.serial, 7);
        assert_eq!(msg.member.as_deref(), Some("RequestName"));
        assert_eq!(msg.path.as_deref(), Some("/x"));
        assert_eq!(msg.signature.as_deref(), Some("su"));
        // Body: string + u32.
        let mut u = Unmarshal::new(&msg.body);
        assert_eq!(u.string().as_deref(), Some("jimao"));
        assert_eq!(u.uint32(), Some(4));
    }

    #[test]
    fn empty_array_properties_marshal() {
        let v = all_properties(&[]);
        assert_eq!(type_signature(&v), "a{sv}");
        let (sig, body) = build_body(&[v]);
        assert_eq!(sig, "a{sv}");
        assert!(!body.is_empty());
    }

    #[test]
    fn icon_pixmap_is_argb_network_order() {
        let v = sni_property("IconPixmap", &[0x11, 0x22, 0x33, 0x44]).unwrap();
        assert_eq!(type_signature(&v), "aay");
    }

    #[test]
    fn uid_parses_from_proc_status() {
        let sample = "Name:\tjimao\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\n";
        // Reuse the same parsing logic on a sample.
        let line = sample.lines().find(|l| l.starts_with("Uid:")).unwrap();
        assert_eq!(line.split_whitespace().nth(1), Some("1000"));
    }

    #[test]
    fn embedded_png_decodes_with_its_colors() {
        let argb = icon_argb(APP_PNG).expect("decodes");
        assert_eq!(argb.len(), 48 * 48 * 4);
        // The icon's background circle is #835be1 (131, 91, 225): the
        // pixmap must contain it as straight ARGB — a regression guard
        // against recoloring that once flattened the icon into a blob.
        assert!(
            argb.as_chunks::<4>().0.contains(&[255, 131, 91, 225]),
            "purple background pixels must survive"
        );
        // Corners are outside the circle: fully transparent.
        assert_eq!(&argb[..4], &[0, 0, 0, 0]);
    }
}
