//! A `MailProvider` adapter over one WASM plugin instance.
//!
//! The operation names and JSON shapes here are the bridge between the
//! existing provider layer and the WASM ABI. A standard protocol plugin can
//! implement the same methods over IMAP/POP3; a Graph plugin can answer the
//! same operations through its own host imports.

use base64::Engine as _;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::{MailError, MailResult};
use crate::mail;
use crate::models::Account;
use crate::provider::{
    Capabilities, DeleteOutcome, FetchKind, FolderBatch, FolderMark, MailProvider, OlderBatch,
};

use super::abi::PluginRequest;
use super::host::{HostError, HostNetwork, PluginInstance, WasmPlugin};

/// One plugin-backed mailbox session. The instance is single-threaded, just
/// like the existing adapters; `sync.rs` calls it from a worker only.
pub struct WasmProvider {
    instance: PluginInstance,
    account: Account,
    capabilities: Capabilities,
    secrets: bool,
}

impl WasmProvider {
    /// Open one session. The capabilities are not a parameter: the module
    /// describes itself (`provider.describe`), and what it declares is what
    /// the host dispatches on — a Gmail server without `X-GM-EXT-1` is the
    /// module's own degrade to answer for.
    pub fn open(
        plugin: &WasmPlugin,
        account: &Account,
        network: Arc<dyn HostNetwork>,
    ) -> Result<Self, HostError> {
        let instance = plugin.instantiate_with(network)?;
        let mut provider = Self {
            instance,
            account: account.clone(),
            capabilities: Capabilities {
                folders: false,
                server_flags: false,
                append: false,
                stored_uids: false,
            },
            secrets: plugin.permissions().secrets,
        };
        if let Ok(described) = provider.call("provider.describe", json!({})) {
            let declared = |name: &str| {
                described
                    .get("capabilities")
                    .and_then(Value::as_array)
                    .is_some_and(|list| {
                        list.iter().any(|capability| {
                            capability.get("name").and_then(Value::as_str) == Some(name)
                        })
                    })
            };
            provider.capabilities = Capabilities {
                folders: declared("mailbox.list"),
                server_flags: declared("message.set_flags"),
                append: declared("message.append"),
                // A provider with no UID arithmetic of its own (Graph) is
                // told which messages the database already holds, POP3-style.
                stored_uids: declared("sync.stored_uids"),
            };
        }
        Ok(provider)
    }

    fn call(&mut self, operation: &str, params: Value) -> MailResult<Value> {
        let mut request = PluginRequest::operation("provider.invoke", operation);
        request.account = Some(json!({
            "id": self.account.id,
            "name": self.account.name,
            "protocol": self.account.protocol.as_str(),
            "config": {
                "host": self.account.host,
                "port": self.account.port,
                "tls": self.account.tls.as_str(),
            }
        }));
        if self.secrets {
            // An OAuth2 account signs in with its token; the password pair is
            // the plugin's declared app-password path. Which one this account
            // has is the account's own state, not a guess.
            request.credentials = Some(if self.account.access_token.is_some() {
                json!({
                    "kind": "oauth2",
                    "username": self.account.username,
                    "access_token": self.account.access_token,
                })
            } else {
                json!({
                    "kind": "password",
                    "username": self.account.username,
                    "password": self.account.password,
                })
            });
        }
        request.params = params;
        let response = self.instance.invoke(&request).map_err(host_error)?;
        if !response.ok {
            let error = response
                .error
                .unwrap_or_else(|| crate::plugin::abi::PluginError::new("failed", "no reason"));
            let message = format!("{}: {}", error.code, error.message);
            // A refused login is the user's to read plainly; everything else
            // keeps its code so the caller can tell a network problem from a
            // protocol one.
            return Err(if error.code == "auth_failed" {
                MailError::Auth(message)
            } else {
                MailError::Protocol(message)
            });
        }
        Ok(response.result)
    }

    /// Submit a fully built message through the module's own submission
    /// (`send.submit`): XOAUTH2 on Gmail, Graph's `sendMail` on an Outlook
    /// module. The envelope travels beside the bytes because SMTP wants
    /// addresses the mail itself may not name exactly (Bcc).
    pub fn submit(&mut self, from: &str, recipients: &[String], raw: &[u8]) -> MailResult<()> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        self.call(
            "send.submit",
            json!({
                "envelope": { "from": from, "to": recipients },
                "rfc822": encoded,
            }),
        )?;
        Ok(())
    }
}

fn host_error(error: HostError) -> MailError {
    MailError::Protocol(error.to_string())
}

// ----- the account-level entry points ----------------------------------------

/// Run `f` with the plugin that contributes the provider this account names.
/// The lookup failure is one message whatever the cause — a disabled module
/// is not in the registry at all, and to the account the two are the same
/// thing: no sessions until it is back.
fn with_plugin<T>(
    account: &Account,
    f: impl FnOnce(&WasmPlugin) -> MailResult<T>,
) -> MailResult<T> {
    let id = account
        .provider
        .as_deref()
        .ok_or_else(|| MailError::Protocol("the account names no plugin provider".to_string()))?;
    let missing = || {
        MailError::Protocol(format!(
            "the {id} provider plugin is not installed or enabled"
        ))
    };
    let registry = crate::plugin::discovery::installed().ok_or_else(missing)?;
    let loaded = registry.provider(id).ok_or_else(missing)?;
    f(&loaded.plugin)
}

/// Open an account's plugin-backed mailbox session on the real network,
/// refreshing its sign-in token first when it is about to expire. Every
/// `provider::with` call lands here, so no caller has to know a token has a
/// clock.
pub fn open_account(account: &Account) -> MailResult<WasmProvider> {
    let account = ensure_tokens(account)?;
    with_plugin(&account, |plugin| {
        WasmProvider::open(
            plugin,
            &account,
            Arc::new(super::net::StdNetwork::default()),
        )
        .map_err(host_error)
    })
}

/// Whether the module declares a capability, for the host's dispatch gates.
pub fn declares_public(account: &Account, name: &str) -> bool {
    declares(account, name)
}

fn declares(account: &Account, name: &str) -> bool {
    with_plugin(account, |plugin| Ok(declares_in(plugin, name))).unwrap_or(false)
}

/// Whether the module files its own Sent copy (`send.save_copy`): a provider
/// that does must not be handed a second copy to `message.append`, which is
/// how a Gmail or Graph submission would end up filed twice.
pub fn files_own_sent(account: &Account) -> bool {
    declares(account, "send.save_copy")
}

/// Whether the module's messages can be pushed to the server
/// (`message.set_flags`), for the flag round trips.
pub fn server_flags(account: &Account) -> bool {
    declares(account, "message.set_flags")
}

/// Whether a *module* (not an account) declares a capability: one
/// `provider.describe` on a throwaway session — a static JSON answer, no
/// socket, no token — so a gate that only wants to know what the module
/// can do does not pay for a real one.
pub fn declares_in(plugin: &WasmPlugin, name: &str) -> bool {
    let Ok(mut instance) = plugin.instantiate() else {
        return false;
    };
    let mut request = PluginRequest::operation("provider.invoke", "provider.describe");
    request.params = json!({});
    let Ok(response) = instance.invoke(&request) else {
        return false;
    };
    response
        .result
        .get("capabilities")
        .and_then(Value::as_array)
        .is_some_and(|list| {
            list.iter()
                .any(|capability| capability.get("name").and_then(Value::as_str) == Some(name))
        })
}

/// Submit one fully built message through the account's plugin provider.
pub fn submit_for_account(
    account: &Account,
    from: &str,
    recipients: &[String],
    raw: &[u8],
) -> MailResult<()> {
    let account = ensure_tokens(account)?;
    with_plugin(&account, |plugin| {
        let mut provider = WasmProvider::open(
            plugin,
            &account,
            Arc::new(super::net::StdNetwork::default()),
        )
        .map_err(host_error)?;
        provider.submit(from, recipients, raw)
    })
}

/// The account's tokens, refreshed when they are inside two minutes of
/// expiring and persisted when they changed. An account with no expiry known
/// uses what it has; the server is the one who says whether that was good.
pub fn ensure_tokens(account: &Account) -> MailResult<Account> {
    let Some(expires_at) = account.token_expires_at else {
        return Ok(account.clone());
    };
    if now_ms().saturating_add(120_000) < expires_at {
        return Ok(account.clone());
    }
    let Some(refresh_token) = account
        .refresh_token
        .clone()
        .filter(|token| !token.is_empty())
    else {
        return Err(MailError::Auth(
            "the provider sign-in has expired — sign in again in Preferences → Accounts"
                .to_string(),
        ));
    };
    let client_id = client_id_of(account);
    let mut refreshed = account.clone();
    with_plugin(account, |plugin| {
        let tokens = super::auth::refresh(
            plugin,
            Arc::new(super::net::StdNetwork::default()),
            &refresh_token,
            &client_id,
        )?;
        refreshed.access_token = Some(tokens.access_token);
        if let Some(token) = tokens.refresh_token {
            refreshed.refresh_token = Some(token);
        }
        refreshed.token_expires_at = tokens.expires_at;
        Ok(())
    })?;
    crate::db::save_account_tokens(
        &account.id,
        refreshed.access_token.as_deref(),
        refreshed.refresh_token.as_deref(),
        refreshed.token_expires_at,
    )?;
    Ok(refreshed)
}

/// The OAuth client id the account's provider config carries, or nothing.
pub fn client_id_of(account: &Account) -> String {
    account
        .provider_config
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|config| {
            config
                .get("client_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn base64_decode(value: &Value) -> MailResult<Vec<u8>> {
    let encoded = value
        .as_str()
        .ok_or_else(|| MailError::Protocol("expected a base64 string".to_string()))?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| MailError::Protocol(format!("invalid base64: {error}")))
}

fn messages(value: &Value) -> MailResult<Vec<crate::models::MailMessage>> {
    let list = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| MailError::Protocol("sync result has no messages".to_string()))?;
    let mut parsed = Vec::with_capacity(list.len());
    for item in list {
        let uid = item
            .get("uid")
            .and_then(Value::as_str)
            .ok_or_else(|| MailError::Protocol("message has no uid".to_string()))?;
        let seen = item.get("seen").and_then(Value::as_bool).unwrap_or(false);
        let raw = base64_decode(
            item.get("raw")
                .ok_or_else(|| MailError::Protocol("message has no raw source".to_string()))?,
        )?;
        parsed.push(mail::parse(&raw, uid, seen));
    }
    Ok(parsed)
}

fn optional_u32(value: &Value, key: &str) -> MailResult<Option<u32>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(number) => number
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
            .map(Some)
            .ok_or_else(|| MailError::Protocol(format!("{key} is not a u32"))),
    }
}

fn oldest(value: &Value) -> MailResult<Option<Option<u32>>> {
    if value.get("oldest").is_none() {
        return Ok(None);
    }
    if value["oldest"].is_null() {
        return Ok(Some(None));
    }
    Ok(Some(Some(
        value["oldest"]
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
            .ok_or_else(|| MailError::Protocol("oldest is not a u32".to_string()))?,
    )))
}

impl MailProvider for WasmProvider {
    fn capabilities(&self) -> crate::provider::Capabilities {
        self.capabilities
    }

    fn folders(&mut self) -> MailResult<Vec<String>> {
        let result = self.call("mailbox.list", json!({}))?;
        result
            .as_array()
            .ok_or_else(|| MailError::Protocol("mailbox.list did not return an array".to_string()))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| MailError::Protocol("mailbox name is not a string".to_string()))
            })
            .collect()
    }

    fn fetch_folder(&mut self, folder: &str, mark: &FolderMark) -> MailResult<FolderBatch> {
        let result = self.call(
            "mailbox.sync",
            json!({
                "folder": folder,
                "mark": {
                    "uidvalidity": mark.uidvalidity,
                    "uid_next": mark.uid_next,
                    "first_num": mark.first_num,
                    "stored": mark.stored,
                }
            }),
        )?;
        Ok(FolderBatch {
            uidvalidity: optional_u32(&result, "uidvalidity")?,
            messages: messages(&result)?,
            kind: if result.get("kind").and_then(Value::as_str) == Some("fill") {
                FetchKind::Fill
            } else {
                FetchKind::Resume
            },
            first_num: optional_u32(&result, "first_num")?,
            oldest: oldest(&result)?,
            marks_before_store: result
                .get("marks_before_store")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    fn fetch_older(
        &mut self,
        folder: &str,
        mark: &FolderMark,
        batch: u32,
    ) -> MailResult<OlderBatch> {
        let result = self.call(
            "mailbox.sync.older",
            json!({
                "folder": folder,
                "batch": batch,
                "mark": {
                    "uidvalidity": mark.uidvalidity,
                    "uid_next": mark.uid_next,
                    "first_num": mark.first_num,
                    "stored": mark.stored,
                }
            }),
        )?;
        Ok(OlderBatch {
            uidvalidity: optional_u32(&result, "uidvalidity")?,
            messages: messages(&result)?,
            first_num: optional_u32(&result, "first_num")?,
            oldest: oldest(&result)?,
            available: result
                .get("available")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    fn body(&mut self, folder: &str, uid: &str) -> MailResult<Vec<u8>> {
        let result = self.call("message.fetch", json!({ "folder": folder, "uid": uid }))?;
        base64_decode(&result)
    }

    fn set_seen(&mut self, folder: &str, uid: &str) -> MailResult<()> {
        self.call(
            "message.set_flags",
            json!({ "folder": folder, "uid": uid, "seen": true }),
        )?;
        Ok(())
    }

    fn set_flagged(&mut self, folder: &str, uid: &str, flagged: bool) -> MailResult<()> {
        self.call(
            "message.set_flags",
            json!({ "folder": folder, "uid": uid, "flagged": flagged }),
        )?;
        Ok(())
    }

    fn delete(&mut self, folder: &str, uids: &[String]) -> DeleteOutcome {
        match self.call("message.delete", json!({ "folder": folder, "uids": uids })) {
            Ok(result) => DeleteOutcome {
                gone: result
                    .get("gone")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                error: None,
            },
            Err(error) => DeleteOutcome {
                gone: Vec::new(),
                error: Some(error),
            },
        }
    }

    fn append(&mut self, mailbox: &str, raw: &[u8]) -> MailResult<()> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        self.call(
            "message.append",
            json!({ "mailbox": mailbox, "raw": encoded }),
        )?;
        Ok(())
    }

    fn logout(&mut self) {
        let _ = self.call("provider.logout", json!({}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Protocol, TlsMode};
    use crate::provider::Capabilities;

    fn account() -> Account {
        Account {
            id: "test".to_string(),
            name: "Test".to_string(),
            protocol: Protocol::Imap,
            host: "imap.example.com".to_string(),
            port: 993,
            username: "user".to_string(),
            password: "secret".to_string(),
            tls: TlsMode::Implicit,
            smtp: None,
            provider: None,
            provider_config: None,
            access_token: None,
            refresh_token: None,
            token_expires_at: None,
        }
    }

    fn module(response: &str) -> String {
        let escaped = response.replace('"', "\\22");
        format!(
            r#"(module
                (memory (export "memory") 1)
                (data (i32.const 1024) "{escaped}")
                (func (export "kimo_abi_version") (result i32) i32.const 1)
                (func (export "kimo_alloc") (param i32) (result i32) i32.const 4096)
                (func (export "kimo_free") (param i32 i32))
                (func (export "kimo_invoke") (param i32 i32) (result i64)
                    i64.const 1024
                    i64.const 32
                    i64.shl
                    i64.const {}
                    i64.or))"#,
            response.len()
        )
    }

    fn capabilities() -> Capabilities {
        Capabilities {
            folders: true,
            server_flags: true,
            append: true,
            stored_uids: false,
        }
    }

    #[test]
    fn a_plugin_can_answer_the_provider_trait() {
        // The describe response is what the capabilities come from now, so
        // the fixture has to say both: the list, and what it can do.
        let manifest: crate::plugin::abi::Manifest = serde_json::from_value(serde_json::json!({
            "schema": 1,
            "id": "org.example.provider",
            "name": "Example",
            "version": "0.1.0",
            "abi": 1,
            "module": "example.wasm",
            "permissions": [],
            "providers": [{ "id": "example" }]
        }))
        .unwrap();
        let describe = r#"{"ok":true,"result":{"capabilities":[{"name":"mailbox.list"},{"name":"message.set_flags"},{"name":"message.append"}]}}"#;
        let plugin = WasmPlugin::load(manifest, module(describe).as_bytes()).unwrap();
        let provider = WasmProvider::open(
            &plugin,
            &account(),
            Arc::new(crate::plugin::host::DeniedNetwork),
        )
        .unwrap();
        assert_eq!(provider.capabilities(), capabilities());
    }

    #[test]
    fn the_real_modules_declare_what_the_send_and_flag_gates_ask() {
        // The submission modules file their own Sent copy; a module with no
        // descriptor answer says no, which is the gate's safe default.
        let gmail =
            crate::plugin::abi::Manifest::parse(include_str!("../../plugins/gmail/plugin.json"))
                .unwrap();
        let gmail =
            WasmPlugin::load(gmail, include_bytes!("../../plugins/gmail/gmail.wasm")).unwrap();
        assert!(declares_in(&gmail, "send.save_copy"));
        assert!(declares_in(&gmail, "message.append"));
        assert!(declares_in(&gmail, "message.set_flags"));
        assert!(!declares_in(&gmail, "sync.stored_uids"));

        let outlook =
            crate::plugin::abi::Manifest::parse(include_str!("../../plugins/outlook/plugin.json"))
                .unwrap();
        let outlook = WasmPlugin::load(
            outlook,
            include_bytes!("../../plugins/outlook/outlook.wasm"),
        )
        .unwrap();
        assert!(declares_in(&outlook, "send.save_copy"));
        assert!(declares_in(&outlook, "sync.stored_uids"));
        assert!(declares_in(&outlook, "message.set_flags"));
        assert!(!declares_in(&outlook, "message.append"));
    }
}
