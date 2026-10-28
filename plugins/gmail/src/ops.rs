//! The operation catalogue: what `provider.invoke` (and the `auth.*` pair)
//! answer, per `docs/mail-spec.md` §5–§6.4.
//!
//! Every network operation opens its own IMAP session, authenticates, does
//! one thing, and logs out — the same shape the host's own adapters have,
//! because the host schedules one operation at a time and keeps no session
//! between invokes. When the server does not advertise `X-GM-EXT-1`, the
//! fetches quietly stop asking for labels and ids, labels operations become
//! `unsupported`, and delete degrades to flags-plus-expunge: that is the
//! `kimo-imap/1` subset the Gmail profile promises to fall back to.

use std::ptr::addr_of_mut;

use serde_json::{json, Value};

use crate::abi::{self, Request, Response};
use crate::imap::{self, Failed, Fetched};
use crate::oauth;
use crate::smtp;

/// The newest mail a folder's first fetch takes, one window of the same size
/// the host's own adapters use.
const BACKFILL_LIMIT: u32 = 50;
/// How much body one `mailbox.sync` call may read. It is a fuel bound, not a
/// protocol one: the interpreter has 500 M instructions per invoke, and a
/// twenty-megabyte backfill spent all of them base64ing mail into one JSON
/// answer. Four megabytes leaves room for the copies and encoding around the
/// bytes, and the host asks again from the new mark. A first mail larger than
/// this is read whole anyway — the cap never starves a folder.
const SYNC_BUDGET_BYTES: usize = 4 * 1024 * 1024;
/// A Gmail search expression longer than this is not a search anyone typed.
const RAW_QUERY_CAP: usize = 1024;
/// Token responses are small; this is the HTTP buffer the plugin offers.
const HTTP_CAP: usize = 64 * 1024;

/// The deployment's Google OAuth client id, which Google issues to whoever
/// ships this build — not to the software. Empty means "bring your own",
/// and `auth.begin` says so rather than inventing one.
const DEFAULT_CLIENT_ID: &str = "";

/// The sign-in `auth.begin` started and `auth.continue` must finish. One at
/// a time: the module is single-instance and single-threaded.
static mut PENDING: Option<oauth::Pending> = None;

fn set_pending(value: Option<oauth::Pending>) {
    unsafe {
        *addr_of_mut!(PENDING) = value;
    }
}

/// Take the sign-in in progress, if any: `auth.continue` consumes it, win or
/// lose, so a finished or failed code can never be replayed.
fn take_pending() -> Option<oauth::Pending> {
    unsafe { std::mem::take(&mut *addr_of_mut!(PENDING)) }
}

pub fn dispatch(request: &[u8]) -> Vec<u8> {
    let parsed: Request = match serde_json::from_slice(request) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Response::error("protocol", format!("malformed request: {error}")).to_bytes()
        }
    };
    answer(&parsed).to_bytes()
}

fn answer(request: &Request) -> Response {
    match request.method.as_str() {
        "plugin.describe" | "provider.describe" => describe(),
        "provider.health" => health(request),
        "provider.invoke" => match request.operation.as_deref() {
            Some(operation) => invoke(request, operation),
            None => Response::error("protocol", "provider.invoke without an operation"),
        },
        "auth.begin" => auth_begin(request),
        "auth.continue" => auth_continue(request),
        "auth.cancel" => {
            set_pending(None);
            Response::ok(json!({ "kind": "done" }))
        }
        "auth.refresh" => auth_refresh(request),
        "provider.logout" => Response::ok(json!({})),
        other => Response::error(
            "unsupported",
            format!("{other} is not an operation this provider has"),
        ),
    }
}

/// The static descriptor: the Gmail profile in full, with the network rules
/// the host turns into an endpoint allowlist.
fn describe() -> Response {
    Response::ok(json!({
        "id": "gmail",
        "name": "Gmail",
        "version": env!("CARGO_PKG_VERSION"),
        "spec_version": "1.0",
        "profiles": ["kimo-core/1", "kimo-imap/1", "kimo-gmail/1"],
        "capabilities": [
            { "name": "mailbox.list" },
            { "name": "mailbox.sync" },
            { "name": "message.list" },
            { "name": "message.fetch" },
            { "name": "message.fetch.raw" },
            { "name": "message.set_flags" },
            { "name": "message.set_labels" },
            { "name": "message.delete" },
            { "name": "message.append" },
            { "name": "message.search" },
            { "name": "query.raw", "params": { "dialect": "gmail" } },
            { "name": "thread.native" },
            { "name": "send.submit" },
            { "name": "send.save_copy" },
            { "name": "auth.oauth2.auth_code_pkce" },
            { "name": "auth.xoauth2" },
            { "name": "auth.password" }
        ],
        "auth": [
            { "kind": "oauth2", "flow": "authorization_code_pkce", "scope": oauth::SCOPE },
            { "kind": "password" }
        ],
        "config": [
            { "id": "email", "label": "Email address", "type": "text", "required": true },
            { "id": "client_id", "label": "Google OAuth client id (from the deployment's Google Cloud project)", "type": "text", "required": false }
        ],
        "network": [
            { "transport": "https", "host": "accounts.google.com" },
            { "transport": "https", "host": "oauth2.googleapis.com" },
            { "transport": "tls", "host": imap::HOST, "port": imap::PORT },
            { "transport": "tls", "host": smtp::HOST, "port": smtp::PORT }
        ]
    }))
}

/// Connect and authenticate, and say what the *server* actually offers: the
/// effective capability set is the declared one minus everything that needs
/// `X-GM-EXT-1` when the server did not advertise it.
fn health(request: &Request) -> Response {
    let session = match session(request) {
        Ok(session) => session,
        Err(response) => return response,
    };
    let mut client = match imap_session(&session) {
        Ok(client) => client,
        Err(response) => return response,
    };
    client.logout();
    if client.has(imap::GM_EXT) {
        Response::ok(
            json!({ "ok": true, "gm_ext": true, "profiles": ["kimo-core/1", "kimo-imap/1", "kimo-gmail/1"] }),
        )
    } else {
        abi::log(
            1,
            "the server did not offer X-GM-EXT-1: labels, native threads and Gmail search are off",
        );
        Response::ok(json!({
            "ok": true,
            "gm_ext": false,
            "profiles": ["kimo-core/1", "kimo-imap/1"],
            "warnings": [{ "code": "degraded", "message": "the server did not offer X-GM-EXT-1: labels, native threads and Gmail search are off" }]
        }))
    }
}

fn invoke(request: &Request, operation: &str) -> Response {
    // The descriptor is static: no credentials, no socket. It has to answer
    // here, before the session, because the host asks for it on its own
    // (capability gates, the account form) with nothing to log in with.
    if operation == "provider.describe" {
        return describe();
    }
    let session = match session(request) {
        Ok(session) => session,
        Err(response) => return response,
    };
    let params = &request.params;
    // Every arm is Result<Value, Response>: open a session, do one thing,
    // log out, and let the one match at the end shape the envelope.
    let result: Result<Value, Response> = match operation {
        "mailbox.list" => {
            let mut client = match imap_session(&session) {
                Ok(client) => client,
                Err(response) => return response,
            };
            let folders = client.list().map_err(failed).map(|folders| json!(folders));
            client.logout();
            folders
        }
        "mailbox.sync" => {
            let folder = match text(params, "folder") {
                Some(folder) => folder,
                None => return Response::error("protocol", "mailbox.sync without a folder"),
            };
            let mut client = match imap_session(&session) {
                Ok(client) => client,
                Err(response) => return response,
            };
            let result = sync_folder(&mut client, folder, mark_of(params));
            client.logout();
            result
        }
        "mailbox.sync.older" => {
            let folder = match text(params, "folder") {
                Some(folder) => folder,
                None => return Response::error("protocol", "mailbox.sync.older without a folder"),
            };
            let batch = params
                .get("batch")
                .and_then(Value::as_u64)
                .and_then(|batch| u32::try_from(batch).ok())
                .unwrap_or(BACKFILL_LIMIT);
            let mut client = match imap_session(&session) {
                Ok(client) => client,
                Err(response) => return response,
            };
            let result = older_folder(&mut client, folder, mark_of(params), batch);
            client.logout();
            result
        }
        "message.fetch" => {
            let (folder, uid) = match (text(params, "folder"), text(params, "uid")) {
                (Some(folder), Some(uid)) => (folder, uid),
                _ => return Response::error("protocol", "message.fetch needs a folder and a uid"),
            };
            let mut client = match imap_session(&session) {
                Ok(client) => client,
                Err(response) => return response,
            };
            let result = client
                .select(folder)
                .map_err(failed)
                .and_then(|_| client.fetch_full(uid, true).map_err(failed))
                .and_then(|fetched| {
                    fetched
                        .first()
                        .map(|message| json!({ "raw": oauth::base64_std(&message.raw) }))
                        .ok_or_else(|| {
                            Response::error("protocol", "the message is not in the folder")
                        })
                });
            client.logout();
            result
        }
        "message.set_flags" => {
            let (folder, uid) = match (text(params, "folder"), text(params, "uid")) {
                (Some(folder), Some(uid)) => (folder, uid),
                _ => {
                    return Response::error(
                        "protocol",
                        "message.set_flags needs a folder and a uid",
                    )
                }
            };
            let uid: u32 = match uid.parse() {
                Ok(uid) => uid,
                Err(_) => return Response::error("protocol", "the uid is not a number"),
            };
            let mut client = match imap_session(&session) {
                Ok(client) => client,
                Err(response) => return response,
            };
            let mut result: Result<(), Response> =
                client.select(folder).map_err(failed).map(|_| ());
            if let Some(seen) = params.get("seen").and_then(Value::as_bool) {
                result = result.and(client.uid_store_flags(uid, seen, "\\Seen").map_err(failed));
            }
            if let Some(flagged) = params.get("flagged").and_then(Value::as_bool) {
                result = result.and(
                    client
                        .uid_store_flags(uid, flagged, "\\Flagged")
                        .map_err(failed),
                );
            }
            client.logout();
            result.map(|()| json!({}))
        }
        "message.set_labels" => {
            let folder = match text(params, "folder") {
                Some(folder) => folder,
                None => return Response::error("protocol", "message.set_labels without a folder"),
            };
            let uids = uids_of(params);
            if uids.is_empty() {
                return Response::error("protocol", "message.set_labels without messages");
            }
            let add = labels_of(params, "add");
            let remove = labels_of(params, "remove");
            if add.is_empty() && remove.is_empty() {
                return Response::error("protocol", "message.set_labels changes nothing");
            }
            let mut client = match imap_session(&session) {
                Ok(client) => client,
                Err(response) => return response,
            };
            if !client.has(imap::GM_EXT) {
                client.logout();
                return Response::error(
                    "unsupported",
                    "this server has no X-GM-LABELS — the labels are local",
                );
            }
            let mut result: Result<(), Response> =
                client.select(folder).map_err(failed).map(|_| ());
            for uid in uids {
                if !add.is_empty() {
                    result = result.and(client.uid_store_labels(uid, true, &add).map_err(failed));
                }
                if !remove.is_empty() {
                    result =
                        result.and(client.uid_store_labels(uid, false, &remove).map_err(failed));
                }
            }
            client.logout();
            result.map(|()| json!({}))
        }
        "message.delete" => {
            let folder = match text(params, "folder") {
                Some(folder) => folder,
                None => return Response::error("protocol", "message.delete without a folder"),
            };
            let uids = uids_of(params);
            if uids.is_empty() {
                return Response::error("protocol", "message.delete without messages");
            }
            let mut client = match imap_session(&session) {
                Ok(client) => client,
                Err(response) => return response,
            };
            let mut result: Result<(), Response> =
                client.select(folder).map_err(failed).map(|_| ());
            let trash = ["\\Trash".to_string()];
            let inbox = ["\\Inbox".to_string()];
            for uid in &uids {
                if client.has(imap::GM_EXT) {
                    // Gmail's soft delete: into \Trash, out of \Inbox. The
                    // message stays under every other label it carries.
                    result =
                        result.and(client.uid_store_labels(*uid, true, &trash).map_err(failed));
                    result =
                        result.and(client.uid_store_labels(*uid, false, &inbox).map_err(failed));
                } else {
                    // The degrade path: a plain IMAP flags delete.
                    result = result.and(
                        client
                            .uid_store_flags(*uid, true, "\\Deleted")
                            .map_err(failed),
                    );
                }
            }
            client.logout();
            let gone: Vec<String> = uids.iter().map(|uid| uid.to_string()).collect();
            result.map(|()| json!({ "gone": gone }))
        }
        "message.append" => {
            let (mailbox, raw) = match (
                text(params, "mailbox"),
                params.get("raw").and_then(Value::as_str),
            ) {
                (Some(mailbox), Some(raw)) => (mailbox, raw),
                _ => {
                    return Response::error(
                        "protocol",
                        "message.append needs a mailbox and a message",
                    )
                }
            };
            use base64::Engine as _;
            let raw = base64::engine::general_purpose::STANDARD
                .decode(raw)
                .unwrap_or_default();
            let mut client = match imap_session(&session) {
                Ok(client) => client,
                Err(response) => return response,
            };
            let result = client
                .append(mailbox, &raw)
                .map_err(failed)
                .map(|()| json!({}));
            client.logout();
            result
        }
        "message.search" => {
            let expression = match text(params, "expression").or_else(|| text(params, "query")) {
                Some(expression) => expression,
                None => return Response::error("protocol", "message.search without an expression"),
            };
            if let Some(dialect) = text(params, "dialect") {
                if dialect != "gmail" {
                    return Response::error(
                        "unsupported",
                        format!("the query dialect {dialect} is not Gmail's"),
                    );
                }
            }
            // The expression becomes an IMAP quoted string; CR/LF would
            // smuggle a second command, and a screenful is not a search.
            if expression.contains('\r')
                || expression.contains('\n')
                || expression.len() > RAW_QUERY_CAP
            {
                return Response::error(
                    "protocol",
                    "the search expression is not one a person typed",
                );
            }
            let mut client = match imap_session(&session) {
                Ok(client) => client,
                Err(response) => return response,
            };
            if !client.has(imap::GM_EXT) {
                client.logout();
                return Response::error("unsupported", "this server cannot run a Gmail search");
            }
            let result = client
                .uid_search_raw(expression)
                .map_err(failed)
                .map(|uids| json!({ "uids": uids }));
            client.logout();
            result
        }
        "send.submit" => {
            let raw = match params.get("rfc822").and_then(Value::as_str) {
                Some(raw) => raw,
                None => return Response::error("protocol", "send.submit without a message"),
            };
            use base64::Engine as _;
            let raw = base64::engine::general_purpose::STANDARD
                .decode(raw)
                .unwrap_or_default();
            let envelope = params.get("envelope").cloned().unwrap_or_else(|| json!({}));
            let from = envelope
                .get("from")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let recipients: Vec<String> = ["to", "cc", "bcc"]
                .iter()
                .flat_map(|field| {
                    envelope
                        .get(*field)
                        .and_then(Value::as_array)
                        .map(|list| {
                            list.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .collect();
            if from.is_empty() || recipients.is_empty() {
                return Response::error("protocol", "send.submit needs a sender and a recipient");
            }
            let (user, token) = match &session {
                Session::Token { user, token } => (user.as_str(), token.as_str()),
                // An app password is an IMAP LOGIN, not a submission token;
                // pretending otherwise would send mail with the wrong legs.
                Session::Password { .. } => {
                    return Response::error(
                        "unsupported",
                        "Gmail submission needs the OAuth token, not the password",
                    )
                }
            };
            match smtp::submit(user, token, &from, &recipients, &raw) {
                Ok(()) => Ok(json!({ "status": "accepted" })),
                Err(rejected) => {
                    return if rejected.ambiguous {
                        Response::error("ambiguous", rejected.message)
                    } else {
                        Response::error("protocol", rejected.message)
                    }
                }
            }
        }
        other => Err(Response::error(
            "unsupported",
            format!("{other} is not an operation this provider has"),
        )),
    };
    match result {
        Ok(value) => Response::ok(value),
        Err(response) => response,
    }
}

// ----- the two sync operations ----------------------------------------------

/// One folder's fetch, the same shape the host's IMAP adapter runs: the
/// server's oldest UID, then either everything from the resume mark or the
/// newest backfill window.
fn sync_folder(client: &mut imap::Client, folder: &str, mark: &Value) -> Result<Value, Response> {
    let info = client.select(folder).map_err(failed)?;
    let resume = resume_uid(mark, info.uidvalidity);
    let oldest = client.oldest_uid(info.exists).map_err(failed)?;

    let mut floor = None;
    let mut more = false;
    let fetched = if info.exists > 0 {
        match resume {
            Some(next) => {
                let (fetched, unfinished) = client
                    .fetch_bounded(&format!("{next}:*"), true, SYNC_BUDGET_BYTES)
                    .map_err(failed)?;
                more = unfinished;
                fetched
                    .into_iter()
                    .filter(|message| message.uid >= next)
                    .collect::<Vec<Fetched>>()
            }
            None => {
                let start = info.exists.saturating_sub(BACKFILL_LIMIT) + 1;
                // From the window's lowest sequence upward: a call that stops
                // at the byte budget leaves the newer half of the window to
                // the resume mark, which walks up it on the next call. The
                // other direction would leave a hole no mark covers.
                let (fetched, unfinished) = client
                    .fetch_bounded(&format!("{start}:*"), false, SYNC_BUDGET_BYTES)
                    .map_err(failed)?;
                more = unfinished;
                floor = fetched.iter().map(|message| message.uid).min();
                fetched
            }
        }
    } else {
        Vec::new()
    };

    Ok(json!({
        "uidvalidity": info.uidvalidity,
        "kind": if resume.is_some() { "resume" } else { "fill" },
        "first_num": floor,
        "oldest": oldest,
        "marks_before_store": false,
        // The range was not finished: the host calls again with the mark this
        // batch just moved, and the rest comes in another bounded call.
        "more": more,
        "messages": fetched.iter().map(message_json).collect::<Vec<_>>(),
    }))
}

/// The batch below the floor the folder has been fetched down to; the floor
/// moves even when the range held nothing.
fn older_folder(
    client: &mut imap::Client,
    folder: &str,
    mark: &Value,
    batch: u32,
) -> Result<Value, Response> {
    let info = client.select(folder).map_err(failed)?;
    let oldest = client.oldest_uid(info.exists).map_err(failed)?;
    // Stored UIDs address different mail under another UIDVALIDITY: a batch
    // fetched now would replace them with a slice out of the middle.
    let same_generation = matches!(
        (
            mark.get("uidvalidity").and_then(Value::as_u64),
            info.uidvalidity.map(u64::from)
        ),
        (Some(stored), Some(selected)) if stored == selected
    );
    if !same_generation {
        return Err(Response::error(
            "protocol",
            "the folder changed on the server — sync it before loading older mail",
        ));
    }

    let mut first_num = None;
    let mut fetched = Vec::new();
    let mut available = false;
    if let (Some(floor), Some(oldest)) = (
        mark.get("first_num")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
        oldest,
    ) {
        if let Some((low, high)) = older_uids(floor, oldest, batch) {
            fetched = client
                .fetch_full(&format!("{low}:{high}"), true)
                .map_err(failed)?;
            first_num = Some(low);
            available = low > oldest;
        }
    }

    Ok(json!({
        "uidvalidity": info.uidvalidity,
        "first_num": first_num,
        "oldest": oldest,
        "available": available,
        "messages": fetched.iter().map(message_json).collect::<Vec<_>>(),
    }))
}

fn older_uids(floor: u32, oldest: u32, batch: u32) -> Option<(u32, u32)> {
    if floor <= 1 || oldest >= floor {
        return None;
    }
    Some((oldest.max(floor.saturating_sub(batch)), floor - 1))
}

/// The UID a warm fetch resumes from: both sides have to agree on the
/// UIDVALIDITY generation, and the mark has to have one to resume at.
fn resume_uid(mark: &Value, selected: Option<u32>) -> Option<u32> {
    match (
        mark.get("uidvalidity").and_then(Value::as_u64),
        selected.map(u64::from),
    ) {
        (Some(stored), Some(selected)) if stored == selected => mark
            .get("uid_next")
            .and_then(Value::as_u64)
            .and_then(|uid| u32::try_from(uid).ok())
            .filter(|uid| *uid > 0),
        _ => None,
    }
}

fn message_json(message: &Fetched) -> Value {
    let seen = message
        .flags
        .iter()
        .any(|flag| flag.eq_ignore_ascii_case("\\Seen"));
    json!({
        "uid": message.uid.to_string(),
        "seen": seen,
        "raw": oauth::base64_std(&message.raw),
        "labels": message.labels,
        "thread": message.thrid,
        "gmid": message.gmid,
    })
}

// ----- authentication --------------------------------------------------------

enum Session {
    Password { user: String, password: String },
    Token { user: String, token: String },
}

fn session(request: &Request) -> Result<Session, Response> {
    let credentials = request.credentials.as_ref().ok_or_else(|| {
        Response::error(
            "auth_failed",
            "the account has no credentials to open Gmail with",
        )
    })?;
    let user = credentials
        .get("username")
        .and_then(Value::as_str)
        .filter(|user| !user.is_empty())
        .ok_or_else(|| Response::error("auth_failed", "the account has no address"))?
        .to_string();
    match credentials.get("kind").and_then(Value::as_str) {
        Some("password") => {
            let password = credentials
                .get("password")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(Session::Password { user, password })
        }
        Some("oauth2") => {
            let token = credentials
                .get("access_token")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
                .ok_or_else(|| Response::error("auth_failed", "the account has no access token"))?
                .to_string();
            Ok(Session::Token { user, token })
        }
        Some(other) => Err(Response::error(
            "unsupported",
            format!("{other} is not a credential this provider takes"),
        )),
        None => Err(Response::error(
            "protocol",
            "the credentials say what they are not",
        )),
    }
}

fn imap_session(session: &Session) -> Result<imap::Client, Response> {
    let mut client = imap::Client::connect().map_err(failed)?;
    let opened = match session {
        Session::Password { user, password } => client.login(user, password).map_err(failed),
        Session::Token { user, token } => client.authenticate_xoauth2(user, token).map_err(failed),
    };
    match opened {
        Ok(()) => Ok(client),
        Err(response) => {
            client.logout();
            Err(response)
        }
    }
}

fn failed(error: Failed) -> Response {
    match error {
        Failed::Auth(message) => Response::error("auth_failed", message),
        Failed::Network(message) => Response::error("network", message),
        Failed::Protocol(message) => Response::error("protocol", message),
    }
}

// ----- the OAuth2 flow -------------------------------------------------------

fn auth_begin(request: &Request) -> Response {
    let params = &request.params;
    let port = match params.get("redirect_port").and_then(Value::as_u64) {
        Some(port) if port > 0 && port <= u16::MAX as u64 => port as u16,
        _ => {
            return Response::error(
                "protocol",
                "auth.begin needs the loopback port the host is holding",
            )
        }
    };
    let client_id = text(params, "client_id")
        .filter(|id| !id.is_empty())
        .unwrap_or(DEFAULT_CLIENT_ID)
        .to_string();
    if client_id.is_empty() {
        return Response::error(
            "config",
            "no Google OAuth client id: set one in the account's config (a Desktop-type client from the deployment's Google Cloud project)",
        );
    }
    // Optional: a public client (Entra) has no secret, and asking for one it
    // does not have would refuse a flow that works. Google's desktop clients
    // do have one, and their token endpoint will not answer without it.
    let client_secret = text(params, "client_secret")
        .filter(|secret| !secret.is_empty())
        .map(str::to_string);
    let login_hint = text(params, "login_hint")
        .or_else(|| text(params, "email"))
        .map(str::to_string);
    let pending = match oauth::pending(&client_id, client_secret, port, login_hint) {
        Ok(pending) => pending,
        Err(message) => return Response::error("protocol", message),
    };
    let url = oauth::authorize_url(&pending);
    set_pending(Some(pending));
    Response::ok(json!({ "kind": "open_url", "url": url }))
}

/// The token endpoint's answer, opened out of the host's HTTP envelope.
/// Google says `access_token` on a grant and `error`/`error_description` on a
/// refusal; the status only explains a body that is not JSON at all.
fn tokens_of(envelope: &Value) -> Result<oauth::Tokens, String> {
    let status = envelope.get("status").and_then(Value::as_u64).unwrap_or(0);
    let bytes = abi::decoded_body(envelope)?;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(body) => oauth::parse_tokens(&body).map_err(explain_refusal),
        Err(error) => Err(format!(
            "the endpoint answered HTTP {status} with something that is not JSON ({error})"
        )),
    }
}

/// Google's refusal in words that name the thing to change. `client_secret is
/// missing` is not a broken flow: Google issues its clients a secret (desktop
/// ones included) and will not trade the code without it. The account form has
/// the field; the console shows the secret when it is created.
fn explain_refusal(message: String) -> String {
    if message.contains("client_secret is missing") {
        return "Google wants this OAuth client's secret as well as its id. Copy \
                the client secret from the same client in the console (a desktop \
                client's secret is public by design) into the account's \"OAuth \
                client secret\" field and sign in again."
            .to_string();
    }
    message
}

fn auth_continue(request: &Request) -> Response {
    let params = &request.params;
    let code = text(params, "code").unwrap_or_default();
    let state = text(params, "state").unwrap_or_default();
    let pending = match take_pending() {
        Some(pending) => pending,
        None => return Response::error("auth_failed", "no sign-in is in progress"),
    };
    // The state is the one thing that ties the redirect to this window; a
    // mismatch is a stale tab, not a sign-in to finish.
    if state != pending.state {
        return Response::error("auth_failed", "the browser's answer is for another sign-in");
    }
    if code.is_empty() {
        return Response::error("auth_failed", "the browser's answer carried no code");
    }
    let exchange = abi::http_envelope(&oauth::exchange_request(&pending, code), HTTP_CAP)
        .map_err(|message| Response::error("network", format!("token endpoint: {message}")))
        .and_then(|envelope| {
            tokens_of(&envelope).map_err(|message| {
                Response::error("auth_failed", format!("token endpoint: {message}"))
            })
        });
    match exchange {
        Ok(tokens) => Response {
            ok: true,
            result: Some(json!({ "kind": "done" })),
            credentials: Some(json!({
                "kind": "oauth2",
                "access_token": tokens.access_token,
                "refresh_token": tokens.refresh_token,
                "expires_at": tokens.expires_at,
                "scope": tokens.scope,
            })),
            error: None,
        },
        Err(response) => response,
    }
}

fn auth_refresh(request: &Request) -> Response {
    let params = &request.params;
    let refresh_token = match text(params, "refresh_token").filter(|token| !token.is_empty()) {
        Some(token) => token,
        None => return Response::error("auth_failed", "the account has no refresh token"),
    };
    let client_id = text(params, "client_id")
        .filter(|id| !id.is_empty())
        .unwrap_or(DEFAULT_CLIENT_ID);
    if client_id.is_empty() {
        return Response::error("config", "no Google OAuth client id to refresh with");
    }
    let client_secret = text(params, "client_secret").filter(|secret| !secret.is_empty());
    let refreshed = abi::http_envelope(
        &oauth::refresh_request(client_id, client_secret, refresh_token),
        HTTP_CAP,
    )
    .map_err(|message| Response::error("network", format!("token endpoint: {message}")))
    .and_then(|envelope| {
        tokens_of(&envelope)
            .map_err(|message| Response::error("auth_failed", format!("token endpoint: {message}")))
    });
    match refreshed {
        Ok(tokens) => Response {
            ok: true,
            result: Some(json!({ "kind": "done" })),
            credentials: Some(json!({
                "kind": "oauth2",
                "access_token": tokens.access_token,
                "refresh_token": tokens.refresh_token.or_else(|| Some(refresh_token.to_string())),
                "expires_at": tokens.expires_at,
                "scope": tokens.scope,
            })),
            error: None,
        },
        Err(response) => response,
    }
}

// ----- small parameter readers ----------------------------------------------

fn text<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

/// The sync mark the host kept, or nothing: `Value::Null` is const, so the
/// absent case borrows a static instead of a temporary.
static NULL_MARK: Value = Value::Null;

fn mark_of(params: &Value) -> &Value {
    params.get("mark").unwrap_or(&NULL_MARK)
}

fn labels_of(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .filter(|label| !label.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn uids_of(params: &Value) -> Vec<u32> {
    let list: Vec<String> = match params.get("uids").and_then(Value::as_array) {
        Some(list) => list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        None => text(params, "uid")
            .map(str::to_string)
            .into_iter()
            .collect(),
    };
    list.iter().filter_map(|uid| uid.parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// The OAuth pending sign-in is one global, so a test that writes it must
    /// not run beside one that spends it: whichever lost the race used to fail
    /// on a pending state the other had already taken.
    static OAUTH_PENDING: Mutex<()> = Mutex::new(());

    fn oauth_lock() -> MutexGuard<'static, ()> {
        OAUTH_PENDING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn request(method: &str, operation: Option<&str>, params: Value) -> Vec<u8> {
        // The tests write credentials inside `params` for readability; the
        // envelope carries them beside it, so lift them out.
        let credentials = params.get("credentials").cloned();
        let mut params = params;
        if let Some(object) = params.as_object_mut() {
            object.remove("credentials");
        }
        serde_json::to_vec(&json!({
            "abi": 1,
            "method": method,
            "operation": operation,
            "credentials": credentials,
            "params": params,
        }))
        .unwrap()
    }

    fn result_of(response: &[u8]) -> Value {
        let parsed: Value = serde_json::from_slice(response).unwrap();
        assert!(parsed["ok"].as_bool().unwrap(), "{parsed}");
        parsed["result"].clone()
    }

    fn error_of(response: &[u8]) -> (String, String) {
        let parsed: Value = serde_json::from_slice(response).unwrap();
        assert!(!parsed["ok"].as_bool().unwrap(), "{parsed}");
        (
            parsed["error"]["code"].as_str().unwrap().to_string(),
            parsed["error"]["message"].as_str().unwrap().to_string(),
        )
    }

    #[test]
    fn describe_declares_the_gmail_profile() {
        let described = result_of(&dispatch(&request("provider.describe", None, json!({}))));
        let profiles = described["profiles"].as_array().unwrap();
        assert!(profiles.contains(&json!("kimo-gmail/1")));
        let capabilities: Vec<&str> = described["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|cap| cap["name"].as_str().unwrap())
            .collect();
        for needed in [
            "mailbox.list",
            "mailbox.sync",
            "message.fetch",
            "message.set_flags",
            "message.set_labels",
            "message.delete",
            "message.append",
            "query.raw",
            "thread.native",
            "send.submit",
            "auth.oauth2.auth_code_pkce",
        ] {
            assert!(capabilities.contains(&needed), "{needed} missing");
        }
        let raw: Vec<&str> = described["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|cap| cap["name"] == "query.raw")
            .map(|cap| cap["params"]["dialect"].as_str().unwrap())
            .collect();
        assert_eq!(raw, ["gmail"]);
    }

    #[test]
    fn an_unknown_operation_is_unsupported_not_a_trap() {
        // With credentials an account could use, so the only thing left to
        // refuse the call is the operation name itself.
        let (code, _) = error_of(&dispatch(&request(
            "provider.invoke",
            Some("message.junk"),
            json!({
                "credentials": { "kind": "oauth2", "username": "u@gmail.com", "access_token": "t" },
            }),
        )));
        assert_eq!(code, "unsupported");
    }

    #[test]
    fn network_operations_need_credentials() {
        let (code, _) = error_of(&dispatch(&request(
            "provider.invoke",
            Some("mailbox.list"),
            json!({}),
        )));
        assert_eq!(code, "auth_failed");
    }

    #[test]
    fn auth_begin_builds_the_url_the_host_opens() {
        let _oauth = oauth_lock();
        set_pending(None);
        let challenge = result_of(&dispatch(&request(
            "auth.begin",
            None,
            json!({ "redirect_port": 8914, "client_id": "cid-1" }),
        )));
        assert_eq!(challenge["kind"], "open_url");
        let url = challenge["url"].as_str().unwrap();
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8914%2F"));
        assert!(url.contains("code_challenge_method=S256"));
        set_pending(None);
    }

    #[test]
    fn auth_begin_refuses_to_invent_a_client_id() {
        let (code, message) = error_of(&dispatch(&request(
            "auth.begin",
            None,
            json!({ "redirect_port": 8914 }),
        )));
        assert_eq!(code, "config");
        assert!(message.contains("client id"));
    }

    #[test]
    fn a_grant_is_read_out_of_the_hosts_envelope() {
        // What `StdNetwork` really hands a module: status, headers, and the
        // endpoint's JSON base64'd into `body`. Reading `access_token` off the
        // envelope itself is the bug this pins down.
        let granted = json!({
            "access_token": "ya29.test",
            "refresh_token": "1//refresh",
            "expires_in": 3600,
            "scope": "https://mail.google.com/",
        });
        let envelope = json!({
            "status": 200,
            "headers": [{ "name": "content-type", "value": "application/json" }],
            "body": oauth::base64_std(&serde_json::to_vec(&granted).unwrap()),
        });
        let tokens = tokens_of(&envelope).expect("the grant parses");
        assert_eq!(tokens.access_token, "ya29.test");
        assert_eq!(tokens.refresh_token.as_deref(), Some("1//refresh"));
        assert!(tokens.expires_at.is_some());
    }

    #[test]
    fn a_refusal_keeps_googles_own_words() {
        let refused = json!({
            "error": "invalid_grant",
            "error_description": "Bad Request",
        });
        let envelope = json!({
            "status": 400,
            "headers": [],
            "body": oauth::base64_std(&serde_json::to_vec(&refused).unwrap()),
        });
        let message = tokens_of(&envelope).expect_err("a refusal is an error");
        assert_eq!(message, "Bad Request");
    }

    #[test]
    fn a_missing_secret_names_the_field_to_fill() {
        // Google answers a client without its secret with this and nothing
        // else; the user needs the fix, not the fact.
        let refused = json!({
            "error": "invalid_request",
            "error_description": "client_secret is missing.",
        });
        let envelope = json!({
            "status": 400,
            "headers": [],
            "body": oauth::base64_std(&serde_json::to_vec(&refused).unwrap()),
        });
        let message = tokens_of(&envelope).expect_err("a refusal is an error");
        assert!(message.contains("secret"), "{message}");
        assert!(message.contains("OAuth client secret"), "{message}");
    }

    #[test]
    fn auth_begin_carries_the_secret_into_the_exchange() {
        let _oauth = oauth_lock();
        set_pending(None);
        dispatch(&request(
            "auth.begin",
            None,
            json!({ "redirect_port": 8914, "client_id": "cid-1", "client_secret": "cs-1" }),
        ));
        let pending = take_pending().expect("a sign-in is pending");
        assert_eq!(pending.client_secret.as_deref(), Some("cs-1"));
        set_pending(None);
    }

    #[test]
    fn a_body_that_is_not_json_names_its_status() {
        let envelope = json!({
            "status": 502,
            "headers": [],
            "body": oauth::base64_std(b"<html>bad gateway</html>"),
        });
        let message = tokens_of(&envelope).expect_err("an HTML error page is an error");
        assert!(message.contains("502"), "{message}");
    }

    #[test]
    fn auth_continue_checks_the_state_before_spending_the_code() {
        let _oauth = oauth_lock();
        set_pending(None);
        dispatch(&request(
            "auth.begin",
            None,
            json!({ "redirect_port": 8914, "client_id": "cid-1" }),
        ));
        let (code, message) = error_of(&dispatch(&request(
            "auth.continue",
            None,
            json!({ "code": "c", "state": "not-the-state" }),
        )));
        assert_eq!(code, "auth_failed");
        assert!(message.contains("another sign-in"));
        set_pending(None);
    }

    #[test]
    fn a_search_with_a_command_in_it_is_refused() {
        let (code, _) = error_of(&dispatch(&request(
            "provider.invoke",
            Some("message.search"),
            json!({
                "credentials": { "kind": "oauth2", "username": "u@gmail.com", "access_token": "t" },
                "expression": "from:me\r\nNOOP",
            }),
        )));
        // The credentials are there, so the operation reaches its own
        // validation before any socket; the CRLF is what stops it.
        assert_eq!(code, "protocol");
    }

    #[test]
    fn older_uids_walks_below_the_floor_in_batches() {
        assert_eq!(older_uids(100, 1, 50), Some((50, 99)));
        assert_eq!(older_uids(30, 1, 50), Some((1, 29)));
        assert_eq!(older_uids(1, 1, 50), None);
        assert_eq!(older_uids(100, 100, 50), None);
    }
}
