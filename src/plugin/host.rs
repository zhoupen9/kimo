//! The WASM host: module validation, permissions, memory and the JSON ABI.
//!
//! The host is intentionally the only side that owns execution. A plugin is
//! a `.wasm` module in a `wasmi` interpreter; it has no filesystem, network,
//! database or UI handles. Network access is mediated by `HostNetwork`, and
//! every import is checked against the manifest before instantiation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use wasmi::{
    AsContext, Caller, Config, Engine, Extern, ExternType, Linker, Memory, Module, Store, TypedFunc,
};
use wasmi_core::{LimiterError, ResourceLimiter};

use super::abi::{Manifest, NetworkRule, Permissions, PluginRequest, PluginResponse, ABI_VERSION};
use super::extension::{DisplayPlan, OrganizerPlan};

const PAGE_BYTES: usize = 64 * 1024;
const MAX_BLOBS: usize = 1_024;
const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;

/// A host-provided network capability. The default denies everything; a
/// future account layer can supply a real HTTP/TLS backend without changing
/// the WASM ABI.
pub trait HostNetwork: Send + Sync {
    fn http(&self, _request: &[u8]) -> Result<Vec<u8>, String> {
        Err("the host has no HTTP backend".to_string())
    }

    fn open(&self, _host: &str, _port: u16, _mode: u32) -> Result<u32, String> {
        Err("the host has no TLS backend".to_string())
    }

    fn starttls(&self, _id: u32) -> Result<(), String> {
        Err("the host has no TLS backend".to_string())
    }

    fn read(&self, _id: u32, _len: usize) -> Result<Vec<u8>, String> {
        Err("the host has no TLS backend".to_string())
    }

    fn write(&self, _id: u32, _bytes: &[u8]) -> Result<usize, String> {
        Err("the host has no TLS backend".to_string())
    }

    fn close(&self, _id: u32) {}
}

/// The only backend this first host build installs. A missing capability is a
/// denial, not a silent fallback.
#[derive(Default)]
pub struct DeniedNetwork;

impl HostNetwork for DeniedNetwork {}

/// Resource caps for one instance. The defaults mirror the specification:
/// 64 MiB for an extension, 128 MiB for a provider.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub fuel: u64,
    pub memory_bytes: usize,
    pub request_bytes: usize,
    pub response_bytes: usize,
    pub max_effects: usize,
}

impl Limits {
    pub fn for_manifest(manifest: &Manifest) -> Self {
        Self {
            // A provider's one invoke is a whole IMAP session's worth of
            // interpreted parsing — a folder sync base64-encodes every body —
            // so its instruction budget is an order above an extension
            // hook's, which answers with a small plan.
            fuel: if manifest.is_extension_only() {
                50_000_000
            } else {
                500_000_000
            },
            memory_bytes: if manifest.is_extension_only() {
                64 * 1024 * 1024
            } else {
                128 * 1024 * 1024
            },
            request_bytes: 32 * 1024 * 1024,
            response_bytes: 32 * 1024 * 1024,
            max_effects: 10_000,
        }
    }
}

/// A validated, compiled module. Instances are created per contribution so a
/// provider's long call cannot share a store with an extension hook.
pub struct WasmPlugin {
    permissions: Permissions,
    limits: Limits,
    engine: Engine,
    module: Module,
    allowed: Vec<NetworkRule>,
}

impl WasmPlugin {
    pub fn load(manifest: Manifest, bytes: &[u8]) -> Result<Self, HostError> {
        let permissions = manifest
            .validate()
            .map_err(|error| HostError::Manifest(error.to_string()))?;
        let limits = Limits::for_manifest(&manifest);

        let mut config = Config::default();
        config.consume_fuel(true);
        config.wasm_memory64(false);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, bytes).map_err(HostError::wasm)?;

        validate_module(&module, permissions, limits.memory_bytes)?;

        Ok(Self {
            permissions,
            limits,
            engine,
            module,
            allowed: manifest.network,
        })
    }

    pub fn permissions(&self) -> Permissions {
        self.permissions
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Instantiate with the deny-everything backend: the startup probe and the
    /// extension hooks. A provider account passes its own `StdNetwork`.
    pub fn instantiate(&self) -> Result<PluginInstance, HostError> {
        self.instantiate_with(Arc::new(DeniedNetwork))
    }

    /// Instantiate with the caller's network backend. The backend is per
    /// instance, not per module: discovery compiles once with no network at
    /// all, and an account that opens the provider hands it the real one.
    pub fn instantiate_with(
        &self,
        network: Arc<dyn HostNetwork>,
    ) -> Result<PluginInstance, HostError> {
        let state = HostState {
            permissions: self.permissions,
            logs: Vec::new(),
            last_error: None,
            cancel: Arc::new(AtomicBool::new(false)),
            blobs: HashMap::new(),
            next_blob: 1,
            network,
            allowed: self.allowed.clone(),
            limiter: MemoryLimiter::new(self.limits.memory_bytes),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limiter);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|error| HostError::Wasm(error.to_string()))?;

        let mut linker = Linker::new(&self.engine);
        define_host_functions(&mut linker)?;
        let instance = linker
            .instantiate_and_start(&mut store, &self.module)
            .map_err(HostError::wasm)?;

        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| HostError::Module("module does not export memory".to_string()))?;
        let abi = instance
            .get_typed_func::<(), u32>(&store, "kimo_abi_version")
            .map_err(HostError::wasm)?;
        let found = abi
            .call(&mut store, ())
            .map_err(|error| HostError::Trap(error.to_string()))?;
        if found != ABI_VERSION {
            return Err(HostError::Module(format!(
                "module speaks ABI {found}, host speaks {ABI_VERSION}"
            )));
        }
        let alloc = instance
            .get_typed_func::<u32, u32>(&store, "kimo_alloc")
            .map_err(HostError::wasm)?;
        let free = instance
            .get_typed_func::<(u32, u32), ()>(&store, "kimo_free")
            .map_err(HostError::wasm)?;
        let invoke = instance
            .get_typed_func::<(u32, u32), u64>(&store, "kimo_invoke")
            .map_err(HostError::wasm)?;

        Ok(PluginInstance {
            store,
            memory,
            alloc,
            free,
            invoke,
            limits: self.limits,
        })
    }
}

/// One instantiated module. It is single-threaded and must be called one
/// operation at a time, which is exactly the host's scheduling rule.
pub struct PluginInstance {
    store: Store<HostState>,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    free: TypedFunc<(u32, u32), ()>,
    invoke: TypedFunc<(u32, u32), u64>,
    limits: Limits,
}

impl PluginInstance {
    pub fn invoke(&mut self, request: &PluginRequest) -> Result<PluginResponse, HostError> {
        let bytes =
            serde_json::to_vec(request).map_err(|error| HostError::Json(error.to_string()))?;
        if bytes.len() > self.limits.request_bytes {
            return Err(HostError::Limit(format!(
                "request is {} bytes, limit is {}",
                bytes.len(),
                self.limits.request_bytes
            )));
        }
        self.store
            .set_fuel(self.limits.fuel)
            .map_err(|error| HostError::Wasm(error.to_string()))?;

        let request_ptr = self
            .alloc
            .call(&mut self.store, bytes.len() as u32)
            .map_err(|error| HostError::Trap(error.to_string()))?;
        if request_ptr == 0 {
            return Err(HostError::Protocol(
                "kimo_alloc returned a null pointer".to_string(),
            ));
        }
        self.memory
            .write(&mut self.store, request_ptr as usize, &bytes)
            .map_err(|error| HostError::Memory(error.to_string()))?;

        let packed = self
            .invoke
            .call(&mut self.store, (request_ptr, bytes.len() as u32))
            .map_err(|error| HostError::Trap(error.to_string()))?;
        let response_ptr = (packed >> 32) as u32;
        let response_len = packed as u32;
        if response_ptr == 0 || response_len == 0 {
            return Err(HostError::Protocol(
                "module returned an empty response".to_string(),
            ));
        }
        if response_len as usize > self.limits.response_bytes {
            return Err(HostError::Limit(format!(
                "response is {response_len} bytes, limit is {}",
                self.limits.response_bytes
            )));
        }

        let mut response = vec![0; response_len as usize];
        self.memory
            .read(&self.store, response_ptr as usize, &mut response)
            .map_err(|error| HostError::Memory(error.to_string()))?;
        let _ = self
            .free
            .call(&mut self.store, (request_ptr, bytes.len() as u32));
        let _ = self
            .free
            .call(&mut self.store, (response_ptr, response_len));

        let parsed: PluginResponse = serde_json::from_slice(&response)
            .map_err(|error| HostError::Json(error.to_string()))?;
        if parsed.effects.len() > self.limits.max_effects {
            return Err(HostError::Limit(format!(
                "{} effects, limit is {}",
                parsed.effects.len(),
                self.limits.max_effects
            )));
        }
        Ok(parsed)
    }

    /// Invoke an extension's organizer hook and validate its plan before the
    /// UI can see it. The plan is the result value, not the whole envelope.
    #[allow(dead_code)] // The organizer hook has no UI worker yet.
    pub fn organizer(
        &mut self,
        request: &PluginRequest,
        message_ids: &[String],
    ) -> Result<OrganizerPlan, HostError> {
        let plan = self.extension_result::<OrganizerPlan>(request)?;
        plan.validate(message_ids).map_err(HostError::Protocol)?;
        Ok(plan)
    }

    /// Invoke an extension's display hook and validate the declarative plan.
    #[allow(dead_code)] // The display hook has no UI worker yet.
    pub fn display(&mut self, request: &PluginRequest) -> Result<DisplayPlan, HostError> {
        let plan = self.extension_result::<DisplayPlan>(request)?;
        plan.validate().map_err(HostError::Protocol)?;
        Ok(plan)
    }

    #[allow(dead_code)] // With the two hooks above.
    fn extension_result<T: serde::de::DeserializeOwned>(
        &mut self,
        request: &PluginRequest,
    ) -> Result<T, HostError> {
        let response = self.invoke(request)?;
        if !response.ok {
            let message = response
                .error
                .map(|error| format!("{}: {}", error.code, error.message))
                .unwrap_or_else(|| "extension returned an error".to_string());
            return Err(HostError::Protocol(message));
        }
        serde_json::from_value(response.result).map_err(|error| HostError::Json(error.to_string()))
    }

    #[allow(dead_code)] // The account layer reads these when it surfaces a module's error.
    pub fn logs(&self) -> &[LogLine] {
        &self.store.data().logs
    }

    #[allow(dead_code)] // With `logs` above.
    pub fn last_error(&self) -> Option<&str> {
        self.store.data().last_error.as_deref()
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // Read through `PluginInstance::logs`.
pub struct LogLine {
    pub level: i32,
    pub message: String,
}

struct HostState {
    permissions: Permissions,
    logs: Vec<LogLine>,
    last_error: Option<String>,
    cancel: Arc<AtomicBool>,
    blobs: HashMap<u64, Vec<u8>>,
    next_blob: u64,
    network: Arc<dyn HostNetwork>,
    allowed: Vec<NetworkRule>,
    limiter: MemoryLimiter,
}

struct MemoryLimiter {
    max_bytes: usize,
}

impl MemoryLimiter {
    fn new(max_bytes: usize) -> Self {
        Self { max_bytes }
    }
}

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, LimiterError> {
        Ok(desired <= self.max_bytes)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, LimiterError> {
        Ok(desired <= 1_024)
    }

    fn instances(&self) -> usize {
        1
    }

    fn tables(&self) -> usize {
        1
    }

    fn memories(&self) -> usize {
        1
    }
}

fn validate_module(
    module: &Module,
    permissions: Permissions,
    memory_bytes: usize,
) -> Result<(), HostError> {
    for import in module.imports() {
        if import.module() != "kimo" {
            return Err(HostError::Permission(format!(
                "module imports forbidden namespace {:?}",
                import.module()
            )));
        }
        if !permissions.allows_host_import(import.name()) {
            return Err(HostError::Permission(format!(
                "module imports {:?} without its permission",
                import.name()
            )));
        }
    }

    let required = [
        "memory",
        "kimo_abi_version",
        "kimo_alloc",
        "kimo_free",
        "kimo_invoke",
    ];
    for name in required {
        if !module.exports().any(|export| export.name() == name) {
            return Err(HostError::Module(format!("module does not export {name}")));
        }
    }

    if let Some(export) = module.exports().find(|export| export.name() == "memory") {
        if let ExternType::Memory(ty) = export.ty() {
            let minimum = ty.minimum() * PAGE_BYTES as u64;
            if minimum > memory_bytes as u64 {
                return Err(HostError::Limit(format!(
                    "module memory starts at {minimum} bytes, cap is {memory_bytes}"
                )));
            }
        }
    }
    Ok(())
}

macro_rules! link {
    ($name:expr, $func:expr, $linker:expr) => {
        $linker
            .func_wrap("kimo", $name, $func)
            .map_err(|error| HostError::Link(error.to_string()))
            .map(|_| ())
    };
}

fn define_host_functions(linker: &mut Linker<HostState>) -> Result<(), HostError> {
    link!(
        "log",
        |mut caller: Caller<'_, HostState>, level: i32, ptr: i32, len: i32| {
            if let Ok(bytes) = read_memory(&caller, ptr, len) {
                caller.data_mut().logs.push(LogLine {
                    level,
                    message: String::from_utf8_lossy(&bytes).into_owned(),
                });
            }
        },
        linker
    )?;
    link!(
        "now_ms",
        |_caller: Caller<'_, HostState>| -> i64 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_millis() as i64)
        },
        linker
    )?;
    link!(
        "cancelled",
        |caller: Caller<'_, HostState>| -> i32 {
            i32::from(caller.data().cancel.load(Ordering::Relaxed))
        },
        linker
    )?;
    link!(
        "random",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
            if ptr < 0 || len < 0 {
                return -1;
            }
            let mut bytes = vec![0; len as usize];
            let provider = rustls::crypto::ring::default_provider();
            if provider.secure_random.fill(&mut bytes).is_err() {
                caller.data_mut().last_error = Some("CSPRNG failed".to_string());
                return -1;
            }
            match memory(&caller)
                .and_then(|memory| memory.write(&mut caller, ptr as usize, &bytes).ok())
            {
                Some(()) => len,
                None => -1,
            }
        },
        linker
    )?;

    link!(
        "http_request",
        |mut caller: Caller<'_, HostState>,
         request_ptr: i32,
         request_len: i32,
         out_ptr: i32,
         out_cap: i32|
         -> i32 {
            if !caller.data().permissions.net_http {
                return deny(&mut caller, "net.http is not granted");
            }
            if out_ptr < 0 || out_cap < 0 {
                return deny(&mut caller, "invalid output buffer");
            }
            let request = match read_memory(&caller, request_ptr, request_len) {
                Ok(request) => request,
                Err(error) => return deny(&mut caller, &error.to_string()),
            };
            let allowed =
                http_url(&request).is_some_and(|url| http_allowed(&caller.data().allowed, &url));
            if !allowed {
                return deny(&mut caller, "endpoint is not allowed");
            }
            let network = Arc::clone(&caller.data().network);
            match network.http(&request) {
                Ok(response) if response.len() <= out_cap as usize => {
                    match memory(&caller).and_then(|memory| {
                        memory.write(&mut caller, out_ptr as usize, &response).ok()
                    }) {
                        Some(()) => response.len() as i32,
                        None => deny(&mut caller, "response does not fit"),
                    }
                }
                Ok(response) => deny(
                    &mut caller,
                    &format!("response is {} bytes, buffer is {out_cap}", response.len()),
                ),
                Err(error) => deny(&mut caller, &error),
            }
        },
        linker
    )?;

    link!(
        "net_open",
        |mut caller: Caller<'_, HostState>,
         host_ptr: i32,
         host_len: i32,
         port: i32,
         mode: i32|
         -> i32 {
            if !caller.data().permissions.net_tls {
                return deny(&mut caller, "net.tls is not granted");
            }
            let host = match read_memory(&caller, host_ptr, host_len) {
                Ok(host) => match String::from_utf8(host) {
                    Ok(host) => host,
                    Err(_) => return deny(&mut caller, "host is not UTF-8"),
                },
                Err(error) => return deny(&mut caller, &error.to_string()),
            };
            if !(0..=u16::MAX as i32).contains(&port) || mode < 0 {
                return deny(&mut caller, "invalid endpoint");
            }
            if !net_allowed(&caller.data().allowed, &host, port as u16) {
                return deny(&mut caller, "endpoint is not allowed");
            }
            let network = Arc::clone(&caller.data().network);
            match network.open(&host, port as u16, mode as u32) {
                Ok(id) => id as i32,
                Err(error) => deny(&mut caller, &error),
            }
        },
        linker
    )?;

    link!(
        "net_starttls",
        |mut caller: Caller<'_, HostState>, id: i32| -> i32 {
            if !caller.data().permissions.net_tls {
                return deny(&mut caller, "net.tls is not granted");
            }
            let network = Arc::clone(&caller.data().network);
            match network.starttls(id as u32) {
                Ok(()) => 0,
                Err(error) => deny(&mut caller, &error),
            }
        },
        linker
    )?;

    link!(
        "net_read",
        |mut caller: Caller<'_, HostState>, id: i32, ptr: i32, len: i32| -> i32 {
            if !caller.data().permissions.net_tls {
                return deny(&mut caller, "net.tls is not granted");
            }
            if ptr < 0 || len < 0 {
                return deny(&mut caller, "invalid buffer");
            }
            let network = Arc::clone(&caller.data().network);
            match network.read(id as u32, len as usize) {
                Ok(bytes) if bytes.len() <= len as usize => {
                    match memory(&caller)
                        .and_then(|memory| memory.write(&mut caller, ptr as usize, &bytes).ok())
                    {
                        Some(()) => bytes.len() as i32,
                        None => deny(&mut caller, "read does not fit"),
                    }
                }
                Ok(_) => deny(&mut caller, "read returned more bytes than requested"),
                Err(error) => deny(&mut caller, &error),
            }
        },
        linker
    )?;

    link!(
        "net_write",
        |mut caller: Caller<'_, HostState>, id: i32, ptr: i32, len: i32| -> i32 {
            if !caller.data().permissions.net_tls {
                return deny(&mut caller, "net.tls is not granted");
            }
            let bytes = match read_memory(&caller, ptr, len) {
                Ok(bytes) => bytes,
                Err(error) => return deny(&mut caller, &error.to_string()),
            };
            let network = Arc::clone(&caller.data().network);
            match network.write(id as u32, &bytes) {
                Ok(written) if written <= bytes.len() => written as i32,
                Ok(_) => deny(&mut caller, "write returned too many bytes"),
                Err(error) => deny(&mut caller, &error),
            }
        },
        linker
    )?;

    link!(
        "net_close",
        |caller: Caller<'_, HostState>, id: i32| {
            if caller.data().permissions.net_tls {
                let network = Arc::clone(&caller.data().network);
                network.close(id as u32);
            }
        },
        linker
    )?;

    link!(
        "blob_write",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
            if len < 0 || len as usize > MAX_BLOB_BYTES {
                return 0;
            }
            let bytes = match read_memory(&caller, ptr, len) {
                Ok(bytes) => bytes,
                Err(_) => return 0,
            };
            let state = caller.data_mut();
            if state.blobs.len() >= MAX_BLOBS {
                state.last_error = Some("blob limit reached".to_string());
                return 0;
            }
            let id = state.next_blob;
            state.next_blob += 1;
            state.blobs.insert(id, bytes);
            id as i64
        },
        linker
    )?;

    link!(
        "blob_read",
        |mut caller: Caller<'_, HostState>, id: i64, offset: i32, ptr: i32, len: i32| -> i32 {
            if offset < 0 || ptr < 0 || len < 0 {
                return -1;
            }
            let bytes = match caller.data().blobs.get(&(id as u64)).cloned() {
                Some(bytes) => bytes,
                None => return -1,
            };
            let start = offset as usize;
            if start >= bytes.len() {
                return 0;
            }
            let end = (start + len as usize).min(bytes.len());
            let slice = &bytes[start..end];
            match memory(&caller)
                .and_then(|memory| memory.write(&mut caller, ptr as usize, slice).ok())
            {
                Some(()) => slice.len() as i32,
                None => -1,
            }
        },
        linker
    )?;

    link!(
        "blob_free",
        |mut caller: Caller<'_, HostState>, id: i64| {
            caller.data_mut().blobs.remove(&(id as u64));
        },
        linker
    )?;

    Ok(())
}

fn http_url(request: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(request).ok()?;
    value.get("url")?.as_str().map(str::to_string)
}

fn http_allowed(rules: &[NetworkRule], url: &str) -> bool {
    let Some((scheme, authority)) = url.split_once("://") else {
        return false;
    };
    let authority = authority.split(['/', '?', '#']).next().unwrap_or(authority);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => {
            (host, port.parse::<u16>().ok())
        }
        _ => (authority, None),
    };
    let port = port.unwrap_or(if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    });
    rules.iter().any(|rule| {
        rule.transport.eq_ignore_ascii_case("http") || rule.transport.eq_ignore_ascii_case("https")
    } && host_matches(&rule.host, host)
        && (rule.port.is_none() || rule.port == Some(port))
    )
}

fn net_allowed(rules: &[NetworkRule], host: &str, port: u16) -> bool {
    rules.iter().any(|rule| {
        rule.transport.eq_ignore_ascii_case("tls")
            && host_matches(&rule.host, host)
            && (rule.port.is_none() || rule.port == Some(port))
    })
}

fn host_matches(pattern: &str, host: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("*.") {
        host.len() > suffix.len()
            && host.ends_with(suffix)
            && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
    } else {
        pattern.eq_ignore_ascii_case(host)
    }
}

fn deny(caller: &mut Caller<'_, HostState>, message: &str) -> i32 {
    caller.data_mut().last_error = Some(message.to_string());
    -1
}

fn memory(caller: &Caller<'_, HostState>) -> Option<Memory> {
    caller.get_export("memory").and_then(Extern::into_memory)
}

fn read_memory(caller: &Caller<'_, HostState>, ptr: i32, len: i32) -> Result<Vec<u8>, HostError> {
    if ptr < 0 || len < 0 {
        return Err(HostError::Memory("negative pointer or length".to_string()));
    }
    let memory = memory(caller)
        .ok_or_else(|| HostError::Memory("module has no memory export".to_string()))?;
    let mut bytes = vec![0; len as usize];
    memory
        .read(caller.as_context(), ptr as usize, &mut bytes)
        .map_err(|error| HostError::Memory(error.to_string()))?;
    Ok(bytes)
}

#[derive(Debug)]
pub enum HostError {
    Manifest(String),
    Module(String),
    Permission(String),
    Link(String),
    Json(String),
    Wasm(String),
    Trap(String),
    Memory(String),
    Limit(String),
    Protocol(String),
}

impl HostError {
    fn wasm(error: wasmi::Error) -> Self {
        Self::Wasm(error.to_string())
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manifest(message)
            | Self::Module(message)
            | Self::Permission(message)
            | Self::Link(message)
            | Self::Json(message)
            | Self::Wasm(message)
            | Self::Trap(message)
            | Self::Memory(message)
            | Self::Limit(message)
            | Self::Protocol(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for HostError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::abi::Contribution;

    fn manifest(providers: bool, permissions: &[&str]) -> Manifest {
        Manifest {
            schema: 1,
            id: "org.example.test".to_string(),
            name: "Test".to_string(),
            version: "0.1.0".to_string(),
            abi: ABI_VERSION,
            module: "test.wasm".to_string(),
            default_enabled: false,
            permissions: permissions.iter().map(|p| (*p).to_string()).collect(),
            providers: if providers {
                vec![Contribution {
                    id: "test".to_string(),
                    name: None,
                }]
            } else {
                Vec::new()
            },
            extensions: if providers {
                Vec::new()
            } else {
                vec![Contribution {
                    id: "test".to_string(),
                    name: None,
                }]
            },
            network: Vec::new(),
        }
    }

    fn response_module(response: &str) -> String {
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

    fn echo_module() -> String {
        response_module(r#"{"ok":true,"result":{"pong":true}}"#)
    }

    fn http_module(request: &str) -> String {
        let response = r#"{"ok":true,"result":null}"#;
        let request_len = request.len();
        let response_len = response.len();
        let request = request.replace('"', "\\22");
        let response = response.replace('"', "\\22");
        format!(
            r#"(module
                (import "kimo" "http_request" (func $http (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 1024) "{response}")
                (data (i32.const 2000) "{request}")
                (func (export "kimo_abi_version") (result i32) i32.const 1)
                (func (export "kimo_alloc") (param i32) (result i32) i32.const 4096)
                (func (export "kimo_free") (param i32 i32))
                (func (export "kimo_invoke") (param i32 i32) (result i64)
                    i32.const 2000
                    i32.const {}
                    i32.const 3000
                    i32.const 16
                    call $http
                    drop
                    i64.const 1024
                    i64.const 32
                    i64.shl
                    i64.const {}
                    i64.or))"#,
            request_len, response_len
        )
    }

    fn loop_module() -> &'static str {
        r#"(module
            (memory (export "memory") 1)
            (func (export "kimo_abi_version") (result i32) i32.const 1)
            (func (export "kimo_alloc") (param i32) (result i32) i32.const 4096)
            (func (export "kimo_free") (param i32 i32))
            (func (export "kimo_invoke") (param i32 i32) (result i64)
                (loop br 0)
                i64.const 0))"#
    }

    #[test]
    fn invokes_the_json_abi() {
        let plugin = WasmPlugin::load(manifest(true, &[]), echo_module().as_bytes()).unwrap();
        let mut instance = plugin.instantiate().unwrap();
        let response = instance
            .invoke(&PluginRequest::operation(
                "provider.invoke",
                "provider.describe",
            ))
            .unwrap();
        assert!(response.ok);
        assert_eq!(response.result["pong"], serde_json::json!(true));
    }

    #[test]
    fn rejects_a_network_import_without_its_permission() {
        let module = r#"(module
            (import "kimo" "http_request" (func (param i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "kimo_abi_version") (result i32) i32.const 1)
            (func (export "kimo_alloc") (param i32) (result i32) i32.const 4096)
            (func (export "kimo_free") (param i32 i32))
            (func (export "kimo_invoke") (param i32 i32) (result i64) i64.const 0))"#;
        let error = match WasmPlugin::load(manifest(true, &[]), module.as_bytes()) {
            Ok(_) => panic!("a network import without permission was accepted"),
            Err(error) => error,
        };
        assert!(matches!(error, HostError::Permission(_)));
    }

    #[test]
    fn host_logs_reach_the_instance() {
        let response = r#"{"ok":true,"result":null}"#;
        let escaped = response.replace('"', "\\22");
        let module = format!(
            r#"(module
                (import "kimo" "log" (func $log (param i32 i32 i32)))
                (memory (export "memory") 1)
                (data (i32.const 1024) "{escaped}")
                (data (i32.const 2000) "hello")
                (func (export "kimo_abi_version") (result i32) i32.const 1)
                (func (export "kimo_alloc") (param i32) (result i32) i32.const 4096)
                (func (export "kimo_free") (param i32 i32))
                (func (export "kimo_invoke") (param i32 i32) (result i64)
                    i32.const 0
                    i32.const 2000
                    i32.const 5
                    call $log
                    i64.const 1024
                    i64.const 32
                    i64.shl
                    i64.const {}
                    i64.or))"#,
            response.len()
        );
        let plugin = WasmPlugin::load(manifest(true, &[]), module.as_bytes()).unwrap();
        let mut instance = plugin.instantiate().unwrap();
        instance
            .invoke(&PluginRequest::new("provider.describe"))
            .unwrap();
        assert_eq!(instance.logs().len(), 1);
        assert_eq!(instance.logs()[0].level, 0);
        assert_eq!(instance.logs()[0].message, "hello");
    }

    #[test]
    fn organizer_plan_is_validated_before_it_reaches_the_list() {
        let response =
            r#"{"ok":true,"result":{"groups":[{"id":"g","title":"Group","message_ids":["m1"]}]}}"#;
        let plugin =
            WasmPlugin::load(manifest(false, &[]), response_module(response).as_bytes()).unwrap();
        let mut instance = plugin.instantiate().unwrap();
        let plan = instance
            .organizer(&PluginRequest::new("extension.invoke"), &["m1".to_string()])
            .unwrap();
        assert_eq!(plan.groups.len(), 1);
    }

    #[test]
    fn display_plan_is_validated_before_it_reaches_the_pane() {
        let response = r#"{"ok":true,"result":{"content":{"mode":"keep"}}}"#;
        let plugin =
            WasmPlugin::load(manifest(false, &[]), response_module(response).as_bytes()).unwrap();
        let mut instance = plugin.instantiate().unwrap();
        let plan = instance
            .display(&PluginRequest::new("extension.invoke"))
            .unwrap();
        assert!(plan.content.is_some());
    }

    #[test]
    fn the_sender_groups_sample_is_a_loadable_extension() {
        let manifest = crate::plugin::abi::Manifest::parse(include_str!(
            "../../plugins/sender-groups/plugin.json"
        ))
        .unwrap();
        let plugin = WasmPlugin::load(
            manifest,
            include_bytes!("../../plugins/sender-groups/sender-groups.wasm"),
        )
        .unwrap();
        let mut instance = plugin.instantiate().unwrap();
        let plan = instance
            .organizer(&PluginRequest::new("extension.invoke"), &["m1".to_string()])
            .unwrap();
        assert_eq!(plan.groups[0].title, "All mail");
    }

    #[test]
    fn http_is_denied_without_a_network_rule() {
        let request = r#"{"method":"GET","url":"https://example.com/"}"#;
        let plugin = WasmPlugin::load(
            manifest(true, &["net.http"]),
            http_module(request).as_bytes(),
        )
        .unwrap();
        let mut instance = plugin.instantiate().unwrap();
        instance
            .invoke(&PluginRequest::new("provider.describe"))
            .unwrap();
        assert!(instance.last_error().unwrap().contains("not allowed"));
    }

    #[test]
    fn http_reaches_the_backend_only_after_the_rule_matches() {
        let request = r#"{"method":"GET","url":"https://example.com/"}"#;
        let mut manifest = manifest(true, &["net.http"]);
        manifest.network = vec![crate::plugin::abi::NetworkRule {
            transport: "http".to_string(),
            host: "example.com".to_string(),
            port: None,
        }];
        let plugin = WasmPlugin::load(manifest, http_module(request).as_bytes()).unwrap();
        let mut instance = plugin.instantiate().unwrap();
        instance
            .invoke(&PluginRequest::new("provider.describe"))
            .unwrap();
        assert!(instance.last_error().unwrap().contains("no HTTP backend"));
    }

    #[test]
    fn traps_a_runaway_module() {
        let plugin = WasmPlugin::load(manifest(true, &[]), loop_module().as_bytes()).unwrap();
        let mut instance = plugin.instantiate().unwrap();
        let error = instance
            .invoke(&PluginRequest::new("provider.describe"))
            .unwrap_err();
        assert!(matches!(error, HostError::Trap(_)));
    }
}
