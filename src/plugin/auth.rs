//! The browser leg of an OAuth2 sign-in, for a provider plugin that asks for
//! one.
//!
//! The plugin owns everything Google-shaped — the authorization URL, the
//! scopes, the token exchange — and the host owns everything desktop-shaped:
//! the loopback socket the browser redirects to, the browser itself, and the
//! deadline. The plugin cannot listen on a socket or spawn a process, so the
//! `auth.begin` → `auth.continue` pair of the spec is split around the browser
//! in exactly that way: the host binds the loopback first (the port has to be
//! in the redirect URI the plugin builds), hands the port to `auth.begin`,
//! opens the URL that comes back, catches the redirect, and gives the code to
//! `auth.continue`, which exchanges it for tokens through the plugin's own
//! `http_request` permission.
//!
//! Nothing here runs on the UI thread: a sign-in is one worker thread holding
//! one listener, exactly like a sync job.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{MailError, MailResult};

use super::abi::{PluginError, PluginRequest};
use super::host::{HostNetwork, PluginInstance, WasmPlugin};

/// How long the browser may take to come back with a redirect: the user has
/// to find the window, log in, and click through consent.
const SIGN_IN_WAIT: Duration = Duration::from_secs(300);
/// How long one HTTP request on the loopback socket may take to arrive whole.
const REDIRECT_READ: Duration = Duration::from_secs(5);
/// A redirect request line is small; anything past this is not a browser.
const REQUEST_CAP: usize = 8 * 1024;

/// OAuth2 tokens as the plugin returns them, ready for the caller to store
/// with the account and pass back as `credentials` on later calls. The debug
/// print is lengths on purpose: a token that reaches a log line is a token
/// that has leaked.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Milliseconds since the epoch, from the plugin (which asked `now_ms`).
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field(
                "access_token",
                &format!("{} bytes", self.access_token.len()),
            )
            .field(
                "refresh_token",
                &self
                    .refresh_token
                    .as_ref()
                    .map(|token| format!("{} bytes", token.len())),
            )
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

/// Run one authorization-code sign-in: loopback socket, the plugin's
/// `auth.begin`, the desktop's browser, the redirect, and `auth.continue`.
/// `params` carries what the account layer knows that the flow needs — the
/// provider's own config, a login hint, a client id.
pub fn sign_in(
    plugin: &WasmPlugin,
    network: Arc<dyn HostNetwork>,
    params: Value,
) -> MailResult<Credentials> {
    sign_in_with(plugin, network, params, &open_in_browser)
}

/// The same flow with the browser step supplied, so a test can drive the whole
/// dance against the real module without a desktop.
pub fn sign_in_with(
    plugin: &WasmPlugin,
    network: Arc<dyn HostNetwork>,
    mut params: Value,
    open: &dyn Fn(&str) -> MailResult<()>,
) -> MailResult<Credentials> {
    // The socket comes first: its port is part of the redirect URI the plugin
    // builds, and a URI naming a port nobody listens on is a sign-in that can
    // never come back.
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| auth_error("cannot bind the loopback socket", &error))?;
    let port = listener
        .local_addr()
        .map_err(|error| auth_error("the loopback socket has no address", &error))?
        .port();

    let mut instance = plugin
        .instantiate_with(network)
        .map_err(|error| auth_error("the plugin cannot start", &error))?;

    if !params.is_object() {
        params = json!({});
    }
    params["redirect_port"] = json!(port);
    let challenge = invoke(&mut instance, PluginRequest::new("auth.begin"), params)?;
    if challenge.get("kind").and_then(Value::as_str) != Some("open_url") {
        return Err(MailError::Protocol(
            "the plugin did not ask for a browser".to_string(),
        ));
    }
    let url = challenge
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| MailError::Protocol("the sign-in URL is missing".to_string()))?;
    check_url(url)?;

    // The page is the provider's own, opened by the desktop's own browser;
    // the loopback answer is the only part that ever touches Kimo.
    open(url)?;

    let redirect = Redirect::wait(&listener, SIGN_IN_WAIT)?;
    if let Some(error) = redirect.error {
        return Err(MailError::Auth(format!("the sign-in was refused: {error}")));
    }

    let done = invoke(
        &mut instance,
        PluginRequest::new("auth.continue"),
        json!({ "code": redirect.code, "state": redirect.state }),
    )?;
    let credentials = done
        .get("credentials")
        .cloned()
        .ok_or_else(|| MailError::Protocol("the plugin returned no tokens".to_string()))?;
    serde_json::from_value(credentials)
        .map_err(|error| MailError::Protocol(format!("the tokens are malformed: {error}")))
}

/// Refresh through the plugin's `auth.refresh`: the same narrow operation the
/// spec gives a provider for exactly this, so the client id and the grant stay
/// inside the plugin.
pub fn refresh(
    plugin: &WasmPlugin,
    network: Arc<dyn HostNetwork>,
    refresh_token: &str,
    client_id: &str,
) -> MailResult<Credentials> {
    let mut instance = plugin
        .instantiate_with(network)
        .map_err(|error| auth_error("the plugin cannot start", &error))?;
    let done = invoke(
        &mut instance,
        PluginRequest::new("auth.refresh"),
        json!({ "refresh_token": refresh_token, "client_id": client_id }),
    )?;
    let credentials = done
        .get("credentials")
        .cloned()
        .ok_or_else(|| MailError::Protocol("the plugin returned no tokens".to_string()))?;
    serde_json::from_value(credentials)
        .map_err(|error| MailError::Protocol(format!("the tokens are malformed: {error}")))
}

/// One plugin call: `ok` hands back the result, an error keeps its code and
/// message (a token never reaches either, by the plugin's own contract).
fn invoke(
    instance: &mut PluginInstance,
    mut request: PluginRequest,
    params: Value,
) -> MailResult<Value> {
    request.params = params;
    let response = instance
        .invoke(&request)
        .map_err(|error| auth_error("the plugin call failed", &error))?;
    if !response.ok {
        let error = response
            .error
            .unwrap_or_else(|| PluginError::new("failed", "the plugin gave no reason"));
        // `auth_failed` is the one code that is the user's to read plainly;
        // everything else keeps its code so the caller can tell a network
        // problem from a refused grant.
        return Err(if error.code == "auth_failed" {
            MailError::Auth(error.message)
        } else {
            MailError::Protocol(format!("{}: {}", error.code, error.message))
        });
    }
    Ok(response.result)
}

fn auth_error(context: &str, error: &impl std::fmt::Display) -> MailError {
    MailError::Protocol(format!("{context}: {error}"))
}

/// A URL Kimo will hand to the desktop's browser. Only `http` and `https`
/// reach it — the plugin chose the page, but `file:` or `javascript:` in a
/// browser the user trusts is not the plugin's call to make.
fn check_url(url: &str) -> MailResult<()> {
    let Some((scheme, _)) = url.split_once("://") else {
        return Err(bad_url(url));
    };
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        Ok(())
    } else {
        Err(bad_url(url))
    }
}

fn bad_url(url: &str) -> MailError {
    MailError::Protocol(format!("the sign-in URL is not a web page: {url}"))
}

/// Open the sign-in page: the desktop's OpenURI portal first — the same
/// conversation the file dialog uses — and `xdg-open` when there is no portal
/// to ask, which is a session without xdg-desktop-portal rather than a
/// desktop without the page.
fn open_in_browser(url: &str) -> MailResult<()> {
    if crate::portal::open_uri(url).is_ok() {
        return Ok(());
    }
    let opened = std::process::Command::new("xdg-open")
        .arg(url)
        .status()
        .map_err(|error| MailError::Protocol(format!("cannot open a browser: {error}")))?;
    if opened.success() {
        Ok(())
    } else {
        Err(MailError::Protocol(
            "the browser could not be started".to_string(),
        ))
    }
}

/// The answer a browser brings back to the loopback socket: the code and
/// state of a grant, or the error of a refusal.
struct Redirect {
    #[allow(dead_code)] // The code is consumed by the caller; kept for clarity.
    code: String,
    state: String,
    error: Option<String>,
}

impl Redirect {
    /// Wait for one browser to connect, for at most `deadline`. The listener
    /// is non-blocking and polled rather than parked in `accept`, so the wait
    /// stays bounded by our clock rather than the socket's.
    fn wait(listener: &TcpListener, deadline: Duration) -> MailResult<Redirect> {
        listener
            .set_nonblocking(true)
            .map_err(|error| auth_error("loopback socket", &error))?;
        let started = Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, _)) => return Redirect::read(stream),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() > deadline {
                        return Err(MailError::Auth(
                            "the sign-in timed out waiting for the browser".to_string(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(auth_error("loopback socket", &error)),
            }
        }
    }

    /// Read one `GET /?code=…&state=… HTTP/1.1`, answer it, and keep its
    /// query. The answer is a page that says the tab can be closed: the
    /// browser shows it, and it is the last thing the flow asks of the user.
    fn read(mut stream: TcpStream) -> MailResult<Redirect> {
        stream
            .set_read_timeout(Some(REDIRECT_READ))
            .map_err(|error| auth_error("loopback socket", &error))?;
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream
                .read(&mut byte)
                .map_err(|error| auth_error("the browser hung up", &error))?;
            request.push(byte[0]);
            if request.len() > REQUEST_CAP {
                return Err(MailError::Protocol(
                    "the redirect request is not a browser's".to_string(),
                ));
            }
        }
        let head = String::from_utf8_lossy(&request);
        let path = head
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| MailError::Protocol("the redirect request has no path".to_string()))?;
        let query = path.split_once('?').map(|(_, query)| query).unwrap_or("");
        let mut redirect = Redirect {
            code: String::new(),
            state: String::new(),
            error: None,
        };
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key {
                "code" => redirect.code = percent_decoded(value),
                "state" => redirect.state = percent_decoded(value),
                "error" => {
                    redirect.error = Some(percent_decoded(value));
                }
                "error_description" => {
                    let description = percent_decoded(value);
                    redirect.error = Some(match redirect.error.take() {
                        Some(name) => format!("{name}: {description}"),
                        None => description,
                    });
                }
                _ => {}
            }
        }
        // The browser gets its answer whatever the query said: a refusal also
        // ends with a page, not a dropped connection.
        let page = "<!doctype html><meta charset=utf-8><body style=\"font-family:sans-serif;\
                    text-align:center;padding:3em\">Kimo sign-in complete — this tab can be closed.";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
        if let Some(error) = &redirect.error {
            return Err(MailError::Auth(format!("the sign-in was refused: {error}")));
        }
        if redirect.code.is_empty() {
            return Err(MailError::Protocol(
                "the redirect carried no code".to_string(),
            ));
        }
        Ok(redirect)
    }
}

/// Undo the percent-encoding of a query value: `%2F` is a slash, `+` a space,
/// and anything that is not an escape is itself.
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = bytes.get(index + 1..index + 3).unwrap_or_default();
                let high = hex.first().and_then(|b| (*b as char).to_digit(16));
                let low = hex.get(1).and_then(|b| (*b as char).to_digit(16));
                if let (Some(high), Some(low)) = (high, low) {
                    out.push((high * 16 + low) as u8);
                    index += 3;
                } else {
                    out.push(b'%');
                    index += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::host::DeniedNetwork;
    use std::sync::mpsc;

    /// The real Gmail module, loaded the way discovery loads it.
    fn gmail() -> WasmPlugin {
        let manifest =
            crate::plugin::abi::Manifest::parse(include_str!("../../plugins/gmail/plugin.json"))
                .unwrap();
        WasmPlugin::load(manifest, include_bytes!("../../plugins/gmail/gmail.wasm")).unwrap()
    }

    /// Play the browser against `listener`: ask for `path` and answer with
    /// whatever page came back.
    fn browser_play(listener: &TcpListener, path: &str) -> String {
        let port = listener.local_addr().unwrap().port();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .unwrap();
        let mut page = String::new();
        stream.read_to_string(&mut page).unwrap();
        page
    }

    #[test]
    fn the_loopback_answer_carries_the_code_and_the_state() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let waiter = listener
            .try_clone()
            .expect("a listener clones for the waiting thread");
        let (done, got) = mpsc::channel();
        std::thread::spawn(move || {
            let redirect = Redirect::wait(&waiter, Duration::from_secs(5)).unwrap();
            done.send((redirect.code, redirect.state)).unwrap();
        });
        let page = browser_play(&listener, "/cb?code=4%2F0Ax%3Dabc&state=s1");
        let (code, state) = got.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(code, "4/0Ax=abc");
        assert_eq!(state, "s1");
        assert!(page.starts_with("HTTP/1.1 200 OK"));
        assert!(page.contains("this tab can be closed"));
    }

    #[test]
    fn a_refused_consent_is_an_error_not_a_code() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let waiter = listener.try_clone().unwrap();
        std::thread::spawn(move || {
            let error = Redirect::wait(&waiter, Duration::from_secs(5))
                .map(|_| ())
                .expect_err("a refused consent is an error");
            assert!(error.to_string().contains("access_denied"));
        });
        let _ = browser_play(
            &listener,
            "/cb?error=access_denied&error_description=Consent+denied",
        );
    }

    #[test]
    fn the_whole_sign_in_reaches_the_token_exchange() {
        // The real module, a fake browser that plays the redirect, and no
        // HTTP backend: the flow has to get all the way to the token
        // exchange before the denied backend stops it, which is exactly
        // where the host's part of the dance ends and the plugin's own
        // `net.http` leg begins.
        let plugin = gmail();
        let network = Arc::new(DeniedNetwork);

        // The URL the plugin built is the only channel the fake browser
        // needs: it names the loopback port and carries the state to echo.
        let handed = Arc::new(std::sync::Mutex::new(None::<String>));
        let slot = Arc::clone(&handed);
        let open = move |url: &str| -> MailResult<()> {
            *slot.lock().unwrap() = Some(url.to_string());
            Ok(())
        };

        let flow = std::thread::spawn(move || {
            sign_in_with(
                &plugin,
                network,
                json!({ "client_id": "test-client.apps.googleusercontent.com" }),
                &open,
            )
        });

        let url = loop {
            if let Some(url) = handed.lock().unwrap().take() {
                break url;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(url.starts_with("https://accounts.google.com/"), "{url}");
        let query = |key: &str| -> String {
            url.split(&format!("{key}="))
                .nth(1)
                .and_then(|rest| rest.split('&').next())
                .map(percent_decoded)
                .unwrap_or_default()
        };
        let redirect_uri = query("redirect_uri");
        let port: u16 = redirect_uri
            .rsplit(':')
            .next()
            .and_then(|digits| digits.trim_end_matches('/').parse().ok())
            .expect("the redirect URI names the loopback port");
        let state = query("state");

        let mut browser = TcpStream::connect(("127.0.0.1", port)).unwrap();
        browser
            .write_all(
                format!("GET /?code=4%2F0Ax&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        let mut page = String::new();
        let _ = browser.read_to_string(&mut page);

        let error = flow
            .join()
            .unwrap()
            .expect_err("the denied backend must stop the token exchange");
        // The plugin cannot see *why* its HTTP import was refused — the
        // reason stays in the host — so the message is its own; what matters
        // is that it is the token exchange saying it, not an earlier leg.
        assert!(
            error.to_string().contains("token endpoint"),
            "the flow stopped somewhere else: {error}"
        );
    }

    #[test]
    fn percent_decoding_unescapes_a_query() {
        assert_eq!(percent_decoded("a%2Fb+c%3Dd"), "a/b c=d");
        assert_eq!(percent_decoded("%E4%B8%AD"), "中");
        assert_eq!(percent_decoded("100%"), "100%");
        assert_eq!(percent_decoded("%zz"), "%zz");
    }

    #[test]
    fn urls_that_are_not_pages_are_refused() {
        assert!(check_url("https://accounts.google.com/x").is_ok());
        assert!(check_url("http://localhost:1/cb").is_ok());
        assert!(check_url("file:///etc/passwd").is_err());
        assert!(check_url("javascript:alert(1)").is_err());
        assert!(check_url("accounts.google.com").is_err());
    }
}

#[cfg(test)]
mod live_probe {
    use super::*;
    use crate::plugin::discovery::{self, Registry};

    /// Everything `plugin_sign_in` does, step by step, against the real
    /// module and the live desktop — including the browser open. Run with
    /// `--ignored --nocapture`; a Microsoft login page should appear.
    #[test]
    #[ignore]
    fn probe_the_full_sign_in_leg() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let registry = std::sync::Arc::new(Registry::discover_from(&[root.join("plugins")]));
        discovery::install(std::sync::Arc::clone(&registry));
        eprintln!("1. registry: {} plugins", registry.plugins().len());
        let loaded = registry.provider("outlook").expect("outlook loaded");
        eprintln!("2. plugin resolved: {}", loaded.manifest.id);

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("the loopback socket binds");
        let port = listener.local_addr().unwrap().port();
        eprintln!("3. loopback port {port}");

        let mut instance = loaded
            .plugin
            .instantiate_with(Arc::new(crate::plugin::host::DeniedNetwork))
            .expect("the module instantiates");
        let challenge = invoke(
            &mut instance,
            PluginRequest::new("auth.begin"),
            json!({ "redirect_port": port, "client_id": "probe-client-id" }),
        )
        .expect("auth.begin answers");
        eprintln!("4. auth.begin kind: {:?}", challenge.get("kind"));
        let url = challenge.get("url").and_then(Value::as_str).expect("a url");
        check_url(url).expect("the url is a page");
        eprintln!("5. url ok: {url}");

        open_in_browser(url).expect("the browser opens");
        eprintln!("6. browser leg returned Ok — page should be open now");
    }
}
