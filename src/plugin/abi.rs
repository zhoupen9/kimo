//! The JSON contract shared by Kimo and a WASM plugin.
//!
//! The ABI is deliberately a small, versioned JSON envelope rather than a
//! native struct: a `.wasm` module has no Rust ABI to stabilise, and the
//! host validates the module before it runs. The types here are the common
//! vocabulary for provider operations and extension plans; a plugin may add
//! fields, but the host rejects an unknown operation or effect variant.

#![allow(dead_code)] // The complete contract is published before every field has a caller.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The only ABI this build knows. A module with another major version is
/// rejected whole.
pub const ABI_VERSION: u32 = 1;

/// The manifest every plugin directory must carry.
pub const MANIFEST_FILE: &str = "plugin.json";

/// A request to a plugin instance. The host copies it into the module's
/// linear memory as UTF-8 JSON and reads the response back the same way.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginRequest {
    pub abi: u32,
    pub method: String,
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub operation: Option<String>,
    #[serde(default)]
    pub account: Option<Value>,
    #[serde(default)]
    pub credentials: Option<Value>,
    #[serde(default)]
    pub target: Value,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

impl PluginRequest {
    pub fn new(method: impl Into<String>) -> Self {
        Self {
            abi: ABI_VERSION,
            method: method.into(),
            request_id: String::new(),
            operation: None,
            account: None,
            credentials: None,
            target: Value::Null,
            params: Value::Null,
            deadline_ms: None,
        }
    }

    pub fn operation(method: impl Into<String>, operation: impl Into<String>) -> Self {
        Self {
            operation: Some(operation.into()),
            ..Self::new(method)
        }
    }
}

/// A plugin response. `effects` are local mutations the host may apply; the
/// host validates every one before touching SQLite.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginResponse {
    pub ok: bool,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub effects: Vec<Effect>,
    #[serde(default)]
    pub warnings: Vec<Warning>,
    #[serde(default)]
    pub error: Option<PluginError>,
    #[serde(default)]
    pub credentials: Option<Value>,
}

impl PluginResponse {
    pub fn ok(result: Value) -> Self {
        Self {
            ok: true,
            result,
            effects: Vec::new(),
            warnings: Vec::new(),
            error: None,
            credentials: None,
        }
    }

    pub fn failed(error: PluginError) -> Self {
        Self {
            ok: false,
            result: Value::Null,
            effects: Vec::new(),
            warnings: Vec::new(),
            error: Some(error),
            credentials: None,
        }
    }
}

/// An effect is tagged by `type`; the fields beside the tag are the effect's
/// own JSON. The host keeps the tag set closed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Effect {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub data: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Warning {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub details: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub retry_after_ms: Option<u64>,
    #[serde(default)]
    pub details: Value,
    #[serde(default)]
    pub cause: Option<String>,
}

impl PluginError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            retry_after_ms: None,
            details: Value::Null,
            cause: None,
        }
    }
}

/// What a provider says it can do. The host dispatches from this table rather
/// than guessing from the wire protocol.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub spec_version: String,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub network: Vec<NetworkRule>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capability {
    pub name: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkRule {
    pub transport: String,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
}

/// A plugin directory's `plugin.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub abi: u32,
    pub module: String,
    #[serde(default)]
    pub default_enabled: bool,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub providers: Vec<Contribution>,
    #[serde(default)]
    pub extensions: Vec<Contribution>,
    #[serde(default)]
    pub network: Vec<NetworkRule>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Contribution {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let manifest: Self = serde_json::from_str(text).map_err(ManifestError::Json)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path).map_err(ManifestError::Io)?;
        Self::parse(&text)
    }

    pub fn validate(&self) -> Result<Permissions, ManifestError> {
        if self.schema != 1 {
            return Err(ManifestError::Invalid("schema must be 1".to_string()));
        }
        if self.id.is_empty() || !self.id.chars().all(valid_id_char) {
            return Err(ManifestError::Invalid(
                "id must be a non-empty [A-Za-z0-9._-] identifier".to_string(),
            ));
        }
        if self.name.trim().is_empty() || self.version.trim().is_empty() {
            return Err(ManifestError::Invalid(
                "name and version must not be empty".to_string(),
            ));
        }
        if self.abi != ABI_VERSION {
            return Err(ManifestError::Invalid(format!(
                "abi {} is not supported (host speaks {ABI_VERSION})",
                self.abi
            )));
        }
        let module = Path::new(&self.module);
        if module.extension().and_then(|e| e.to_str()) != Some("wasm") {
            return Err(ManifestError::Invalid(
                "module must name a .wasm file".to_string(),
            ));
        }
        if module.is_absolute()
            || module.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(ManifestError::Invalid(
                "module must be a relative path without ..".to_string(),
            ));
        }
        if self.providers.is_empty() && self.extensions.is_empty() {
            return Err(ManifestError::Invalid(
                "a plugin must contribute a provider or an extension".to_string(),
            ));
        }
        for contribution in self.providers.iter().chain(self.extensions.iter()) {
            if contribution.id.is_empty() || !contribution.id.chars().all(valid_id_char) {
                return Err(ManifestError::Invalid(
                    "contribution ids must be [A-Za-z0-9._-]".to_string(),
                ));
            }
        }
        Permissions::from_names(&self.permissions)
    }

    pub fn module_path(&self, plugin_dir: &Path) -> PathBuf {
        plugin_dir.join(&self.module)
    }

    pub fn is_extension_only(&self) -> bool {
        self.providers.is_empty() && !self.extensions.is_empty()
    }
}

fn valid_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// The three permissions the host enforces. A module importing a host
/// function outside this set fails validation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Permissions {
    pub net_http: bool,
    pub net_tls: bool,
    pub secrets: bool,
}

impl Permissions {
    pub fn from_names(names: &[String]) -> Result<Self, ManifestError> {
        let mut permissions = Self::default();
        let mut seen = BTreeSet::new();
        for name in names {
            if !seen.insert(name.as_str()) {
                return Err(ManifestError::Invalid(format!(
                    "duplicate permission {name}"
                )));
            }
            match name.as_str() {
                "net.http" => permissions.net_http = true,
                "net.tls" => permissions.net_tls = true,
                "secrets" => permissions.secrets = true,
                other => {
                    return Err(ManifestError::Invalid(format!(
                        "unknown permission {other}"
                    )))
                }
            }
        }
        Ok(permissions)
    }

    pub fn names(self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.net_http {
            names.push("net.http");
        }
        if self.net_tls {
            names.push("net.tls");
        }
        if self.secrets {
            names.push("secrets");
        }
        names
    }

    pub fn allows_host_import(self, name: &str) -> bool {
        match name {
            "http_request" => self.net_http,
            "net_open" | "net_starttls" | "net_read" | "net_write" | "net_close" => self.net_tls,
            "log" | "now_ms" | "cancelled" | "random" | "blob_read" | "blob_write"
            | "blob_free" => true,
            _ => false,
        }
    }
}

#[derive(Debug)]
pub enum ManifestError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Invalid(String),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "cannot read manifest: {error}"),
            Self::Json(error) => write!(f, "manifest is not valid JSON: {error}"),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ManifestError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(permissions: &[&str]) -> Manifest {
        Manifest {
            schema: 1,
            id: "org.example.provider".to_string(),
            name: "Example".to_string(),
            version: "0.1.0".to_string(),
            abi: ABI_VERSION,
            module: "example.wasm".to_string(),
            default_enabled: false,
            permissions: permissions.iter().map(|p| (*p).to_string()).collect(),
            providers: vec![Contribution {
                id: "example".to_string(),
                name: None,
            }],
            extensions: Vec::new(),
            network: Vec::new(),
        }
    }

    #[test]
    fn manifest_accepts_the_declared_permissions_only() {
        let parsed = manifest(&["net.http", "secrets"]).validate().unwrap();
        assert!(parsed.net_http);
        assert!(parsed.secrets);
        assert!(!parsed.net_tls);
        assert!(manifest(&["net.tls"]).validate().unwrap().net_tls);
        assert!(manifest(&["filesystem"]).validate().is_err());
        assert!(manifest(&["secrets", "secrets"]).validate().is_err());
    }

    #[test]
    fn manifest_rejects_a_native_or_escaping_module() {
        let mut native = manifest(&[]);
        native.module = "libexample.so".to_string();
        assert!(native.validate().is_err());

        let mut escaping = manifest(&[]);
        escaping.module = "../example.wasm".to_string();
        assert!(escaping.validate().is_err());
    }

    #[test]
    fn host_imports_are_gated_by_permission() {
        let none = Permissions::default();
        assert!(none.allows_host_import("log"));
        assert!(none.allows_host_import("blob_read"));
        assert!(!none.allows_host_import("http_request"));
        assert!(!none.allows_host_import("net_open"));

        let http = Permissions {
            net_http: true,
            ..Permissions::default()
        };
        assert!(http.allows_host_import("http_request"));
        assert!(!http.allows_host_import("net_open"));
    }

    #[test]
    fn descriptor_and_response_shapes_round_trip() {
        let descriptor: ProviderDescriptor = serde_json::from_value(serde_json::json!({
            "id": "example",
            "name": "Example",
            "version": "0.1.0",
            "capabilities": [{ "name": "mailbox.list" }],
            "network": [{ "transport": "tls", "host": "imap.example.com", "port": 993 }]
        }))
        .unwrap();
        assert_eq!(descriptor.capabilities[0].name, "mailbox.list");
        assert_eq!(descriptor.network[0].host, "imap.example.com");

        let ok = PluginResponse::ok(serde_json::json!({ "pong": true }));
        assert!(ok.ok);
        let failed = PluginResponse::failed(PluginError::new("unsupported", "no"));
        assert!(!failed.ok);
        assert_eq!(failed.error.unwrap().code, "unsupported");
    }
}
