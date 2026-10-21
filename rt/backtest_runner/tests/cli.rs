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

//! End-to-end tests: run the `nautilus-backtest` binary against the example plug-in.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::OnceLock,
};

use rstest::rstest;
use tempfile::TempDir;

const RUNNER: &str = env!("CARGO_BIN_EXE_nautilus-backtest");

/// Builds the example plug-in with the runner's profile and returns its path.
///
/// The plug-in is only loadable by a runtime from the same build, so it must be built into the
/// same target directory and profile as the runner under test.
fn plugin_path() -> &'static Path {
    static PLUGIN: OnceLock<PathBuf> = OnceLock::new();
    PLUGIN.get_or_init(|| {
        let profile_dir = Path::new(RUNNER).parent().unwrap();
        let rt_dir = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();

        let mut cargo = Command::new(env!("CARGO"));
        cargo
            .current_dir(rt_dir)
            .args(["build", "-p", "nautilus-example-ema-cross-plugin"]);
        if profile_dir.ends_with("release") {
            cargo.arg("--release");
        }
        let status = cargo.status().expect("run cargo build for the plug-in");
        assert!(status.success(), "building the example plug-in failed");

        profile_dir.join(format!(
            "{}ema_cross_plugin{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        ))
    })
}

fn write_config(dir: &TempDir, plugin: &Path, type_name: &str, extra: &str) -> PathBuf {
    let path = dir.path().join("backtest.toml");
    let config = format!(
        r#"
{extra}

[[strategies]]
path = "{}"
type_name = "{type_name}"

[strategies.config]
instrument_id = "AUD/USD.SIM"
trade_size = "100000"
fast_period = 10
slow_period = 20
"#,
        plugin.display()
    );
    std::fs::write(&path, config).unwrap();
    path
}

fn run(config: &Path) -> (Output, String) {
    let output = Command::new(RUNNER)
        .arg("--config")
        .arg(config)
        .output()
        .expect("run nautilus-backtest");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output, text)
}

// Same totals as the `engine-ema-cross` example in `nautilus-backtest`
#[rstest]
fn runs_plugin_strategy() {
    let dir = TempDir::new().unwrap();
    let config = write_config(&dir, plugin_path(), "EmaCross", "");

    let (output, text) = run(&config);

    assert!(output.status.success(), "{text}");
    assert!(text.contains("Loaded plug-in ema-cross"), "{text}");
    assert!(text.contains("Iterations: 745"), "{text}");
    assert!(text.contains("Total events: 24"), "{text}");
    assert!(text.contains("Total orders: 12"), "{text}");
    assert!(text.contains("Total positions: 12"), "{text}");
    assert!(text.contains("Processed 745 quotes"), "{text}");
}

#[rstest]
fn netting_venue_with_more_cycles() {
    let dir = TempDir::new().unwrap();
    let config = write_config(
        &dir,
        plugin_path(),
        "EmaCross",
        "synthetic_cycles = 12\noms_type = \"NETTING\"",
    );

    let (output, text) = run(&config);

    assert!(output.status.success(), "{text}");
    assert!(text.contains("Processed 1465 quotes"), "{text}");
}

#[rstest]
fn rejects_unknown_strategy_type() {
    let dir = TempDir::new().unwrap();
    let config = write_config(&dir, plugin_path(), "EmaCrossV2", "");

    let (output, text) = run(&config);

    assert!(!output.status.success());
    assert!(
        text.contains("plug-in 'ema-cross' provides [\"EmaCross\"], not 'EmaCrossV2'"),
        "{text}"
    );
}

#[rstest]
fn rejects_library_without_plugin_symbols() {
    let dir = TempDir::new().unwrap();
    let runtime = plugin_path().with_file_name(format!(
        "{}nautilus_rt{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    let config = write_config(&dir, &runtime, "EmaCross", "");

    let (output, text) = run(&config);

    assert!(!output.status.success());
    assert!(text.contains("missing `nautilus_plugin_init`"), "{text}");
}

#[rstest]
fn rejects_missing_plugin() {
    let dir = TempDir::new().unwrap();
    let config = write_config(&dir, Path::new("does/not/exist.so"), "EmaCross", "");

    let (output, text) = run(&config);

    assert!(!output.status.success());
    assert!(text.contains("not found"), "{text}");
}

#[rstest]
fn rejects_invalid_strategy_config() {
    let dir = TempDir::new().unwrap();
    let config = write_config(&dir, plugin_path(), "EmaCross", "");
    let text = std::fs::read_to_string(&config)
        .unwrap()
        .replace("fast_period = 10", "fast_period = \"ten\"");
    std::fs::write(&config, text).unwrap();

    let (output, text) = run(&config);

    assert!(!output.status.success());
    assert!(
        text.contains("plug-in 'ema-cross' failed to create strategy 'EmaCross'"),
        "{text}"
    );
    assert!(text.contains("expected usize"), "{text}");
}

#[rstest]
fn rejects_unknown_runner_field() {
    let dir = TempDir::new().unwrap();
    let config = write_config(&dir, plugin_path(), "EmaCross", "cycles = 3");

    let (output, text) = run(&config);

    assert!(!output.status.success());
    assert!(text.contains("unknown field `cycles`"), "{text}");
}
