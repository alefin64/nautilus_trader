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

//! End-to-end tests: run the `nautilus-backtest` binary with the strategy slot plug-in on a
//! catalog built from the synthetic equity scenario.

use std::{
    fs::File,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::OnceLock,
};

use nautilus_rt::{
    arrow::array::{Array, BooleanArray, Float64Array, RecordBatch, StringArray, UInt64Array},
    catalog,
    parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder,
    serde_json::{self, Value},
    synthetic::EquityScenario,
};
use rstest::rstest;
use tempfile::TempDir;

const RUNNER: &str = env!("CARGO_BIN_EXE_nautilus-backtest");
const SESSIONS: usize = 5;

/// Builds a plug-in with the runner's profile and returns its path.
///
/// A plug-in is only loadable by a runtime from the same build, so it must be built into the
/// same target directory and profile as the runner under test.
fn plugin_path(package: &str, lib: &str) -> PathBuf {
    let profile_dir = Path::new(RUNNER).parent().unwrap();
    let rt_dir = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();

    let mut cargo = Command::new(env!("CARGO"));
    cargo.current_dir(rt_dir).args(["build", "-p", package]);
    if profile_dir.ends_with("release") {
        cargo.arg("--release");
    }
    let status = cargo.status().expect("run cargo build for the plug-in");
    assert!(status.success(), "building {package} failed");

    profile_dir.join(format!(
        "{}{lib}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ))
}

fn strategy_plugin() -> &'static Path {
    static PLUGIN: OnceLock<PathBuf> = OnceLock::new();
    PLUGIN.get_or_init(|| plugin_path("strategy-plugin", "strategy_plugin"))
}

struct Fixture {
    dir: TempDir,
    config: PathBuf,
}

impl Fixture {
    /// Writes a catalog, sessions file, and config with `extra` replacements applied.
    fn new(replacements: &[(&str, &str)]) -> Self {
        let dir = TempDir::new().unwrap();
        let scenario = EquityScenario::new(SESSIONS).unwrap();
        let catalog_dir = dir.path().join("catalog");
        catalog::write_bars(&catalog_dir, &scenario.instrument, &scenario.bars).unwrap();
        let sessions = dir.path().join("sessions.parquet");
        scenario.schedule.write_parquet(&sessions).unwrap();

        let mut config = format!(
            r#"
seed = 0

[data]
catalog = "{catalog}"
sessions = "{sessions}"
instrument_id = "SYNTH.XNYS"
bar_type = "SYNTH.XNYS-5-MINUTE-LAST-EXTERNAL"
start = 2025-01-06
end = 2025-01-10

[session]
profile = "rth"
overnight = "hold"

[venue]
oms_type = "NETTING"
account_type = "MARGIN"
base_currency = "USD"
starting_capital = 100000
max_leverage = 1.0
max_position_pct_equity = 100
allow_short = true
short_borrow_bps_per_year = 50

[fill_model]
type = "next_bar_open"

[fill_model.slippage_bps]
pre_market = 10
regular = 2
after_hours = 10

[fill_model.commission]
per_share = 0.005
min_per_order = 1.0

[[strategies]]
path = "{plugin}"
type_name = "Strategy"

[strategies.config]
instrument_id = "SYNTH.XNYS"
bar_type = "SYNTH.XNYS-5-MINUTE-LAST-EXTERNAL"
fast_period = 10
slow_period = 30
trade_size = "500"
go_short = true
"#,
            catalog = catalog_dir.display(),
            sessions = sessions.display(),
            plugin = strategy_plugin().display(),
        );
        for (from, to) in replacements {
            assert!(config.contains(from), "config has no '{from}'");
            config = config.replace(from, to);
        }
        let path = dir.path().join("backtest.toml");
        std::fs::write(&path, config).unwrap();
        Self { dir, config: path }
    }

    fn output(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn run(&self, output: &str) -> (Output, String) {
        let output = Command::new(RUNNER)
            .arg("--config")
            .arg(&self.config)
            .arg("--output")
            .arg(self.output(output))
            .output()
            .expect("run nautilus-backtest");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (output, text)
    }

    fn run_ok(&self, output: &str) -> PathBuf {
        let (result, text) = self.run(output);
        assert!(result.status.success(), "{text}");
        self.output(output)
    }
}

fn read_parquet(path: &Path) -> RecordBatch {
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap())
        .unwrap()
        .build()
        .unwrap();
    let batches: Vec<RecordBatch> = reader.map(Result::unwrap).collect();
    assert_eq!(batches.len(), 1);
    batches.into_iter().next().unwrap()
}

fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> &'a T {
    batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("no column {name}"))
        .as_any()
        .downcast_ref::<T>()
        .unwrap_or_else(|| panic!("column {name} has another type"))
}

fn results_u64(dir: &Path, name: &str) -> u64 {
    column::<UInt64Array>(&read_parquet(&dir.join("results.parquet")), name).value(0)
}

// Same strategy, config, and scenario as `backtest_on_synthetic_bars` in the strategy slot
#[rstest]
fn runs_strategy_plugin_and_writes_outputs() {
    let fixture = Fixture::new(&[]);

    let out = fixture.run_ok("out");

    assert_eq!(results_u64(&out, "sessions"), SESSIONS as u64);
    assert_eq!(results_u64(&out, "trade_count"), 7);
    assert_eq!(results_u64(&out, "fill_count"), 8);
    assert_eq!(results_u64(&out, "denied_orders"), 0);

    let results = read_parquet(&out.join("results.parquet"));
    assert_eq!(results.num_rows(), 1);
    let net = column::<Float64Array>(&results, "net_pnl").value(0);
    let gross = column::<Float64Array>(&results, "gross_pnl").value(0);
    let costs = column::<Float64Array>(&results, "total_costs").value(0);
    assert!((gross - costs - net).abs() < 1e-6);
    assert_eq!(
        column::<StringArray>(&results, "build_fingerprint").value(0),
        nautilus_rt::BUILD_FINGERPRINT
    );

    let returns = read_parquet(&out.join("returns.parquet"));
    assert_eq!(returns.num_rows(), SESSIONS);

    let trades = read_parquet(&out.join("trades.parquet"));
    let record = column::<StringArray>(&trades, "record");
    let fills = (0..record.len())
        .filter(|&i| record.value(i) == "fill")
        .count();
    let positions = (0..record.len())
        .filter(|&i| record.value(i) == "position")
        .count();
    assert_eq!(fills, 8);
    assert_eq!(positions, 8); // 7 closed and 1 still open

    let run: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("run.json")).unwrap()).unwrap();
    assert_eq!(run["format"], "nautilus-backtest-run/1");
    assert_eq!(
        run["engine"]["build_fingerprint"],
        nautilus_rt::BUILD_FINGERPRINT
    );
    assert_eq!(run["config"]["venue"]["oms_type"], "NETTING");
    assert_eq!(run["config"]["data"]["start"], "2025-01-06");
    assert_eq!(run["inputs"]["plugins"][0]["name"], "strategy");
    assert!(run["outputs"]["results.parquet"].is_string());
}

// Same inputs give byte-identical outputs
#[rstest]
fn outputs_are_deterministic() {
    let fixture = Fixture::new(&[]);

    let first = fixture.run_ok("first");
    let second = fixture.run_ok("second");

    for name in [
        "results.parquet",
        "trades.parquet",
        "returns.parquet",
        "run.json",
    ] {
        let a = std::fs::read(first.join(name)).unwrap();
        let b = std::fs::read(second.join(name)).unwrap();
        assert!(!a.is_empty(), "{name} is empty");
        assert!(a == b, "{name} differs between runs");
    }
}

#[rstest]
fn flatten_closes_positions_every_session() {
    let fixture = Fixture::new(&[("overnight = \"hold\"", "overnight = \"flatten\"")]);

    let out = fixture.run_ok("out");

    let returns = read_parquet(&out.join("returns.parquet"));
    let position = column::<Float64Array>(&returns, "position");
    assert!((0..position.len()).all(|i| position.value(i) == 0.0));
    let trades = read_parquet(&out.join("trades.parquet"));
    let flatten = column::<BooleanArray>(&trades, "flatten");
    let flattened = (0..flatten.len())
        .filter(|&i| flatten.is_valid(i) && flatten.value(i))
        .count();
    assert_eq!(flattened, SESSIONS);
    assert!(results_u64(&out, "denied_orders") > 0);
}

#[rstest]
fn date_range_selects_sessions() {
    let fixture = Fixture::new(&[
        ("start = 2025-01-06", "start = 2025-01-07"),
        ("end = 2025-01-10", "end = 2025-01-08"),
    ]);

    let out = fixture.run_ok("out");

    assert_eq!(results_u64(&out, "sessions"), 2);
    assert_eq!(results_u64(&out, "bars"), 2 * 78);
}

#[rstest]
#[case::unknown_type(
    &[("type_name = \"Strategy\"", "type_name = \"Other\"")],
    "plug-in 'strategy' provides [\"Strategy\"], not 'Other'"
)]
#[case::invalid_strategy_config(
    &[("fast_period = 10", "fast_period = \"ten\"")],
    "plug-in 'strategy' failed to create strategy 'Strategy'"
)]
#[case::unknown_runner_field(&[("seed = 0", "seed = 0\ncycles = 3")], "unknown field `cycles`")]
#[case::missing_value(&[("min_per_order = 1.0", "")], "missing field `min_per_order`")]
#[case::no_sessions_in_range(
    &[("start = 2025-01-06", "start = 2024-01-01"), ("end = 2025-01-10", "end = 2024-01-31")],
    "has no trading days from 2024-01-01 to 2024-01-31"
)]
#[case::unknown_instrument(
    &[("instrument_id = \"SYNTH.XNYS\"\nbar_type = \"SYNTH.XNYS-5", "instrument_id = \"OTHER.XNYS\"\nbar_type = \"OTHER.XNYS-5")],
    "has no instrument OTHER.XNYS"
)]
#[case::missing_plugin(&[("libstrategy_plugin", "libmissing_plugin")], "not found")]
fn rejects_invalid_setup(#[case] replacements: &[(&str, &str)], #[case] message: &str) {
    let fixture = Fixture::new(replacements);

    let (output, text) = fixture.run("out");

    assert!(!output.status.success(), "{text}");
    assert!(text.contains(message), "{text}");
}

#[rstest]
fn rejects_library_without_plugin_symbols() {
    let runtime = strategy_plugin().with_file_name(format!(
        "{}nautilus_rt{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    let plugin = strategy_plugin().display().to_string();
    let fixture = Fixture::new(&[(plugin.as_str(), &runtime.display().to_string())]);

    let (output, text) = fixture.run("out");

    assert!(!output.status.success());
    assert!(text.contains("missing `nautilus_plugin_init`"), "{text}");
}

#[rstest]
fn prints_build_info() {
    let output = Command::new(RUNNER).arg("--build-info").output().unwrap();

    assert!(output.status.success());
    let info: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(info["build_fingerprint"], nautilus_rt::BUILD_FINGERPRINT);
    assert_eq!(info["rustc"], nautilus_rt::RUSTC_VERSION);
    assert!(nautilus_rt::RUSTC_VERSION.starts_with("rustc "));
}

#[rstest]
fn requires_config_and_output() {
    let output = Command::new(RUNNER).output().unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--config"));
}
