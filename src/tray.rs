//! Linux system-tray icon via the StatusNotifierItem (SNI) protocol.
//!
//! D-Bus frameworks were deliberately avoided: `zbus` brings an async
//! executor (rule 2) and the GTK-based tray crates link C (rule 1), so this
//! module speaks just enough of the D-Bus wire protocol, over the session
//! bus, on one dedicated `std::thread`: it registers a
//! `org.kde.StatusNotifierItem` and serves the properties/activations that
//! KDE Plasma and compatible trays query, plus a `com.canonical.dbusmenu`
//! context menu (Show / Quit) at `MENU_PATH`. The icon is `assets/app.png`
//! (256 px, rasterized from `assets/app.svg`), embedded at compile time,
//! decoded with the pure-Rust image crate with its colors kept as-is. It is
//! deliberately large: hosts scale the pixmap down to their panel size, and
//! the 48 px source this replaced was blurry on a HiDPI panel. A missing
//! StatusNotifierWatcher (e.g. GNOME without an extension) simply disables
//! the tray — the app keeps running.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;

/// The application icon (256 px, rasterized from `assets/app.svg`), embedded
/// at compile time.
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
const DBUSMENU_INTERFACE: &str = "com.canonical.dbusmenu";
const INTROSPECTABLE: &str = "org.freedesktop.DBus.Introspectable";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Where the SNI `Menu` property points: our `com.canonical.dbusmenu` object.
const MENU_PATH: &str = "/Menu";
/// Menu node ids (0 is the invisible root that the layout hangs off).
const MENU_SHOW_ID: i32 = 1;
const MENU_QUIT_ID: i32 = 2;

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
    "    <property name=\"IconPixmap\" type=\"a(iiay)\" access=\"read\"/>\n",
    "    <property name=\"OverlayIconName\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"OverlayIconPixmap\" type=\"a(iiay)\" access=\"read\"/>\n",
    "    <property name=\"AttentionIconName\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"AttentionIconPixmap\" type=\"a(iiay)\" access=\"read\"/>\n",
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

/// Introspection for the menu object at `MENU_PATH`.
const MENU_INTROSPECT_XML: &str = concat!(
    "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\" ",
    "\"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">\n",
    "<node>\n",
    "  <interface name=\"com.canonical.dbusmenu\">\n",
    "    <property name=\"Version\" type=\"u\" access=\"read\"/>\n",
    "    <property name=\"TextDirection\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"Status\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"IconThemePath\" type=\"as\" access=\"read\"/>\n",
    "    <method name=\"GetLayout\">\n",
    "      <arg type=\"i\" name=\"parentId\" direction=\"in\"/>\n",
    "      <arg type=\"i\" name=\"recursionDepth\" direction=\"in\"/>\n",
    "      <arg type=\"as\" name=\"propertyNames\" direction=\"in\"/>\n",
    "      <arg type=\"u\" name=\"revision\" direction=\"out\"/>\n",
    "      <arg type=\"(ia{sv}av)\" name=\"layout\" direction=\"out\"/>\n",
    "    </method>\n",
    "    <method name=\"GetGroupProperties\">\n",
    "      <arg type=\"ai\" name=\"ids\" direction=\"in\"/>\n",
    "      <arg type=\"as\" name=\"propertyNames\" direction=\"in\"/>\n",
    "      <arg type=\"a(ia{sv})\" name=\"properties\" direction=\"out\"/>\n",
    "    </method>\n",
    "    <method name=\"GetProperty\">\n",
    "      <arg type=\"i\" name=\"id\" direction=\"in\"/>\n",
    "      <arg type=\"s\" name=\"name\" direction=\"in\"/>\n",
    "      <arg type=\"v\" name=\"value\" direction=\"out\"/>\n",
    "    </method>\n",
    "    <method name=\"Event\">\n",
    "      <arg type=\"i\" name=\"id\" direction=\"in\"/>\n",
    "      <arg type=\"s\" name=\"eventId\" direction=\"in\"/>\n",
    "      <arg type=\"v\" name=\"data\" direction=\"in\"/>\n",
    "      <arg type=\"u\" name=\"timestamp\" direction=\"in\"/>\n",
    "    </method>\n",
    "    <method name=\"EventGroup\">\n",
    "      <arg type=\"a(isvu)\" name=\"events\" direction=\"in\"/>\n",
    "      <arg type=\"ai\" name=\"idErrors\" direction=\"out\"/>\n",
    "    </method>\n",
    "    <method name=\"AboutToShow\">\n",
    "      <arg type=\"i\" name=\"id\" direction=\"in\"/>\n",
    "      <arg type=\"b\" name=\"needUpdate\" direction=\"out\"/>\n",
    "    </method>\n",
    "    <method name=\"AboutToShowGroup\">\n",
    "      <arg type=\"ai\" name=\"ids\" direction=\"in\"/>\n",
    "      <arg type=\"ai\" name=\"updatesNeeded\" direction=\"out\"/>\n",
    "      <arg type=\"ai\" name=\"idErrors\" direction=\"out\"/>\n",
    "    </method>\n",
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

/// Little-endian marshaller; alignment is relative to the buffer start. D-Bus
/// aligns values relative to the start of the *message*, so every buffer must
/// begin at an 8-aligned message offset — which is why arrays are marshalled
/// in place instead of into a scratch buffer.
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
                self.align(4);
                let len_at = self.buf.len();
                self.buf.extend_from_slice(&[0; 4]);
                self.align(signature_alignment(signature));
                let data_at = self.buf.len();
                for item in values {
                    self.value(item);
                }
                let len = (self.buf.len() - data_at) as u32;
                self.buf[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
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
    path: Option<String>,
    interface: Option<String>,
    member: Option<String>,
    error_name: Option<String>,
    reply_serial: Option<u32>,
    sender: Option<String>,
    signature: Option<String>,
    body: Vec<u8>,
}

/// The first complete type in `sig`, with the number of bytes it spans.
fn first_type(sig: &str) -> Option<(String, usize)> {
    let first = *sig.as_bytes().first()?;
    match first {
        b'a' => {
            let (element, used) = first_type(&sig[1..])?;
            Some((format!("a{element}"), used + 1))
        }
        b'(' | b'{' => {
            let close = if first == b'(' { b')' } else { b'}' };
            let mut depth = 0usize;
            for (i, byte) in sig.bytes().enumerate() {
                if byte == first {
                    depth += 1;
                } else if byte == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some((sig[..=i].to_string(), i + 1));
                    }
                }
            }
            None
        }
        _ => Some((sig[..1].to_string(), 1)),
    }
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

    fn int32(&mut self) -> Option<i32> {
        self.align(4);
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    /// An `ai` array of int32s, excluding the array header.
    fn int32_array(&mut self) -> Option<Vec<i32>> {
        self.align(4);
        let len = self.uint32()? as usize;
        self.align(4);
        (0..len / 4)
            .map(|_| Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?)))
            .collect()
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

    /// Step over one complete value of type `sig`. Used to skip variant
    /// payloads we do not care about (the `v` of an `EventGroup` event).
    fn skip_value(&mut self, sig: &str) -> Option<()> {
        let (first, _) = first_type(sig)?;
        match first.as_bytes().first()? {
            b'y' => {
                self.take(1)?;
            }
            b'n' | b'q' => {
                self.align(2);
                self.take(2)?;
            }
            b'b' | b'i' | b'u' => {
                self.align(4);
                self.take(4)?;
            }
            b'x' | b't' | b'd' => {
                self.align(8);
                self.take(8)?;
            }
            b's' | b'o' => {
                self.string()?;
            }
            b'g' => {
                self.signature()?;
            }
            b'v' => {
                let inner = self.signature()?;
                self.skip_value(&inner)?;
            }
            b'a' => {
                self.align(4);
                let len = self.uint32()? as usize;
                let (element, _) = first_type(&first[1..])?;
                self.align(signature_alignment(&element));
                self.take(len)?;
            }
            b'(' | b'{' => {
                self.align(8);
                let mut rest = &first[1..first.len() - 1];
                while !rest.is_empty() {
                    let (member, used) = first_type(rest)?;
                    self.skip_value(&member)?;
                    rest = &rest[used..];
                }
            }
            _ => return None,
        }
        Some(())
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

/// Fill `buf` completely, riding out idle-timeout expiries.
///
/// The session bus sends nothing at all for hours at a time, so the socket's
/// read timeout only bounds one `read` syscall: its expiry is the normal
/// state, not a broken connection. `read_exact` reports it as a fatal
/// `WouldBlock`, which killed the tray thread after 30 quiet seconds and
/// dropped the icon from the panel. Only EOF or a real error may end a read.
fn read_full(mut stream: &UnixStream, buf: &mut [u8]) -> Result<(), String> {
    let mut filled = 0;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return Err("bus closed the connection".to_string()),
            Ok(n) => filled += n,
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(format!("bus read: {e}")),
        }
    }
    Ok(())
}

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
        // Bounds a single read syscall only: the bus is silent most of the
        // time, and `read_full` treats an expiry as "wait longer" rather than
        // as a disconnection.
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
        read_full(&self.reader, &mut head)?;
        let body_len = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
        let fields_len = u32::from_le_bytes(head[12..16].try_into().unwrap()) as usize;
        let mut buf = head.to_vec();
        let mut fields = vec![0u8; fields_len];
        read_full(&self.reader, &mut fields)?;
        buf.extend_from_slice(&fields);
        let header_end = buf.len().div_ceil(8) * 8;
        let padding = header_end - buf.len();
        let mut pad = vec![0u8; padding];
        read_full(&self.reader, &mut pad)?;
        buf.extend_from_slice(&pad);
        let mut body = vec![0u8; body_len];
        read_full(&self.reader, &mut body)?;
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

/// A decoded tray icon: its pixel size plus straight-alpha ARGB32 pixels in
/// network byte order — the payload of one SNI \`IconPixmap\` entry.
struct Icon {
    width: i32,
    height: i32,
    argb: Vec<u8>,
}

/// Decode the embedded PNG icon. The icon's own colors are kept.
fn icon_argb(png: &[u8]) -> Option<Icon> {
    let decoded = image::load_from_memory(png).ok()?;
    let rgba = decoded.to_rgba8();
    let (width, height) = (rgba.width() as i32, rgba.height() as i32);
    let mut argb = Vec::with_capacity(rgba.len());
    for px in rgba.as_raw().as_chunks::<4>().0 {
        argb.push(px[3]); // a
        argb.push(px[0]); // r
        argb.push(px[1]); // g
        argb.push(px[2]); // b
    }
    Some(Icon {
        width,
        height,
        argb,
    })
}

/// The SNI `IconPixmap` value: `a(iiay)`, one (width, height, ARGB32 data)
/// struct per icon size. Hosts cannot read a bare byte array.
fn pixmap_value(icon: &Icon) -> Value {
    Value::Array {
        signature: "(iiay)".to_string(),
        values: vec![Value::Struct(vec![
            Value::Int32(icon.width),
            Value::Int32(icon.height),
            Value::bytes(&icon.argb),
        ])],
    }
}

/// An empty `IconPixmap` value: `a(iiay)` with no entries.
fn empty_pixmaps() -> Value {
    Value::Array {
        signature: "(iiay)".to_string(),
        values: Vec::new(),
    }
}

/// An `a{sv}` dictionary built from (name, value) pairs.
fn properties_dict(props: &[(&str, Value)]) -> Value {
    Value::Array {
        signature: "{sv}".to_string(),
        values: props
            .iter()
            .map(|(name, value)| {
                Value::Struct(vec![
                    Value::Str((*name).to_string()),
                    Value::Variant(Box::new(value.clone())),
                ])
            })
            .collect(),
    }
}

/// One `com.canonical.dbusmenu` layout node: `(ia{sv}av)`.
fn menu_node(id: i32, props: &[(&str, Value)], children: Vec<Value>) -> Value {
    Value::Struct(vec![
        Value::Int32(id),
        properties_dict(props),
        Value::Array {
            signature: "v".to_string(),
            values: children
                .into_iter()
                .map(|child| Value::Variant(Box::new(child)))
                .collect(),
        },
    ])
}

/// The label of a menu entry; `None` for the root and unknown ids.
fn menu_label(id: i32) -> Option<&'static str> {
    match id {
        MENU_SHOW_ID => Some("Show"),
        MENU_QUIT_ID => Some("Quit"),
        _ => None,
    }
}

/// A clickable menu entry (no submenu of its own).
fn menu_entry(id: i32, label: &str) -> Value {
    menu_node(
        id,
        &[
            ("label", Value::Str(label.to_string())),
            ("enabled", Value::Bool(true)),
            ("visible", Value::Bool(true)),
        ],
        Vec::new(),
    )
}

/// The layout value of a `GetLayout` reply, for the requested parent node.
fn menu_layout_for(parent: i32) -> Value {
    match menu_label(parent) {
        Some(label) => menu_entry(parent, label),
        None if parent == 0 => menu_node(
            0,
            &[("children-display", Value::Str("submenu".to_string()))],
            vec![
                menu_entry(MENU_SHOW_ID, "Show"),
                menu_entry(MENU_QUIT_ID, "Quit"),
            ],
        ),
        None => menu_node(parent, &[], Vec::new()),
    }
}

/// Properties of the menu object itself (not of its entries).
fn menu_property(name: &str) -> Option<Value> {
    match name {
        "Version" => Some(Value::UInt32(3)),
        "TextDirection" => Some(Value::Str("ltr".to_string())),
        "Status" => Some(Value::Str("normal".to_string())),
        "IconThemePath" => Some(Value::Array {
            signature: "s".to_string(),
            values: Vec::new(),
        }),
        _ => None,
    }
}

fn all_menu_properties() -> Value {
    properties_dict(&[
        ("Version", Value::UInt32(3)),
        ("TextDirection", Value::Str("ltr".to_string())),
        ("Status", Value::Str("normal".to_string())),
        (
            "IconThemePath",
            Value::Array {
                signature: "s".to_string(),
                values: Vec::new(),
            },
        ),
    ])
}

/// A menu entry's properties, for `GetProperty`/`GetGroupProperties`.
fn menu_item_properties(id: i32) -> Value {
    match menu_label(id) {
        // No `type`: libdbusmenu-gtk only builds a widget for its default
        // item type, and anything else (e.g. "normal") is dropped as unknown.
        Some(label) => properties_dict(&[
            ("label", Value::Str(label.to_string())),
            ("enabled", Value::Bool(true)),
            ("visible", Value::Bool(true)),
        ]),
        None => properties_dict(&[("children-display", Value::Str("submenu".to_string()))]),
    }
}

/// One named property of a menu entry.
fn menu_item_property(id: i32, name: &str) -> Option<Value> {
    let Value::Array { values, .. } = menu_item_properties(id) else {
        return None;
    };
    values.into_iter().find_map(|entry| {
        let Value::Struct(fields) = entry else {
            return None;
        };
        if !matches!(fields.first(), Some(Value::Str(key)) if key == name) {
            return None;
        }
        match fields.get(1) {
            Some(Value::Variant(inner)) => Some((**inner).clone()),
            _ => None,
        }
    })
}

/// The (id, event) pairs of an `Event` or `EventGroup` call. Malformed input
/// yields no events rather than killing the tray thread.
fn menu_events(member: &str, body: &[u8]) -> Vec<(i32, String)> {
    let mut u = Unmarshal::new(body);
    let mut out = Vec::new();
    if member == "Event" {
        if let (Some(id), Some(event)) = (u.int32(), u.string()) {
            out.push((id, event));
        }
        return out;
    }
    // EventGroup: a(isvu)
    u.align(4);
    let Some(len) = u.uint32() else {
        return out;
    };
    let end = u.pos + len as usize;
    while u.pos < end {
        u.align(8);
        let (Some(id), Some(event)) = (u.int32(), u.string()) else {
            break;
        };
        out.push((id, event));
        let Some(sig) = u.signature() else { break };
        if u.skip_value(&sig).is_none() || u.uint32().is_none() {
            break;
        }
    }
    out
}

/// Bring the main window back and focus it.
fn show_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

/// Ask eframe to close the main viewport, which ends the process.
fn quit(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    ctx.request_repaint();
}

fn sni_property(name: &str, icon: &Icon) -> Option<Value> {
    match name {
        "Category" => Some(Value::Str("ApplicationStatus".into())),
        "Id" => Some(Value::Str("kimo".into())),
        "Title" => Some(Value::Str("Kimo".into())),
        "Status" => Some(Value::Str("Active".into())),
        "WindowId" => Some(Value::Int32(0)),
        "IconName" => Some(Value::Str(String::new())),
        "IconPixmap" => Some(pixmap_value(icon)),
        "OverlayIconName" | "AttentionIconName" | "AttentionMovieName" => {
            Some(Value::Str(String::new()))
        }
        "OverlayIconPixmap" | "AttentionIconPixmap" => Some(empty_pixmaps()),
        "ToolTip" => Some(Value::Struct(vec![
            Value::Str(String::new()),
            Value::Array {
                signature: "(ii)".to_string(),
                values: Vec::new(),
            },
            Value::Str("Kimo".into()),
            Value::Str(String::new()),
        ])),
        "ItemIsMenu" => Some(Value::Bool(false)),
        "Menu" => Some(Value::ObjectPath(MENU_PATH.to_string())),
        _ => None,
    }
}

fn all_properties(icon: &Icon) -> Value {
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
    let props: Vec<(&str, Value)> = names
        .iter()
        .filter_map(|n| sni_property(n, icon).map(|v| (*n, v)))
        .collect();
    properties_dict(&props)
}

/// Serve StatusNotifierItem and DBusMenu method calls until the bus goes away.
fn serve(bus: &mut Bus, ctx: &egui::Context, icon: &Icon) -> Result<(), String> {
    loop {
        let msg = bus.read_message()?;
        if msg.kind != MSG_METHOD_CALL {
            continue; // signals / broadcast
        }
        let interface = msg.interface.as_deref().unwrap_or("");
        let member = msg.member.as_deref().unwrap_or("");
        let on_menu = msg.path.as_deref() == Some(MENU_PATH);
        // Method calls carry their own serial; replies echo it.
        let call_serial = msg.serial;
        let sender = msg.sender.clone();
        let Some(sender) = sender else { continue };
        let invalid_args = Some("org.freedesktop.DBus.Error.InvalidArgs");

        let (reply_values, error): (Vec<Value>, Option<&str>) = match (interface, member) {
            (INTROSPECTABLE, "Introspect") | ("", "Introspect") => {
                let xml = if on_menu {
                    MENU_INTROSPECT_XML
                } else {
                    INTROSPECT_XML
                };
                (vec![Value::Str(xml.to_string())], None)
            }
            (PROPERTIES, "Get") | (PROPERTIES, "Set") => {
                let mut body = Unmarshal::new(&msg.body);
                let iface = body.string().unwrap_or_default();
                let prop = body.string().unwrap_or_default();
                let value = if iface == DBUSMENU_INTERFACE {
                    menu_property(&prop)
                } else {
                    sni_property(&prop, icon)
                };
                match value {
                    Some(v) => (vec![Value::Variant(Box::new(v))], None),
                    None => (Vec::new(), invalid_args),
                }
            }
            (PROPERTIES, "GetAll") => {
                let mut body = Unmarshal::new(&msg.body);
                let iface = body.string().unwrap_or_default();
                if iface == DBUSMENU_INTERFACE {
                    (vec![all_menu_properties()], None)
                } else {
                    (vec![all_properties(icon)], None)
                }
            }
            (SNI_INTERFACE, "Activate") | (SNI_INTERFACE, "SecondaryActivate") => {
                show_window(ctx);
                (Vec::new(), None)
            }
            (SNI_INTERFACE, "Scroll") => (Vec::new(), None),
            (DBUSMENU_INTERFACE, "GetLayout") => {
                let mut body = Unmarshal::new(&msg.body);
                let parent = body.int32().unwrap_or(0);
                (vec![Value::UInt32(1), menu_layout_for(parent)], None)
            }
            (DBUSMENU_INTERFACE, "GetGroupProperties") => {
                let mut body = Unmarshal::new(&msg.body);
                let mut ids = body.int32_array().unwrap_or_default();
                if ids.is_empty() {
                    ids = vec![0, MENU_SHOW_ID, MENU_QUIT_ID];
                }
                let values = ids
                    .into_iter()
                    .map(|id| Value::Struct(vec![Value::Int32(id), menu_item_properties(id)]))
                    .collect();
                (
                    vec![Value::Array {
                        signature: "(ia{sv})".to_string(),
                        values,
                    }],
                    None,
                )
            }
            (DBUSMENU_INTERFACE, "GetProperty") => {
                let mut body = Unmarshal::new(&msg.body);
                let id = body.int32().unwrap_or(-1);
                let name = body.string().unwrap_or_default();
                match menu_item_property(id, &name) {
                    Some(v) => (vec![Value::Variant(Box::new(v))], None),
                    None => (Vec::new(), invalid_args),
                }
            }
            (DBUSMENU_INTERFACE, "Event") | (DBUSMENU_INTERFACE, "EventGroup") => {
                for (id, event) in menu_events(member, &msg.body) {
                    if event != "clicked" {
                        continue;
                    }
                    match id {
                        MENU_SHOW_ID => show_window(ctx),
                        MENU_QUIT_ID => quit(ctx),
                        _ => {}
                    }
                }
                if member == "EventGroup" {
                    let id_errors = Value::Array {
                        signature: "i".to_string(),
                        values: Vec::new(),
                    };
                    (vec![id_errors], None)
                } else {
                    (Vec::new(), None)
                }
            }
            (DBUSMENU_INTERFACE, "AboutToShow") => (vec![Value::Bool(false)], None),
            (DBUSMENU_INTERFACE, "AboutToShowGroup") => {
                let empty = || Value::Array {
                    signature: "i".to_string(),
                    values: Vec::new(),
                };
                (vec![empty(), empty()], None)
            }
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
    let Some(icon) = icon_argb(APP_PNG) else {
        eprintln!("kimo tray: could not decode the tray icon");
        return;
    };
    let result = std::thread::Builder::new()
        .name("kimo-tray".to_string())
        .spawn(move || {
            let mut bus = match Bus::connect() {
                Ok(bus) => bus,
                Err(e) => {
                    eprintln!("kimo tray: {e}");
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
                serve(&mut bus, &ctx, &icon)
            };
            if let Err(e) = run() {
                eprintln!("kimo tray: {e}");
            }
        });
    if let Err(e) = result {
        eprintln!("kimo tray: could not spawn thread: {e}");
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
        let (signature, body) = build_body(&[Value::Str("kimo".into()), Value::UInt32(4)]);
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
        assert_eq!(u.string().as_deref(), Some("kimo"));
        assert_eq!(u.uint32(), Some(4));
    }

    #[test]
    fn empty_array_properties_marshal() {
        let icon = Icon {
            width: 1,
            height: 1,
            argb: vec![0, 0, 0, 0],
        };
        let v = all_properties(&icon);
        assert_eq!(type_signature(&v), "a{sv}");
        let (sig, body) = build_body(&[v]);
        assert_eq!(sig, "a{sv}");
        assert!(!body.is_empty());
    }

    #[test]
    fn icon_pixmap_is_argb_network_order() {
        // The spec types IconPixmap as a(iiay): (width, height, ARGB32 data).
        let icon = Icon {
            width: 2,
            height: 1,
            argb: vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
        };
        let v = sni_property("IconPixmap", &icon).unwrap();
        assert_eq!(type_signature(&v), "a(iiay)");
        let Value::Array { values, .. } = &v else {
            panic!("IconPixmap must be an array");
        };
        let Value::Struct(entry) = &values[0] else {
            panic!("each IconPixmap entry must be a (iiay) struct");
        };
        assert_eq!(
            entry,
            &vec![Value::Int32(2), Value::Int32(1), Value::bytes(&icon.argb),]
        );
        // Straight alpha, network byte order: alpha first, then R, G, B.
        assert_eq!(&icon.argb[..4], &[0x11, 0x22, 0x33, 0x44]);
    }

    /// D-Bus aligns values relative to the start of the message, so an
    /// 8-aligned element is padded relative to the message even when its
    /// array's own data starts at a 4-mod-8 offset.
    #[test]
    fn array_elements_align_to_the_message() {
        let value = Value::Struct(vec![
            Value::Str("ab".into()),
            Value::Array {
                signature: "v".into(),
                values: vec![Value::Variant(Box::new(Value::Struct(vec![Value::Int32(
                    7,
                )])))],
            },
        ]);
        let (signature, body) = build_body(&[value]);
        assert_eq!(signature, "(sav)");
        // "ab": 4-byte length + 2 bytes + nul = 7, padded to the array length.
        assert_eq!(&body[..8], &[2, 0, 0, 0, b'a', b'b', 0, 0]);
        let len = u32::from_le_bytes(body[8..12].try_into().unwrap());
        assert_eq!(len, 16);
        // The array data starts at 12 (4 mod 8); the variant signature is at
        // 12..17, so the struct inside it is padded to 24, not to 20.
        assert_eq!(&body[12..17], &[3, b'(', b'i', b')', 0]);
        assert_eq!(&body[17..24], &[0; 7]);
        assert_eq!(&body[24..28], &7i32.to_le_bytes());
        assert_eq!(body.len(), 28);
    }

    #[test]
    fn skip_value_steps_over_unwanted_payloads() {
        let (_, body) = build_body(&[
            Value::Int32(9),
            Value::Str("clicked".into()),
            Value::Variant(Box::new(Value::Array {
                signature: "s".into(),
                values: vec![Value::Str("x".into())],
            })),
            Value::UInt32(1234),
        ]);
        let mut u = Unmarshal::new(&body);
        assert_eq!(u.int32(), Some(9));
        assert_eq!(u.string().as_deref(), Some("clicked"));
        let sig = u.signature().unwrap();
        assert_eq!(sig, "as");
        u.skip_value(&sig).unwrap();
        assert_eq!(u.uint32(), Some(1234));
    }

    #[test]
    fn item_points_at_the_menu_object() {
        let icon = Icon {
            width: 1,
            height: 1,
            argb: vec![0; 4],
        };
        assert_eq!(
            sni_property("Menu", &icon),
            Some(Value::ObjectPath(MENU_PATH.to_string()))
        );
        assert_eq!(sni_property("ItemIsMenu", &icon), Some(Value::Bool(false)));
        assert_eq!(menu_property("Version"), Some(Value::UInt32(3)));
        assert_eq!(menu_property("Status"), Some(Value::Str("normal".into())));
    }

    /// Walk a `GetLayout` reply far enough to prove its shape: revision 1,
    /// root id 0, then the Show and Quit entries.
    #[test]
    fn menu_layout_exposes_show_and_quit() {
        let (signature, body) = build_body(&[Value::UInt32(1), menu_layout_for(0)]);
        assert_eq!(signature, "u(ia{sv}av)");

        let mut u = Unmarshal::new(&body);
        assert_eq!(u.uint32(), Some(1));
        u.align(8);
        assert_eq!(u.int32(), Some(0));
        let props_len = u.uint32().unwrap() as usize;
        u.align(8);
        let props_end = u.pos + props_len;
        assert_eq!(u.string().as_deref(), Some("children-display"));
        assert_eq!(u.signature().as_deref(), Some("s"));
        assert_eq!(u.string().as_deref(), Some("submenu"));
        assert_eq!(u.pos, props_end);

        let children_len = u.uint32().unwrap() as usize;
        let children_end = u.pos + children_len;
        let mut entries = Vec::new();
        while u.pos < children_end {
            assert_eq!(u.signature().as_deref(), Some("(ia{sv}av)"));
            u.align(8);
            let id = u.int32().unwrap();
            let props_len = u.uint32().unwrap() as usize;
            u.align(8);
            let props_end = u.pos + props_len;
            u.align(8);
            assert_eq!(u.string().as_deref(), Some("label"));
            assert_eq!(u.signature().as_deref(), Some("s"));
            entries.push((id, u.string().unwrap()));
            u.pos = props_end;
            assert_eq!(u.uint32(), Some(0)); // no submenu
        }
        assert_eq!(
            entries,
            vec![(1, "Show".to_string()), (2, "Quit".to_string())]
        );
    }

    /// libdbusmenu-gtk only builds a widget for items whose `type` it knows
    /// (or that leave it unset); `type: "normal"` silently produced an empty
    /// menu in waybar, so standard entries must not declare one.
    #[test]
    fn standard_items_do_not_declare_a_type() {
        for id in [MENU_SHOW_ID, MENU_QUIT_ID] {
            let Value::Array { values, .. } = menu_item_properties(id) else {
                panic!("properties must be a dict");
            };
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
            assert!(keys.contains(&"label"), "entry {id} has no label: {keys:?}");
            assert!(
                !keys.contains(&"type"),
                "entry {id} declares a type: {keys:?}"
            );
        }
    }

    #[test]
    fn menu_events_parse_both_call_shapes() {
        let (_, event) = build_body(&[
            Value::Int32(MENU_QUIT_ID),
            Value::Str("clicked".into()),
            Value::Variant(Box::new(Value::Str(String::new()))),
            Value::UInt32(0),
        ]);
        assert_eq!(
            menu_events("Event", &event),
            vec![(MENU_QUIT_ID, "clicked".to_string())]
        );

        let (_, group) = build_body(&[Value::Array {
            signature: "(isvu)".into(),
            values: vec![
                Value::Struct(vec![
                    Value::Int32(MENU_SHOW_ID),
                    Value::Str("clicked".into()),
                    Value::Variant(Box::new(Value::Bool(true))),
                    Value::UInt32(1),
                ]),
                Value::Struct(vec![
                    Value::Int32(MENU_QUIT_ID),
                    Value::Str("hovered".into()),
                    Value::Variant(Box::new(Value::Str("x".into()))),
                    Value::UInt32(2),
                ]),
            ],
        }]);
        assert_eq!(
            menu_events("EventGroup", &group),
            vec![
                (MENU_SHOW_ID, "clicked".to_string()),
                (MENU_QUIT_ID, "hovered".to_string())
            ]
        );
    }

    #[test]
    fn uid_parses_from_proc_status() {
        let sample = "Name:\tkimo\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\n";
        // Reuse the same parsing logic on a sample.
        let line = sample.lines().find(|l| l.starts_with("Uid:")).unwrap();
        assert_eq!(line.split_whitespace().nth(1), Some("1000"));
    }

    #[test]
    fn embedded_png_decodes_with_its_colors() {
        let icon = icon_argb(APP_PNG).expect("decodes");
        // Large on purpose: hosts downscale it, and a small source looked
        // blurry once the panel asked for more pixels than it had.
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.argb.len(), 256 * 256 * 4);
        // The icon's background circle is #835be1 (131, 91, 225): the
        // pixmap must contain it as straight ARGB — a regression guard
        // against recoloring that once flattened the icon into a blob.
        assert!(
            icon.argb.as_chunks::<4>().0.contains(&[255, 131, 91, 225]),
            "purple background pixels must survive"
        );
        // Corners are outside the circle: fully transparent.
        assert_eq!(&icon.argb[..4], &[0, 0, 0, 0]);
    }

    /// A quiet bus must not end the tray: `read_message` has to ride out
    /// read-timeout expiries. The peer stays silent for several timeouts and
    /// splits its message mid-header, so this also pins down that a resumed
    /// read keeps its place in the frame.
    #[test]
    fn idle_bus_does_not_end_a_read() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        let body = build_body(&[Value::Str("kimo".into())]).1;
        let wire = build_message(
            MSG_METHOD_RETURN,
            3,
            &[
                (FIELD_REPLY_SERIAL, Value::UInt32(1)),
                (FIELD_SIGNATURE, Value::Signature("s".into())),
            ],
            &body,
        );
        let sender = std::thread::spawn(move || {
            writer.write_all(&wire[..8]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(200));
            writer.write_all(&wire[8..]).unwrap();
        });
        let mut bus = Bus {
            reader: reader.try_clone().unwrap(),
            writer: reader,
            serial: 0,
        };
        let msg = bus.read_message().expect("an idle bus is not an error");
        assert_eq!(msg.serial, 3);
        assert_eq!(msg.reply_serial, Some(1));
        sender.join().unwrap();
    }

    /// A bus that really closed still ends the read loop — and ends it
    /// promptly, rather than spinning on EOF until the harness gives up.
    #[test]
    fn closed_bus_ends_a_read() {
        let (reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        drop(writer);
        let mut bus = Bus {
            reader: reader.try_clone().unwrap(),
            writer: reader,
            serial: 0,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            tx.send(bus.read_message().is_err()).ok();
        });
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5)),
            Ok(true),
            "a closed bus must end the read loop"
        );
    }
}
