//! The real host network backend.
//!
//! A provider module cannot open a socket. It asks this backend for HTTP or a
//! TLS stream, and the backend owns DNS, TCP, rustls, timeouts and byte caps.
//! The default registry uses `DeniedNetwork`; an account that has a plugin
//! provider can install `StdNetwork` instead.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::models::TlsMode;
use crate::protocol::stream::{connect, tls_wrap, MailStream};

use super::host::HostNetwork;

const MAX_HTTP_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 120_000;

/// The host capability implementation used by a plugin-backed account.
pub struct StdNetwork {
    streams: Mutex<HashMap<u32, Stream>>,
    next_stream: AtomicU32,
}

struct Stream {
    stream: MailStream,
    host: String,
}

impl Default for StdNetwork {
    fn default() -> Self {
        Self {
            streams: Mutex::new(HashMap::new()),
            next_stream: AtomicU32::new(1),
        }
    }
}

#[derive(Debug, Deserialize)]
struct HttpRequest {
    method: String,
    url: String,
    #[serde(default)]
    headers: Vec<HttpHeader>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
struct HttpHeader {
    name: String,
    value: String,
}

#[derive(Debug, Serialize)]
struct HttpResponse {
    status: u16,
    headers: Vec<HttpHeader>,
    body: String,
}

impl HostNetwork for StdNetwork {
    fn http(&self, request: &[u8]) -> Result<Vec<u8>, String> {
        let request: HttpRequest =
            serde_json::from_slice(request).map_err(|error| format!("http request: {error}"))?;
        if !(request.url.starts_with("https://") || request.url.starts_with("http://")) {
            return Err("refusing a non-http URL".to_string());
        }
        let timeout = Duration::from_millis(
            request
                .timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(1, MAX_TIMEOUT_MS),
        );
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(timeout)
            .timeout_read(timeout)
            .timeout_write(timeout)
            .build();
        let mut call = agent.request(&request.method, &request.url);
        for header in &request.headers {
            call = call.set(&header.name, &header.value);
        }
        let response = if let Some(body) = request.body {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(body)
                .map_err(|error| format!("http body: {error}"))?;
            call.send_bytes(&bytes)
        } else {
            call.call()
        };
        let response = match response {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(error) => return Err(format!("http: {error}")),
        };
        let status = response.status();
        let headers = response
            .headers_names()
            .into_iter()
            .filter_map(|name| {
                response.header(&name).map(|value| HttpHeader {
                    name,
                    value: value.to_string(),
                })
            })
            .collect();
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(MAX_HTTP_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("http body: {error}"))?;
        if bytes.len() as u64 > MAX_HTTP_BYTES {
            return Err("http response exceeds the size limit".to_string());
        }
        let response = HttpResponse {
            status,
            headers,
            body: base64::engine::general_purpose::STANDARD.encode(bytes),
        };
        serde_json::to_vec(&response).map_err(|error| format!("http response: {error}"))
    }

    fn open(&self, host: &str, port: u16, mode: u32) -> Result<u32, String> {
        let tls = match mode {
            0 => TlsMode::Implicit,
            1 => TlsMode::Plain,
            other => return Err(format!("unknown transport mode {other}")),
        };
        let stream = connect(host, port, tls).map_err(|error| error.to_string())?;
        let id = self.next_stream.fetch_add(1, Ordering::Relaxed);
        self.streams
            .lock()
            .map_err(|_| "stream table poisoned".to_string())?
            .insert(
                id,
                Stream {
                    stream,
                    host: host.to_string(),
                },
            );
        Ok(id)
    }

    fn starttls(&self, id: u32) -> Result<(), String> {
        let mut streams = self
            .streams
            .lock()
            .map_err(|_| "stream table poisoned".to_string())?;
        let entry = streams
            .get_mut(&id)
            .ok_or_else(|| "unknown stream".to_string())?;
        let host = entry.host.clone();
        let taken = std::mem::replace(&mut entry.stream, MailStream::Closed);
        entry.stream = match taken {
            MailStream::Plain(tcp) => MailStream::Tls(
                tls_wrap(tcp, &host).map_err(|error| format!("TLS upgrade: {error}"))?,
            ),
            other => other,
        };
        Ok(())
    }

    fn read(&self, id: u32, len: usize) -> Result<Vec<u8>, String> {
        let mut streams = self
            .streams
            .lock()
            .map_err(|_| "stream table poisoned".to_string())?;
        let entry = streams
            .get_mut(&id)
            .ok_or_else(|| "unknown stream".to_string())?;
        let mut bytes = vec![0; len];
        let read = entry
            .stream
            .read(&mut bytes)
            .map_err(|error| error.to_string())?;
        bytes.truncate(read);
        Ok(bytes)
    }

    fn write(&self, id: u32, bytes: &[u8]) -> Result<usize, String> {
        let mut streams = self
            .streams
            .lock()
            .map_err(|_| "stream table poisoned".to_string())?;
        let entry = streams
            .get_mut(&id)
            .ok_or_else(|| "unknown stream".to_string())?;
        entry
            .stream
            .write_all(bytes)
            .map_err(|error| error.to_string())?;
        entry.stream.flush().map_err(|error| error.to_string())?;
        Ok(bytes.len())
    }

    fn close(&self, id: u32) {
        if let Ok(mut streams) = self.streams.lock() {
            streams.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_non_http_url_before_opening_a_socket() {
        let network = StdNetwork::default();
        let error = network
            .http(br#"{"method":"GET","url":"file:///etc/passwd"}"#)
            .unwrap_err();
        assert!(error.contains("non-http"));
    }

    #[test]
    fn unknown_streams_are_errors() {
        let network = StdNetwork::default();
        assert!(network.read(99, 16).is_err());
        assert!(network.write(99, b"x").is_err());
        assert!(network.starttls(99).is_err());
    }
}
