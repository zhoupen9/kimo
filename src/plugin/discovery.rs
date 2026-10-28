//! Plugin discovery and startup validation.
//!
//! Discovery is deliberately split from execution: every directory with a
//! `plugin.json` is read and its module is compiled, but nothing is invoked
//! until an account or an extension hook asks for it. A broken plugin is
//! collected with its path and never stops the rest of Kimo from starting.

use std::path::{Path, PathBuf};

use super::abi::{Manifest, MANIFEST_FILE};
use super::host::WasmPlugin;

/// A plugin that passed manifest validation and module compilation.
pub struct LoadedPlugin {
    pub manifest: Manifest,
    pub plugin: WasmPlugin,
}

/// One plugin that did not load, with the path a user can inspect.
#[derive(Debug)]
pub struct DiscoveryError {
    pub path: PathBuf,
    pub message: String,
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

/// The result of one startup scan. The registry owns the compiled modules so
/// a later account/hook layer can instantiate them without recompiling.
pub struct Registry {
    plugins: Vec<LoadedPlugin>,
    disabled: Vec<Manifest>,
    errors: Vec<DiscoveryError>,
}

impl Registry {
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
            disabled: Vec::new(),
            errors: Vec::new(),
        }
    }

    pub fn discover_with_disabled(disabled: &[String]) -> Self {
        Self::discover_from_with_disabled(&default_paths(), disabled)
    }

    #[allow(dead_code)] // Tests build a registry from an explicit directory list.
    pub fn discover_from(paths: &[PathBuf]) -> Self {
        Self::discover_from_with_disabled(paths, &[])
    }

    pub fn discover_from_with_disabled(paths: &[PathBuf], disabled: &[String]) -> Self {
        let mut plugins = Vec::new();
        let mut disabled_plugins = Vec::new();
        let mut errors = Vec::new();
        let mut seen = std::collections::BTreeSet::new();

        for base in paths {
            if !base.is_dir() {
                continue;
            }
            let mut entries = match std::fs::read_dir(base) {
                Ok(entries) => entries.filter_map(Result::ok).collect::<Vec<_>>(),
                Err(error) => {
                    errors.push(DiscoveryError {
                        path: base.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            entries.sort_by_key(|entry| entry.path());

            for entry in entries {
                let directory = entry.path();
                if !directory.is_dir() || !directory.join(MANIFEST_FILE).is_file() {
                    continue;
                }
                let manifest = match Manifest::load(&directory.join(MANIFEST_FILE)) {
                    Ok(manifest) => manifest,
                    Err(error) => {
                        errors.push(DiscoveryError {
                            path: directory,
                            message: error.to_string(),
                        });
                        continue;
                    }
                };
                if !seen.insert(manifest.id.clone()) {
                    errors.push(DiscoveryError {
                        path: directory,
                        message: format!("duplicate plugin id {}", manifest.id),
                    });
                    continue;
                }
                if disabled.iter().any(|id| id == &manifest.id) {
                    disabled_plugins.push(manifest);
                    continue;
                }
                let module_path = manifest.module_path(&directory);
                let bytes = match std::fs::read(&module_path) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        errors.push(DiscoveryError {
                            path: directory,
                            message: format!("{}: {error}", module_path.display()),
                        });
                        continue;
                    }
                };
                let plugin = match WasmPlugin::load(manifest.clone(), &bytes) {
                    Ok(plugin) => plugin,
                    Err(error) => {
                        errors.push(DiscoveryError {
                            path: directory,
                            message: error.to_string(),
                        });
                        continue;
                    }
                };
                if let Err(error) = plugin.instantiate() {
                    errors.push(DiscoveryError {
                        path: directory,
                        message: format!("module cannot be instantiated: {error}"),
                    });
                    continue;
                }
                plugins.push(LoadedPlugin { manifest, plugin });
            }
        }

        Self {
            plugins,
            disabled: disabled_plugins,
            errors,
        }
    }

    pub fn plugins(&self) -> &[LoadedPlugin] {
        &self.plugins
    }

    pub fn disabled(&self) -> &[Manifest] {
        &self.disabled
    }

    pub fn errors(&self) -> &[DiscoveryError] {
        &self.errors
    }

    /// Report startup state without making it a modal or a hard failure.
    /// There is no plugin UI in this first host build, so stderr is the only
    /// place a user can see why a module did not load.
    pub fn log_startup(&self) {
        for error in &self.errors {
            eprintln!("kimo: plugin not loaded: {error}");
        }
        for manifest in &self.disabled {
            eprintln!("kimo: plugin {} disabled by settings", manifest.id);
        }
        for loaded in &self.plugins {
            let permissions = loaded.plugin.permissions().names();
            let permissions = if permissions.is_empty() {
                "none".to_string()
            } else {
                permissions.join(",")
            };
            eprintln!(
                "kimo: plugin {} {} loaded ({}; {} MiB)",
                loaded.manifest.id,
                loaded.manifest.version,
                permissions,
                loaded.plugin.limits().memory_bytes / (1024 * 1024)
            );
        }
    }
}

fn default_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(value) = std::env::var("KIMO_PLUGIN_PATH") {
        paths.extend(
            value
                .split(':')
                .filter(|part| !part.is_empty())
                .map(PathBuf::from),
        );
    }
    if let Some(data) = user_data_dir() {
        paths.push(data.join("kimo/plugins"));
    }
    paths.push(PathBuf::from("/usr/lib/kimo/plugins"));

    let mut seen = std::collections::BTreeSet::new();
    paths.retain(|path| seen.insert(path.clone()));
    paths
}

fn user_data_dir() -> Option<PathBuf> {
    if let Some(data) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(data));
    }
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/share"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_directories_are_not_errors() {
        let registry = Registry::discover_from(&[PathBuf::from("/definitely/not/a/plugin/dir")]);
        assert!(registry.plugins().is_empty());
        assert!(registry.errors().is_empty());
    }

    #[test]
    fn plugin_paths_are_unique_and_ordered() {
        let paths = default_paths();
        let mut unique = paths.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(paths, unique);
    }

    #[test]
    fn the_echo_sample_is_a_loadable_wasm_module() {
        let manifest = Manifest::parse(include_str!("../../plugins/echo/plugin.json")).unwrap();
        let plugin =
            WasmPlugin::load(manifest, include_bytes!("../../plugins/echo/echo.wasm")).unwrap();
        let mut instance = plugin.instantiate().unwrap();
        let response = instance
            .invoke(&crate::plugin::abi::PluginRequest::new("plugin.describe"))
            .unwrap();
        assert_eq!(response.result["provider"], "echo");
    }

    #[test]
    fn the_gmail_sample_describes_and_signs_in_through_the_abi() {
        let manifest = Manifest::parse(include_str!("../../plugins/gmail/plugin.json")).unwrap();
        let plugin =
            WasmPlugin::load(manifest, include_bytes!("../../plugins/gmail/gmail.wasm")).unwrap();
        let mut instance = plugin.instantiate().unwrap();

        let described = instance
            .invoke(&crate::plugin::abi::PluginRequest::new("provider.describe"))
            .unwrap();
        assert!(described.ok);
        let profiles: Vec<&str> = described.result["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|profile| profile.as_str())
            .collect();
        assert!(profiles.contains(&"kimo-gmail/1"));
        let capabilities: Vec<&str> = described.result["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|capability| capability["name"].as_str())
            .collect();
        assert!(capabilities.contains(&"message.set_labels"));
        assert!(capabilities.contains(&"query.raw"));
        assert!(capabilities.contains(&"thread.native"));

        // The sign-in URL is the one thing the host opens a browser for; it
        // has to name the loopback port the host is holding a socket for.
        let mut request = crate::plugin::abi::PluginRequest::new("auth.begin");
        request.params = serde_json::json!({
            "redirect_port": 8914,
            "client_id": "test-client.apps.googleusercontent.com"
        });
        let challenge = instance.invoke(&request).unwrap();
        assert!(challenge.ok, "{:?}", challenge.error);
        let url = challenge.result["url"].as_str().unwrap();
        assert!(url.starts_with("https://accounts.google.com/"), "{url}");
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8914%2F"));
        assert!(url.contains("code_challenge_method=S256"));
    }

    #[test]
    fn the_samples_are_discovered_from_their_directories() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let registry = Registry::discover_from(&[root.join("plugins")]);
        assert_eq!(registry.errors().len(), 0, "{:?}", registry.errors());
        assert_eq!(registry.plugins().len(), 3);
        let ids: Vec<&str> = registry
            .plugins()
            .iter()
            .map(|loaded| loaded.manifest.id.as_str())
            .collect();
        assert!(ids.contains(&"org.kimo.echo"));
        assert!(ids.contains(&"org.kimo.sender-groups"));
        assert!(ids.contains(&"org.kimo.gmail"));
    }

    #[test]
    fn a_disabled_plugin_is_listed_but_not_compiled() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let registry = Registry::discover_from_with_disabled(
            &[root.join("plugins")],
            &["org.kimo.echo".to_string()],
        );
        assert_eq!(registry.errors().len(), 0, "{:?}", registry.errors());
        // The other two samples (sender-groups and gmail) still load.
        assert_eq!(registry.plugins().len(), 2);
        assert_eq!(registry.disabled().len(), 1);
        assert_eq!(registry.disabled()[0].id, "org.kimo.echo");
    }
}
