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
    DeleteOutcome, FetchKind, FolderBatch, FolderMark, MailProvider, OlderBatch,
};

use super::abi::PluginRequest;
use super::host::{HostError, HostNetwork, PluginInstance, WasmPlugin};

/// One plugin-backed mailbox session. The instance is single-threaded, just
/// like the existing adapters; `sync.rs` calls it from a worker only.
pub struct WasmProvider {
    instance: PluginInstance,
    account: Account,
    capabilities: crate::provider::Capabilities,
    secrets: bool,
}

impl WasmProvider {
    pub fn open(
        plugin: &WasmPlugin,
        account: &Account,
        capabilities: crate::provider::Capabilities,
        network: Arc<dyn HostNetwork>,
    ) -> Result<Self, HostError> {
        let instance = plugin.instantiate_with(network)?;
        Ok(Self {
            instance,
            account: account.clone(),
            capabilities,
            secrets: plugin.permissions().secrets,
        })
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
            request.credentials = Some(json!({
                "kind": "password",
                "username": self.account.username,
                "password": self.account.password,
            }));
        }
        request.params = params;
        let response = self.instance.invoke(&request).map_err(host_error)?;
        if !response.ok {
            let message = response
                .error
                .map(|error| format!("{}: {}", error.code, error.message))
                .unwrap_or_else(|| "plugin returned an error".to_string());
            return Err(MailError::Protocol(message));
        }
        Ok(response.result)
    }
}

fn host_error(error: HostError) -> MailError {
    MailError::Protocol(error.to_string())
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
        let plugin = WasmPlugin::load(
            manifest,
            module(r#"{"ok":true,"result":["INBOX"]}"#).as_bytes(),
        )
        .unwrap();
        let mut provider = WasmProvider::open(
            &plugin,
            &account(),
            capabilities(),
            Arc::new(crate::plugin::host::DeniedNetwork),
        )
        .unwrap();
        assert_eq!(provider.folders().unwrap(), vec!["INBOX"]);
    }
}
