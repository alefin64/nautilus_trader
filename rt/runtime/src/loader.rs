// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

//! Host side of the strategy plug-in contract.
//!
//! [`PluginLoader`] opens plug-in libraries, verifies them, and registers their strategies.
//! Verification runs in order, and each step only trusts what the previous step established:
//!
//! 1. The `nautilus-plugin` C-ABI manifest: ABI version and model precision mode.
//! 2. The C-ABI build fingerprint, which must equal the loaded runtime's fingerprint.
//! 3. Only then the Rust-ABI [`StrategyPluginDecl`].
//!
//! Loaded libraries are never unloaded. Plug-in code may leave thread-local destructors and
//! `'static` data registered with the engine, so a library stays mapped for the process lifetime.

use std::{
    fmt::Debug,
    fs,
    path::{Path, PathBuf},
    ptr::NonNull,
};

use anyhow::Context;
use libloading::{Library, Symbol};
use nautilus_backtest::engine::BacktestEngine;
use nautilus_plugin::{HostVTable, NAUTILUS_PLUGIN_INIT_SYMBOL, PluginInitFn};
use serde::Deserialize;

use crate::{
    runtime_fingerprint,
    sdk::{DECL_SYMBOL, FINGERPRINT_SYMBOL, FingerprintFn, StrategyPluginDecl, StrategyRegistrar},
};

/// Configuration for one strategy instance provided by a plug-in.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyPluginConfig {
    /// Path to the plug-in library.
    pub path: PathBuf,
    /// Strategy type name exported by the plug-in.
    pub type_name: String,
    /// Instance configuration passed to the plug-in's factory.
    #[serde(default)]
    pub config: serde_json::Value,
}

/// A verified plug-in library.
#[derive(Debug)]
pub struct LoadedPlugin {
    /// Plug-in name from its manifest.
    pub name: String,
    /// Plug-in version from its manifest.
    pub version: String,
    /// Canonical path the library was loaded from.
    pub path: PathBuf,
    decl: &'static StrategyPluginDecl,
}

impl LoadedPlugin {
    /// Returns the strategy type names the plug-in provides.
    #[must_use]
    pub fn type_names(&self) -> &'static [&'static str] {
        self.decl.type_names
    }
}

/// Loads strategy plug-ins and registers their strategies with an engine.
#[derive(Debug, Default)]
pub struct PluginLoader {
    plugins: Vec<LoadedPlugin>,
}

impl PluginLoader {
    /// Creates an empty loader.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the plug-ins loaded so far.
    #[must_use]
    pub fn plugins(&self) -> &[LoadedPlugin] {
        &self.plugins
    }

    /// Loads and verifies the plug-in at `path`, or returns it if already loaded.
    ///
    /// # Errors
    ///
    /// Returns an error if the library cannot be opened, lacks a required symbol, or was not
    /// built together with the loaded runtime.
    pub fn load(&mut self, path: &Path) -> anyhow::Result<&LoadedPlugin> {
        let path = fs::canonicalize(path)
            .with_context(|| format!("plug-in library '{}' not found", path.display()))?;

        if let Some(index) = self.plugins.iter().position(|p| p.path == path) {
            return Ok(&self.plugins[index]);
        }

        let plugin = load_library(&path)
            .with_context(|| format!("failed to load plug-in '{}'", path.display()))?;
        log::info!(
            "Loaded plug-in {} {} from {} with strategies {:?}",
            plugin.name,
            plugin.version,
            plugin.path.display(),
            plugin.type_names(),
        );
        self.plugins.push(plugin);
        Ok(self.plugins.last().expect("plug-in was just pushed"))
    }

    /// Loads the plug-in named by `config` and adds its strategy to a backtest engine.
    ///
    /// # Errors
    ///
    /// Returns an error if the plug-in cannot be loaded, does not provide `config.type_name`,
    /// or its factory or the engine rejects the strategy.
    pub fn add_backtest_strategy(
        &mut self,
        engine: &mut BacktestEngine,
        config: &StrategyPluginConfig,
    ) -> anyhow::Result<()> {
        let plugin = self.load(&config.path)?;
        anyhow::ensure!(
            plugin.type_names().contains(&config.type_name.as_str()),
            "plug-in '{}' provides {:?}, not '{}'",
            plugin.name,
            plugin.type_names(),
            config.type_name,
        );

        let mut registrar = StrategyRegistrar::backtest(engine);
        (plugin.decl.register)(&mut registrar, &config.type_name, config.config.clone())
            .with_context(|| {
                format!(
                    "plug-in '{}' failed to create strategy '{}'",
                    plugin.name, config.type_name
                )
            })
    }
}

fn load_library(path: &Path) -> anyhow::Result<LoadedPlugin> {
    // SAFETY: Opening the library runs its initializers. Plug-ins are trusted artifacts
    // produced by the same build as the runtime.
    let library = unsafe { Library::new(path) }?;

    // Never unload, see module docs
    let library: &'static Library = Box::leak(Box::new(library));

    // SAFETY: `nautilus_plugin_init` has the C-ABI signature declared by `nautilus-plugin`.
    let init: Symbol<'static, PluginInitFn> =
        unsafe { library.get(NAUTILUS_PLUGIN_INIT_SYMBOL) }
            .context("missing `nautilus_plugin_init`; is this a Nautilus plug-in?")?;

    // The host table is an opaque token that generated init thunks only check for null
    let host = NonNull::<HostVTable>::dangling();

    // SAFETY: The init thunk catches panics and returns null or a manifest with static lifetime.
    let manifest = unsafe { init(host.as_ptr()) };
    anyhow::ensure!(!manifest.is_null(), "plug-in init returned no manifest");

    // SAFETY: Non-null manifests point to a static in the never-unloaded library.
    let manifest = unsafe { &*manifest };
    manifest
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid plug-in manifest: {e}"))?;

    // SAFETY: Validated manifest strings are static UTF-8 strings in the library.
    let name = unsafe { manifest.plugin_name.as_str() }.to_string();
    // SAFETY: Validated manifest strings are static UTF-8 strings in the library.
    let version = unsafe { manifest.plugin_version.as_str() }.to_string();

    // SAFETY: The fingerprint symbol has the C-ABI signature declared by `FingerprintFn`.
    let fingerprint: Symbol<'static, FingerprintFn> = unsafe { library.get(FINGERPRINT_SYMBOL) }
        .context("missing `nautilus_rt_plugin_fingerprint`; was the plug-in built with `nautilus_rt::strategy_plugin!`?")?;

    // SAFETY: The function returns a static UTF-8 string in the library.
    let plugin_fingerprint = unsafe { fingerprint().as_str() };
    let runtime = runtime_fingerprint();
    anyhow::ensure!(
        plugin_fingerprint == runtime,
        "plug-in '{name}' was built against runtime {plugin_fingerprint}, but the loaded runtime is {runtime}; rebuild the plug-in with the runtime"
    );

    // SAFETY: The fingerprint check established that the plug-in shares this build, so the
    // symbol is a `StrategyPluginDecl` static.
    let symbol: Symbol<'static, *const StrategyPluginDecl> =
        unsafe { library.get(DECL_SYMBOL) }.context("missing `NAUTILUS_RT_STRATEGY_PLUGIN`")?;

    // SAFETY: The symbol points to a static in the never-unloaded library.
    let decl: &'static StrategyPluginDecl = unsafe { &**symbol };

    Ok(LoadedPlugin {
        name,
        version,
        path: path.to_path_buf(),
        decl,
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn strategy_plugin_config_from_toml() {
        let config: StrategyPluginConfig = toml::from_str(
            r#"
            path = "target/debug/libema_cross_plugin.so"
            type_name = "EmaCross"

            [config]
            fast_period = 10
            "#,
        )
        .unwrap();

        assert_eq!(
            config.path,
            PathBuf::from("target/debug/libema_cross_plugin.so")
        );
        assert_eq!(config.type_name, "EmaCross");
        assert_eq!(config.config["fast_period"], 10);
    }

    #[rstest]
    fn strategy_plugin_config_defaults_to_null_config() {
        let config: StrategyPluginConfig =
            toml::from_str("path = \"plugin.so\"\ntype_name = \"EmaCross\"").unwrap();

        assert!(config.config.is_null());
    }

    #[rstest]
    fn strategy_plugin_config_rejects_unknown_fields() {
        let result: Result<StrategyPluginConfig, _> =
            toml::from_str("path = \"plugin.so\"\ntype_name = \"EmaCross\"\nsha = \"x\"");

        assert!(result.is_err());
    }

    #[rstest]
    fn load_missing_library_fails() {
        let mut loader = PluginLoader::new();

        let err = loader.load(Path::new("does/not/exist.so")).unwrap_err();

        assert!(err.to_string().contains("not found"));
        assert!(loader.plugins().is_empty());
    }

    #[rstest]
    fn load_non_library_fails() {
        let mut loader = PluginLoader::new();
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");

        let err = loader.load(&manifest).unwrap_err();

        assert!(err.to_string().contains("failed to load plug-in"));
        assert!(loader.plugins().is_empty());
    }
}
