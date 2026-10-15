//! The session bus, in just enough D-Bus wire protocol for Kimo's own objects
//! and for the calls it makes on other people's.
//!
//! D-Bus frameworks were deliberately avoided: `zbus` brings an async executor
//! (hard rule 2) and the GTK bindings link C (hard rule 1), so this module
//! speaks the wire protocol itself — little-endian marshalling and
//! unmarshalling, message framing and `AUTH EXTERNAL` — on a plain
//! `UnixStream`. Both of the app's D-Bus users share it: `crate::tray` (a
//! `org.kde.StatusNotifierItem` and the `com.canonical.dbusmenu` behind it)
//! and `crate::notify` (a client of `org.freedesktop.Notifications`).

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;

/// D-Bus message types. (Signals, type 4, are received and skipped.)
pub(crate) const MSG_METHOD_CALL: u8 = 1;
pub(crate) const MSG_METHOD_RETURN: u8 = 2;
pub(crate) const MSG_ERROR: u8 = 3;

/// Header field codes.
pub(crate) const FIELD_PATH: u8 = 1;
pub(crate) const FIELD_INTERFACE: u8 = 2;
pub(crate) const FIELD_MEMBER: u8 = 3;
pub(crate) const FIELD_ERROR_NAME: u8 = 4;
pub(crate) const FIELD_REPLY_SERIAL: u8 = 5;
pub(crate) const FIELD_DESTINATION: u8 = 6;
pub(crate) const FIELD_SENDER: u8 = 7;
pub(crate) const FIELD_SIGNATURE: u8 = 8;

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
    pub(crate) fn bytes(data: &[u8]) -> Value {
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

pub(crate) fn build_body(values: &[Value]) -> (String, Vec<u8>) {
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

pub(crate) fn build_message(kind: u8, serial: u32, fields: &[(u8, Value)], body: &[u8]) -> Vec<u8> {
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
    pub(crate) kind: u8,
    pub(crate) serial: u32,
    pub(crate) path: Option<String>,
    pub(crate) interface: Option<String>,
    pub(crate) member: Option<String>,
    pub(crate) error_name: Option<String>,
    pub(crate) reply_serial: Option<u32>,
    pub(crate) sender: Option<String>,
    pub(crate) signature: Option<String>,
    pub(crate) body: Vec<u8>,
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

pub(crate) struct Unmarshal<'a> {
    data: &'a [u8],
    /// How far into the body the reader has come; the menu code walks it by
    /// hand to step over the entries it does not want.
    pub(crate) pos: usize,
}

impl<'a> Unmarshal<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Unmarshal { data, pos: 0 }
    }

    pub(crate) fn align(&mut self, to: usize) {
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

    pub(crate) fn uint32(&mut self) -> Option<u32> {
        self.align(4);
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    pub(crate) fn int32(&mut self) -> Option<i32> {
        self.align(4);
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    /// An `ai` array of int32s, excluding the array header.
    pub(crate) fn int32_array(&mut self) -> Option<Vec<i32>> {
        self.align(4);
        let len = self.uint32()? as usize;
        self.align(4);
        (0..len / 4)
            .map(|_| Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?)))
            .collect()
    }

    pub(crate) fn string(&mut self) -> Option<String> {
        let len = self.uint32()? as usize;
        let bytes = self.take(len)?;
        let s = String::from_utf8_lossy(bytes).into_owned();
        self.take(1)?; // nul
        Some(s)
    }

    pub(crate) fn signature(&mut self) -> Option<String> {
        let len = self.byte()? as usize;
        let bytes = self.take(len)?;
        let s = String::from_utf8_lossy(bytes).into_owned();
        self.take(1)?; // nul
        Some(s)
    }

    /// Step over one complete value of type `sig`. Used to skip variant
    /// payloads we do not care about (the `v` of an `EventGroup` event).
    pub(crate) fn skip_value(&mut self, sig: &str) -> Option<()> {
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
pub(crate) fn parse_message(buf: &[u8]) -> Option<Message> {
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

pub(crate) struct Bus {
    reader: UnixStream,
    writer: UnixStream,
    serial: u32,
}

impl Bus {
    pub(crate) fn connect() -> Result<Bus, String> {
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

    pub(crate) fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    pub(crate) fn call(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        args: &[Value],
    ) -> Result<Message, String> {
        let serial = self.call_no_reply(destination, path, interface, member, args)?;
        loop {
            let reply = self.read_message()?;
            if reply.reply_serial == Some(serial) {
                return Ok(reply);
            }
            // Skip anything not addressed to this call.
        }
    }

    /// Send a call and answer with its serial, without waiting for the reply —
    /// which is what a caller that may be answered by a *signal* needs, and the
    /// portals are exactly that: `FileChooser.Open` returns the object its answer
    /// will arrive on, and the answer itself is a signal that may even come
    /// first. A call that waited for the reply in here would read that signal
    /// and drop it.
    pub(crate) fn call_no_reply(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        args: &[Value],
    ) -> Result<u32, String> {
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
        Ok(serial)
    }

    /// Send a message that is not a call: the replies `tray::serve` writes.
    pub(crate) fn send(
        &mut self,
        kind: u8,
        fields: &[(u8, Value)],
        body: &[u8],
    ) -> Result<(), String> {
        let serial = self.next_serial();
        let wire = build_message(kind, serial, fields, body);
        self.writer.write_all(&wire).map_err(|e| e.to_string())
    }

    pub(crate) fn read_message(&mut self) -> Result<Message, String> {
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

/// An `a{sv}` dictionary built from (name, value) pairs.
pub(crate) fn properties_dict(props: &[(&str, Value)]) -> Value {
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
    fn uid_parses_from_proc_status() {
        let sample = "Name:\tkimo\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\n";
        // Reuse the same parsing logic on a sample.
        let line = sample.lines().find(|l| l.starts_with("Uid:")).unwrap();
        assert_eq!(line.split_whitespace().nth(1), Some("1000"));
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
