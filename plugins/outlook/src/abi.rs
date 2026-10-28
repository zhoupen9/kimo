//! The Kimo host imports and the JSON envelope, as `docs/mail-spec.md` §8
//! declares them — trimmed to what a Graph provider needs: no `net_*`
//! streams at all, because the manifest this module ships declares no
//! `net.tls` and the host would refuse the imports.
//!
//! The imports are declared for `wasm32` and stubbed for every other
//! target, with the same paths, so the whole plugin builds and its parsing
//! half runs under plain `cargo test` on the host.

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
        pub(crate) fn random(ptr: usize, len: usize) -> i32;
        pub(crate) fn http_request(
            request_ptr: usize,
            request_len: usize,
            out_ptr: usize,
            out_cap: usize,
        ) -> i32;
    }
}

// The host-side stand-ins for the imports: a fixed clock, a deterministic
// byte pattern instead of the CSPRNG, and a refusal where the sandbox would
// reach the network. Tests that need real randomness or HTTPS do not run on
// the host; the point of these is that the code above them does.
#[cfg(not(target_arch = "wasm32"))]
mod raw {
    pub(crate) unsafe fn log(_level: i32, _ptr: usize, _len: usize) {}
    pub(crate) unsafe fn now_ms() -> i64 {
        1_700_000_000_000
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
}

/// One host call's request: the operation name, the credentials (present
/// only when the manifest declared `secrets`) and the parameters. Unknown
/// fields are ignored until this provider has a use for them.
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
/// failure.
#[derive(Serialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials: Option<Value>,
    // Boxed so the envelope stays small: `Result`s carry it as their Err
    // arm, and the padding of two more fields had grown it past a pointer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Box<Error>>,
}

#[derive(Serialize)]
pub struct Error {
    pub code: String,
    pub message: String,
    /// The spec's `retryable`/`retry_after_ms`, present only when the
    /// provider knows them (a throttled Graph request does).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
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
            error: Some(Box::new(Error {
                code: code.to_string(),
                message: message.into(),
                retryable: None,
                retry_after_ms: None,
            })),
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

/// Fill `bytes` from the host's CSPRNG. `false` means the host refused,
/// which is fatal for a sign-in and nothing else.
pub fn fill_random(bytes: &mut [u8]) -> bool {
    let len = bytes.len();
    unsafe { raw::random(bytes.as_mut_ptr() as usize, len) == len as i32 }
}

/// One diagnostic line for the host's per-instance log. Never a credential.
pub fn log(level: i32, message: &str) {
    let bytes = message.as_bytes();
    unsafe { raw::log(level, bytes.as_ptr() as usize, bytes.len()) }
}

/// One HTTP call, with the buffer the caller names: the answer is the
/// envelope the host's `net.http` backend builds (`status`, `headers`,
/// base64 `body`), as bytes. `-1` from the import is every refusal, denial
/// included; the reason stays in the host, so the caller's message must not
/// pretend to know it.
pub fn http_envelope_bytes(request: &Value, cap: usize) -> Result<Vec<u8>, String> {
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
    Ok(out)
}

/// One HTTP call's envelope, opened into JSON.
pub fn http_envelope(request: &Value, cap: usize) -> Result<Value, String> {
    let bytes = http_envelope_bytes(request, cap)?;
    serde_json::from_slice(&bytes).map_err(|error| format!("response: {error}"))
}

/// The response body, base64-decoded: the host's HTTP envelope carries
/// bytes as base64 whatever the endpoint answered.
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
