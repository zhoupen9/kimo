//! The operation catalogue: what `provider.invoke` (and the `auth.*` pair)
//! answer, per `docs/mail-spec.md` §5–§6.5.
//!
//! Every operation is one or a few Graph calls on the account's token; no
//! session survives an invoke. The sync is POP3-shaped on purpose: the host
//! hands over the message ids its database already holds (`sync.stored_uids`)
//! and a page of the newest summaries is filtered against them, so an
//! incremental sync is one listing plus the fetches of what is genuinely
//! new — the same contract the POP3 adapter has, spoken over HTTPS.

use std::ptr::addr_of_mut;

use serde_json::{json, Value};

use crate::abi::{self, Request, Response};
use crate::graph::{self, Failed, Graph};
use crate::oauth;

/// The newest mail a folder's first fetch takes.
const BACKFILL_LIMIT: u32 = 50;
/// A KQL expression longer than this is not a search anyone typed.
const RAW_QUERY_CAP: usize = 1024;
/// Token responses and id pages are small; this is the HTTP buffer the
/// plugin offers them.
const HTTP_CAP: usize = 64 * 1024;

/// The deployment's Entra client id, which the deployment registers — not
/// something the software can ship. Empty means "bring your own", and
/// `auth.begin` says so rather than inventing one.
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

/// The static descriptor: the Graph profile, with the network rules the
/// host turns into an endpoint allowlist. What is deliberately *not* here:
/// `message.append` (Graph files sent mail itself), `message.set_labels`
/// (categories are their own mapping, not yet spoken), and `thread.native`
/// (`conversationId` is not returned with a page yet) — a capability is a
/// promise, and none of these is kept yet.
fn describe() -> Response {
    Response::ok(json!({
        "id": "outlook",
        "name": "Outlook / Microsoft 365",
        "version": env!("CARGO_PKG_VERSION"),
        "spec_version": "1.0",
        "profiles": ["kimo-core/1", "kimo-graph/1"],
        "capabilities": [
            { "name": "mailbox.list" },
            { "name": "mailbox.sync" },
            { "name": "message.list" },
            { "name": "message.fetch" },
            { "name": "message.fetch.raw" },
            { "name": "message.set_flags" },
            { "name": "message.delete" },
            { "name": "message.search" },
            { "name": "query.raw", "params": { "dialect": "graph-kql" } },
            { "name": "send.submit" },
            { "name": "send.save_copy" },
            { "name": "sync.stored_uids" },
            { "name": "auth.oauth2.auth_code_pkce" }
        ],
        "auth": [
            { "kind": "oauth2", "flow": "authorization_code_pkce", "scope": oauth::SCOPE }
        ],
        "config": [
            { "id": "email", "label": "Email address", "type": "text", "required": true },
            { "id": "client_id", "label": "Microsoft (Entra) OAuth client id — an app registration with a desktop redirect URI", "type": "text", "required": false }
        ],
        "network": [
            { "transport": "https", "host": "login.microsoftonline.com" },
            { "transport": "https", "host": "graph.microsoft.com" }
        ]
    }))
}

/// One call on the account's token: the module has no password path at all,
/// because Microsoft switched basic auth off everywhere that matters.
fn session(request: &Request) -> Result<Graph, Response> {
    let credentials = request.credentials.as_ref().ok_or_else(|| {
        Response::error(
            "auth_failed",
            "the account has no credentials to open Graph with",
        )
    })?;
    let token = credentials
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            Response::error(
                "auth_failed",
                "sign in with Microsoft — Graph has no other way in",
            )
        })?;
    Ok(Graph::new(token))
}

fn health(request: &Request) -> Response {
    let graph = match session(request) {
        Ok(graph) => graph,
        Err(response) => return response,
    };
    match graph.alive() {
        Ok(()) => Response::ok(json!({ "ok": true, "profiles": ["kimo-core/1", "kimo-graph/1"] })),
        Err(error) => {
            if let Failed::Auth(message) = &error {
                abi::log(1, message);
            }
            failed(error)
        }
    }
}

fn invoke(request: &Request, operation: &str) -> Response {
    // The descriptor is static: no credentials, no socket. It has to answer
    // here, before the session, because the host asks for it on its own
    // (capability gates, the account form) with nothing to log in with.
    if operation == "provider.describe" {
        return describe();
    }
    let graph = match session(request) {
        Ok(graph) => graph,
        Err(response) => return response,
    };
    let params = &request.params;
    let result: Result<Value, Response> = match operation {
        "mailbox.list" => graph
            .folders()
            .map(|folders| {
                json!(folders
                    .into_iter()
                    .map(|folder| folder.display)
                    .collect::<Vec<_>>())
            })
            .map_err(failed),
        "mailbox.sync" => sync_folder(&graph, params),
        "mailbox.sync.older" => older_folder(&graph, params),
        "message.fetch" => {
            let uid = match text(params, "uid") {
                Some(uid) => uid,
                None => return Response::error("protocol", "message.fetch without a uid"),
            };
            graph
                .raw(uid)
                .map(|raw| json!({ "raw": oauth::base64_std(&raw) }))
                .map_err(failed)
        }
        "message.set_flags" => {
            let uid = match text(params, "uid") {
                Some(uid) => uid,
                None => return Response::error("protocol", "message.set_flags without a uid"),
            };
            let mut result = Ok(());
            if let Some(seen) = params.get("seen").and_then(Value::as_bool) {
                result = graph.set_read(uid, seen).map_err(failed);
            }
            if result.is_ok() {
                if let Some(flagged) = params.get("flagged").and_then(Value::as_bool) {
                    result = graph.set_flagged(uid, flagged).map_err(failed);
                }
            }
            result.map(|()| json!({}))
        }
        "message.delete" => {
            let uids: Vec<String> = params
                .get("uids")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if uids.is_empty() {
                return Response::error("protocol", "message.delete without messages");
            }
            let mut result = Ok(());
            for uid in &uids {
                result = graph.delete_message(uid).map_err(failed);
                if result.is_err() {
                    break;
                }
            }
            // Graph's delete already moved the mail to Deleted Items, so
            // every uid that succeeded is gone from where it was.
            result.map(|()| json!({ "gone": uids }))
        }
        "message.search" => {
            let expression = match text(params, "expression").or_else(|| text(params, "query")) {
                Some(expression) => expression,
                None => return Response::error("protocol", "message.search without an expression"),
            };
            if let Some(dialect) = text(params, "dialect") {
                if dialect != "graph-kql" {
                    return Response::error(
                        "unsupported",
                        format!("the query dialect {dialect} is not Graph's"),
                    );
                }
            }
            if expression.contains('\r')
                || expression.contains('\n')
                || expression.len() > RAW_QUERY_CAP
            {
                return Response::error(
                    "protocol",
                    "the search expression is not one a person typed",
                );
            }
            graph
                .search(expression)
                .map(|uids| json!({ "uids": uids }))
                .map_err(failed)
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
            let recipients: Vec<String> = envelope
                .get("to")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if recipients.is_empty() {
                return Response::error("protocol", "send.submit needs a recipient");
            }
            let message = match graph::message_for_send(&recipients, &raw) {
                Ok(message) => message,
                Err(message) => return Response::error("protocol", message),
            };
            graph
                .send(message)
                .map(|()| json!({ "status": "accepted" }))
                .map_err(failed)
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

/// One folder's sync: the newest page of summaries, filtered against the
/// ids the database already holds, each new one fetched as its raw source.
/// The host's `first_num` mark is this provider's page depth — how far down
/// the folder has been paged — which is what `Load older mail` resumes from.
fn sync_folder(graph: &Graph, params: &Value) -> Result<Value, Response> {
    let folder = match text(params, "folder") {
        Some(folder) => folder,
        None => return Err(Response::error("protocol", "mailbox.sync without a folder")),
    };
    let mark = params.get("mark").cloned().unwrap_or(Value::Null);
    let stored = stored_uids(&mark);
    let folder_id = graph.folder_id(folder).map_err(failed)?;
    let page = graph
        .summaries(&folder_id, BACKFILL_LIMIT, 0)
        .map_err(failed)?;

    let mut messages = Vec::new();
    for summary in &page {
        if stored
            .as_ref()
            .is_some_and(|stored| stored.contains(&summary.id))
        {
            continue;
        }
        let raw = match graph.raw(&summary.id) {
            Ok(raw) => raw,
            Err(error) => return Err(failed(error)),
        };
        messages.push(message_json(summary, &raw));
    }

    Ok(json!({
        "uidvalidity": null,
        // A warm sync (the host knows ids) is the incremental one; the first
        // fill is the backfill window.
        "kind": if stored.is_some() { "resume" } else { "fill" },
        // The older-mail walk resumes from how deep this fill paged.
        "first_num": if stored.is_some() {
            Value::Null
        } else {
            json!(page.len())
        },
        "oldest": null,
        "marks_before_store": false,
        "messages": messages,
    }))
}

/// The page below the depth the folder has been paged to: `$skip` pages
/// down in Graph's own arithmetic, and the depth moves by the page's length
/// whether or not the page held anything new.
fn older_folder(graph: &Graph, params: &Value) -> Result<Value, Response> {
    let folder = match text(params, "folder") {
        Some(folder) => folder,
        None => {
            return Err(Response::error(
                "protocol",
                "mailbox.sync.older without a folder",
            ))
        }
    };
    let batch = params
        .get("batch")
        .and_then(Value::as_u64)
        .and_then(|batch| u32::try_from(batch).ok())
        .unwrap_or(BACKFILL_LIMIT);
    let mark = params.get("mark").cloned().unwrap_or(Value::Null);
    let stored = stored_uids(&mark);
    let depth = mark
        .get("first_num")
        .and_then(Value::as_u64)
        .and_then(|depth| u32::try_from(depth).ok())
        .unwrap_or(0);

    let folder_id = graph.folder_id(folder).map_err(failed)?;
    let page = graph
        .summaries(&folder_id, batch.max(1), depth)
        .map_err(failed)?;

    let mut messages = Vec::new();
    for summary in &page {
        if stored
            .as_ref()
            .is_some_and(|stored| stored.contains(&summary.id))
        {
            continue;
        }
        let raw = match graph.raw(&summary.id) {
            Ok(raw) => raw,
            Err(error) => return Err(failed(error)),
        };
        messages.push(message_json(summary, &raw));
    }

    Ok(json!({
        "uidvalidity": null,
        "first_num": depth + page.len() as u32,
        "oldest": null,
        // A full page means there may be more below it; a short one is the
        // bottom of the folder.
        "available": page.len() as u32 == batch,
        "messages": messages,
    }))
}

fn message_json(summary: &graph::Summary, raw: &[u8]) -> Value {
    json!({
        "uid": summary.id,
        "seen": summary.read,
        "flagged": summary.flagged,
        "raw": oauth::base64_std(raw),
    })
}

/// The ids the host's database already holds, when it sent them.
fn stored_uids(mark: &Value) -> Option<Vec<String>> {
    mark.get("stored").and_then(Value::as_array).map(|list| {
        list.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
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
            "no Microsoft client id: set one in the account's config (an Entra app registration with a desktop redirect URI)",
        );
    }
    let login_hint = text(params, "login_hint")
        .or_else(|| text(params, "email"))
        .map(str::to_string);
    let pending = match oauth::pending(&client_id, port, login_hint) {
        Ok(pending) => pending,
        Err(message) => return Response::error("protocol", message),
    };
    let url = oauth::authorize_url(&pending);
    set_pending(Some(pending));
    Response::ok(json!({ "kind": "open_url", "url": url }))
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
    let exchange = token_call(&oauth::exchange_request(&pending, code));
    match exchange {
        Ok(tokens) => credentials_response(tokens, None),
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
        return Response::error("config", "no Microsoft client id to refresh with");
    }
    let refreshed = token_call(&oauth::refresh_request(client_id, refresh_token));
    match refreshed {
        // Entra does not always hand the refresh token back on a refresh;
        // the one the account holds stays the one it holds.
        Ok(tokens) => credentials_response(tokens, Some(refresh_token.to_string())),
        Err(response) => response,
    }
}

/// One call to the token endpoint: the HTTP envelope opened, its body
/// decoded, and the tokens parsed out of it — with the endpoint's own
/// words for every way it can refuse.
fn token_call(request: &Value) -> Result<oauth::Tokens, Response> {
    abi::http_envelope(request, HTTP_CAP)
        .map_err(|message| Response::error("network", format!("token endpoint: {message}")))
        .and_then(|envelope| {
            let body = abi::decoded_body(&envelope).map_err(|message| {
                Response::error("protocol", format!("token endpoint: {message}"))
            })?;
            let parsed: Value = serde_json::from_slice(&body)
                .map_err(|error| Response::error("protocol", format!("token endpoint: {error}")))?;
            oauth::parse_tokens(&parsed).map_err(|message| {
                Response::error("auth_failed", format!("token endpoint: {message}"))
            })
        })
}

fn credentials_response(tokens: oauth::Tokens, keep_refresh: Option<String>) -> Response {
    Response {
        ok: true,
        result: Some(json!({ "kind": "done" })),
        credentials: Some(json!({
            "kind": "oauth2",
            "access_token": tokens.access_token,
            "refresh_token": tokens.refresh_token.or(keep_refresh),
            "expires_at": tokens.expires_at,
            "scope": tokens.scope,
        })),
        error: None,
    }
}

/// A Graph failure as the envelope's error: a throttled request carries the
/// server's own wait, which the host knows how to show.
fn failed(error: Failed) -> Response {
    match error {
        Failed::Auth(message) => Response::error("auth_failed", message),
        Failed::Rate {
            message,
            retry_after_ms,
        } => {
            let mut response = Response::error("rate_limited", message);
            if let Some(error) = response.error.as_mut() {
                error.retryable = Some(true);
                error.retry_after_ms = retry_after_ms;
            }
            response
        }
        Failed::Protocol(message) => Response::error("protocol", message),
    }
}

fn text<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, operation: Option<&str>, params: Value) -> Vec<u8> {
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
    fn describe_declares_the_graph_profile() {
        let described = result_of(&dispatch(&request("provider.describe", None, json!({}))));
        let profiles = described["profiles"].as_array().unwrap();
        assert!(profiles.contains(&json!("kimo-graph/1")));
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
            "message.delete",
            "send.submit",
            "send.save_copy",
            "sync.stored_uids",
            "auth.oauth2.auth_code_pkce",
        ] {
            assert!(capabilities.contains(&needed), "{needed} missing");
        }
        // A capability is a promise: the mappings not spoken yet stay out.
        assert!(!capabilities.contains(&"message.set_labels"));
        assert!(!capabilities.contains(&"message.append"));
        // And the dialect of the raw query is Graph's own.
        let raw: Vec<&str> = described["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|cap| cap["name"] == "query.raw")
            .map(|cap| cap["params"]["dialect"].as_str().unwrap())
            .collect();
        assert_eq!(raw, ["graph-kql"]);
    }

    #[test]
    fn an_unknown_operation_is_unsupported_not_a_trap() {
        let (code, _) = error_of(&dispatch(&request(
            "provider.invoke",
            Some("message.move"),
            json!({
                "credentials": { "kind": "oauth2", "username": "u@contoso.com", "access_token": "t" },
            }),
        )));
        assert_eq!(code, "unsupported");
    }

    #[test]
    fn a_password_is_not_a_way_into_graph() {
        let (code, message) = error_of(&dispatch(&request(
            "provider.invoke",
            Some("mailbox.list"),
            json!({
                "credentials": { "kind": "password", "username": "u", "password": "p" },
            }),
        )));
        assert_eq!(code, "auth_failed");
        assert!(message.contains("Microsoft"));
    }

    #[test]
    fn auth_begin_builds_the_entra_url_the_host_opens() {
        set_pending(None);
        let challenge = result_of(&dispatch(&request(
            "auth.begin",
            None,
            json!({ "redirect_port": 8914, "client_id": "cid-1" }),
        )));
        assert_eq!(challenge["kind"], "open_url");
        let url = challenge["url"].as_str().unwrap();
        assert!(url.starts_with("https://login.microsoftonline.com/common/oauth2/v2.0/authorize?"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8914%2F"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("response_mode=query"));
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
    fn auth_continue_checks_the_state_before_spending_the_code() {
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
    fn a_throttled_graph_answer_carries_the_servers_own_wait() {
        let response = failed(Failed::Rate {
            message: "Graph asked us to slow down".to_string(),
            retry_after_ms: Some(5000),
        });
        let bytes = response.to_bytes();
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["error"]["code"], "rate_limited");
        assert_eq!(parsed["error"]["retryable"], true);
        assert_eq!(parsed["error"]["retry_after_ms"], 5000);
    }
}
