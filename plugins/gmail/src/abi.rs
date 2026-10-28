//! The Kimo host imports and the JSON envelope, as `docs/mail-spec.md` §8
//! declares them. Nothing here knows Gmail: this is the boundary every
//! provider module shares.
//!
//! The imports are declared for `wasm32` and stubbed for every other target,
//! with the same paths, so the whole plugin builds and its parsing half runs
//! under plain `cargo test` on the host.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// Pointers and lengths cross as `usize`: on wasm32 that *is* the ABI's i32,
// and on a 64-bit host (the test build) it carries a real pointer without
// the truncation an i32 parameter would silently impose.
#[cfg(target_arch = "wasm32")]
mod raw {
    #[link(wasm_import_module = "kimo")]
    extern "C" {
        pub(crate) fn log(level: i32, ptr: usize, len: usize);
        pub(crate) fn now_ms() -> i64;
        pub(crate) fn cancelled() -> i32;
        pub(crate) fn random(ptr: usize, len: usize) -> i32;
        pub(crate) fn http_request(
            request_ptr: usize,
            request_len: usize,
            out_ptr: usize,
            out_cap: usize,
        ) -> i32;
        pub(crate) fn net_open(host_ptr: usize, host_len: usize, port: i32, mode: i32) -> i32;
        pub(crate) fn net_read(id: i32, ptr: usize, len: usize) -> i32;
        pub(crate) fn net_write(id: i32, ptr: usize, len: usize) -> i32;
        pub(crate) fn net_close(id: i32);
    }
}

// The host-side stand-ins for the imports: a fixed clock, a deterministic
// byte pattern instead of the CSPRNG, and refusals where the sandbox would
// reach the network. Tests that need real randomness or a socket do not run
// on the host; the point of these is that the code above them does.
#[cfg(not(target_arch = "wasm32"))]
mod raw {
    pub(crate) unsafe fn log(_level: i32, _ptr: usize, _len: usize) {}
    pub(crate) unsafe fn now_ms() -> i64 {
        1_700_000_000_000
    }
    pub(crate) unsafe fn cancelled() -> i32 {
        0
    }
    pub(crate) unsafe fn random(ptr: usize, len: usize) -> i32 {
        for index in 0..len {
            *((ptr + index) as *mut u8) = ((index % 251) + 1) as u8;
        }
        len as i32
    }
    pub(crate) unsafe fn http_request(
        _request_ptr: usize,
        _request_len: usize,
        _out_ptr: usize,
        _out_cap: usize,
    ) -> i32 {
        -1
    }
    pub(crate) unsafe fn net_open(
        _host_ptr: usize,
        _host_len: usize,
        _port: i32,
        _mode: i32,
    ) -> i32 {
        -1
    }
    pub(crate) unsafe fn net_read(_id: i32, _ptr: usize, _len: usize) -> i32 {
        -1
    }
    pub(crate) unsafe fn net_write(_id: i32, _ptr: usize, _len: usize) -> i32 {
        -1
    }
    pub(crate) unsafe fn net_close(_id: i32) {}
}

/// One host call's request: the operation name, the credentials (present
/// only when the manifest declared `secrets`) and the parameters. Unknown
/// fields — the account block among them — are ignored until this provider
/// has a use for them.
#[derive(Deserialize)]
pub struct Request {
    pub method: String,
    #[serde(default)]
    pub operation: Option<String>,
    #[serde(default)]
    pub credentials: Option<Value>,
    #[serde(default)]
    pub params: Value,
}

/// What goes back: `result` on success, `error` (and nothing else) on
/// failure. The envelope is deliberately smaller than the spec's full
/// `OperationResult` — this provider has no effects to return yet, and an
/// omitted field is ignored, not an error.
#[derive(Serialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

#[derive(Serialize)]
pub struct Error {
    pub code: String,
    pub message: String,
}

impl Response {
    pub fn ok(result: Value) -> Self {
        Self {
            ok: true,
            result: Some(result),
            credentials: None,
            error: None,
        }
    }

    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            result: None,
            credentials: None,
            error: Some(Error {
                code: code.to_string(),
                message: message.into(),
            }),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| {
            b"{\"ok\":false,\"error\":{\"code\":\"protocol\",\"message\":\"the response cannot be serialised\"}}"
                .to_vec()
        })
    }
}

/// Wall-clock milliseconds, for token expiry arithmetic.
pub fn now_ms() -> i64 {
    unsafe { raw::now_ms() }
}

/// One diagnostic line for the host's per-instance log, where a sign-in the
/// user is staring at can say what it saw. Never a credential.
pub fn log(level: i32, message: &str) {
    let bytes = message.as_bytes();
    unsafe { raw::log(level, bytes.as_ptr() as usize, bytes.len()) }
}

/// Fill `bytes` from the host's CSPRNG. `false` means the host refused, which
/// is fatal for a sign-in and nothing else.
pub fn fill_random(bytes: &mut [u8]) -> bool {
    let len = bytes.len();
    unsafe { raw::random(bytes.as_mut_ptr() as usize, len) == len as i32 }
}

/// Whether the host asked this call to stop.
pub fn cancelled() -> bool {
    unsafe { raw::cancelled() != 0 }
}

/// One HTTP request through the host's `net.http` backend, which owns DNS,
/// TLS, timeouts, redirects and size caps. `-1` is every failure, denial
/// included; the reason stays in the host, so the caller's error message must
/// not pretend to know it.
///
/// What comes back is the host's *envelope* — `{status, headers, body}`, the
/// body still base64 — not the endpoint's own JSON. `decoded_body` opens it.
pub fn http_envelope(request: &Value, cap: usize) -> Result<Value, String> {
    let body = serde_json::to_vec(request).map_err(|error| format!("request: {error}"))?;
    let mut out = vec![0u8; cap];
    let written = unsafe {
        raw::http_request(
            body.as_ptr() as usize,
            body.len(),
            out.as_mut_ptr() as usize,
            cap,
        )
    };
    if written < 0 {
        return Err("the host refused the request".to_string());
    }
    let written = written as usize;
    if written > cap {
        return Err("the host wrote past the buffer".to_string());
    }
    out.truncate(written);
    serde_json::from_slice(&out).map_err(|error| format!("response: {error}"))
}

/// The response body, base64-decoded: the host's HTTP envelope carries bytes
/// as base64 whatever the endpoint answered.
pub fn decoded_body(envelope: &Value) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let encoded = envelope
        .get("body")
        .and_then(Value::as_str)
        .ok_or("the response has no body")?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("response body: {error}"))
}

/// A TLS stream the host opened: IMAP and SMTP both ride one, because a
/// plugin has no sockets of its own.
pub struct NetStream {
    id: u32,
}

impl NetStream {
    /// `implicit` is TLS from the first byte — 993 and 465, the only ports
    /// this provider speaks.
    pub fn open(host: &str, port: u16, implicit: bool) -> Result<Self, String> {
        let host = host.as_bytes();
        let id = unsafe {
            raw::net_open(
                host.as_ptr() as usize,
                host.len(),
                port as i32,
                i32::from(!implicit),
            )
        };
        if id < 0 {
            return Err("the host refused the connection".to_string());
        }
        Ok(Self { id: id as u32 })
    }

    pub fn read(&self, buffer: &mut [u8]) -> Result<usize, String> {
        let read =
            unsafe { raw::net_read(self.id as i32, buffer.as_mut_ptr() as usize, buffer.len()) };
        if read < 0 {
            return Err("the host refused the read".to_string());
        }
        Ok(read as usize)
    }

    pub fn write(&self, bytes: &[u8]) -> Result<(), String> {
        let mut written = 0;
        while written < bytes.len() {
            let count = unsafe {
                raw::net_write(
                    self.id as i32,
                    bytes[written..].as_ptr() as usize,
                    bytes.len() - written,
                )
            };
            if count < 0 {
                return Err("the host refused the write".to_string());
            }
            written += count as usize;
        }
        Ok(())
    }

    pub fn close(&self) {
        unsafe { raw::net_close(self.id as i32) }
    }
}
