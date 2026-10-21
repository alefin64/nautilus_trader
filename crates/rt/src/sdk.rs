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

//! Plug-in side of the strategy plug-in contract.
//!
//! A plug-in crate builds as a `dylib`, depends only on `nautilus-rt`, and invokes
//! [`strategy_plugin!`](crate::strategy_plugin) once. The macro exports three symbols:
//!
//! - `nautilus_plugin_init`: the C-ABI manifest from `nautilus-plugin`.
//! - `nautilus_rt_plugin_fingerprint`: a C-ABI function returning [`BUILD_FINGERPRINT`] as
//!   compiled into the plug-in.
//! - `NAUTILUS_RT_STRATEGY_PLUGIN`: a Rust-ABI [`StrategyPluginDecl`]. The loader reads it
//!   only after both C-ABI checks pass.
//!
//! [`BUILD_FINGERPRINT`]: crate::BUILD_FINGERPRINT

use std::fmt::Debug;

use nautilus_backtest::engine::BacktestEngine;
use nautilus_common::{actor::DataActorNative, component::Component};
use nautilus_trading::{Strategy, StrategyNative};

/// Name of the C-ABI symbol returning the plug-in's compiled build fingerprint.
pub const FINGERPRINT_SYMBOL: &[u8] = b"nautilus_rt_plugin_fingerprint";

/// Name of the Rust-ABI symbol holding the plug-in's [`StrategyPluginDecl`].
pub const DECL_SYMBOL: &[u8] = b"NAUTILUS_RT_STRATEGY_PLUGIN";

/// Signature of the C-ABI fingerprint symbol.
pub type FingerprintFn = extern "C" fn() -> nautilus_plugin::BorrowedStr<'static>;

/// Signature of a plug-in's registration function.
///
/// Creates the strategy named `type_name` from `config` and adds it through `registrar`.
pub type RegisterFn = fn(
    registrar: &mut StrategyRegistrar<'_>,
    type_name: &str,
    config: serde_json::Value,
) -> anyhow::Result<()>;

/// Strategy types a plug-in provides and the function that creates them.
#[derive(Debug)]
pub struct StrategyPluginDecl {
    /// Strategy type names accepted by `register`.
    pub type_names: &'static [&'static str],
    /// Creates and registers one strategy instance.
    pub register: RegisterFn,
}

/// Registration target handed to a plug-in's [`RegisterFn`].
#[derive(Debug)]
pub struct StrategyRegistrar<'a> {
    engine: &'a mut BacktestEngine,
}

impl<'a> StrategyRegistrar<'a> {
    /// Creates a registrar that adds strategies to `engine`.
    #[must_use]
    pub fn backtest(engine: &'a mut BacktestEngine) -> Self {
        Self { engine }
    }

    /// Adds `strategy` to the engine.
    ///
    /// # Errors
    ///
    /// Returns an error if the engine rejects the strategy.
    pub fn add_strategy<T>(&mut self, strategy: T) -> anyhow::Result<()>
    where
        T: Strategy + StrategyNative + DataActorNative + Component + Debug + 'static,
    {
        self.engine.add_strategy(strategy)
    }
}

/// Exports the strategy plug-in symbols for a plug-in crate.
///
/// Each entry maps a strategy type name to a function taking the instance's JSON config and
/// returning the strategy, or an error.
///
/// ```ignore
/// nautilus_rt::strategy_plugin! {
///     name: "ema-cross",
///     version: env!("CARGO_PKG_VERSION"),
///     strategies: {
///         "EmaCross" => EmaCross::from_json,
///     },
/// }
/// ```
#[macro_export]
macro_rules! strategy_plugin {
    (
        name: $name:expr,
        version: $version:expr,
        strategies: { $($type_name:literal => $factory:path),+ $(,)? } $(,)?
    ) => {
        $crate::plugin::nautilus_plugin! {
            name: $name,
            version: $version,
        }

        const _: () = {
            #[unsafe(no_mangle)]
            pub extern "C" fn nautilus_rt_plugin_fingerprint()
            -> $crate::plugin::BorrowedStr<'static> {
                $crate::plugin::BorrowedStr::from_str($crate::BUILD_FINGERPRINT)
            }

            fn register(
                registrar: &mut $crate::sdk::StrategyRegistrar<'_>,
                type_name: &str,
                config: $crate::serde_json::Value,
            ) -> $crate::anyhow::Result<()> {
                match type_name {
                    $($type_name => registrar.add_strategy($factory(config)?),)+
                    other => $crate::anyhow::bail!(
                        "plug-in '{}' has no strategy type '{other}'",
                        $name
                    ),
                }
            }

            #[unsafe(no_mangle)]
            pub static NAUTILUS_RT_STRATEGY_PLUGIN: $crate::sdk::StrategyPluginDecl =
                $crate::sdk::StrategyPluginDecl {
                    type_names: &[$($type_name),+],
                    register,
                };
        };
    };
}
