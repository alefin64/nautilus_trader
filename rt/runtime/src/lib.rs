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

//! Shared runtime library and strategy plug-in SDK for
//! [NautilusTrader](https://nautilustrader.io).
//!
//! This crate compiles to a single Rust `dylib` that contains the engine. The host binary and
//! every strategy plug-in link it dynamically, so the whole process shares one copy of the
//! engine's process-wide state.
//!
//! Plug-ins depend on this crate only, and reach engine and third-party crates through its
//! re-exports. Depending on a re-exported crate directly would link a second copy of it.
//!
//! # Build identity
//!
//! The plug-in boundary passes Rust types, which is only sound when the host, this library,
//! and every plug-in come from one build. [`BUILD_FINGERPRINT`] identifies that build, and the
//! [`loader`] refuses to load anything that disagrees with the loaded runtime.
//!
//! # Bar backtests
//!
//! [`sim`] runs strategies on 5-minute bars with the Bullpen fill model (orders fill at the next
//! bar's open), account rules, and session rules. [`catalog`] reads and writes the Nautilus
//! Parquet data catalogs it runs on, and [`sessions`] reads the trading session schedule.

pub mod catalog;
pub mod digest;
pub mod loader;
pub mod sdk;
pub mod sessions;
pub mod sim;
pub mod synthetic;

pub use anyhow;
pub use arrow;
pub use blake3;
pub use indexmap;
pub use jiff;
pub use log;
pub use nautilus_analysis as analysis;
pub use nautilus_backtest as backtest;
pub use nautilus_common as common;
pub use nautilus_core as core;
pub use nautilus_execution as execution;
pub use nautilus_indicators as indicators;
pub use nautilus_model as model;
pub use nautilus_persistence as persistence;
pub use nautilus_plugin as plugin;
pub use nautilus_portfolio as portfolio;
pub use nautilus_serialization as serialization;
pub use nautilus_system as system;
pub use nautilus_trading as trading;
pub use parquet;
pub use rust_decimal;
pub use serde;
pub use serde_json;
pub use ustr;

/// Build fingerprint of the code that references it.
///
/// A constant is evaluated where it is used, so a plug-in or host binary that reads it records
/// the fingerprint it was compiled against. Compare it with [`runtime_fingerprint`] to detect a
/// stale artifact.
pub const BUILD_FINGERPRINT: &str = env!("NAUTILUS_RT_FINGERPRINT");

/// Version line of the compiler that built the code that references it, for example
/// `rustc 1.99.0 (...)`.
pub const RUSTC_VERSION: &str = env!("NAUTILUS_RT_RUSTC_VERSION");

/// Returns the build fingerprint of the runtime library loaded in this process.
#[inline(never)]
#[must_use]
pub fn runtime_fingerprint() -> &'static str {
    BUILD_FINGERPRINT
}

/// Fails unless the calling binary was compiled against the loaded runtime library.
///
/// Call it as `nautilus_rt::ensure_host_build(nautilus_rt::BUILD_FINGERPRINT)` from the host
/// binary, so the argument is the constant compiled into the host.
///
/// # Errors
///
/// Returns an error if `host_fingerprint` differs from [`runtime_fingerprint`].
pub fn ensure_host_build(host_fingerprint: &str) -> anyhow::Result<()> {
    let runtime = runtime_fingerprint();
    anyhow::ensure!(
        host_fingerprint == runtime,
        "host binary was built against runtime {host_fingerprint}, but the loaded runtime is {runtime}; rebuild the host and runtime together"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn build_fingerprint_is_blake3_hex() {
        assert_eq!(BUILD_FINGERPRINT.len(), 64);
        assert!(BUILD_FINGERPRINT.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[rstest]
    fn ensure_host_build_accepts_runtime_fingerprint() {
        ensure_host_build(BUILD_FINGERPRINT).unwrap();
    }

    #[rstest]
    fn ensure_host_build_rejects_other_fingerprint() {
        let err = ensure_host_build("stale").unwrap_err();

        assert!(
            err.to_string()
                .contains("rebuild the host and runtime together")
        );
    }
}
