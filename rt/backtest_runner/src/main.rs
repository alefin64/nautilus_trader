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

//! Runs a backtest of compiled strategy plug-ins on bars from a Nautilus Parquet catalog.
//!
//! ```text
//! nautilus-backtest --config backtest.toml --output out/
//! ```
//!
//! The run uses the bar simulation in `nautilus_rt::sim`: orders fill at the next bar's open
//! plus slippage, with commission, borrow, account limits, and session rules from the config.
//! It writes `results.parquet`, `trades.parquet`, `returns.parquet`, and `run.json` to the
//! output directory. Identical inputs give byte-identical files.

mod config;
mod output;

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use clap::Parser;
use nautilus_rt::{
    anyhow::{self, Context},
    backtest::config::BacktestEngineConfig,
    catalog,
    common::logging::logger::LoggerConfig,
    core::UnixNanos,
    digest,
    loader::PluginLoader,
    log::LevelFilter,
    serde_json::{self, json},
    sessions::SessionSchedule,
    sim::{BarSimulation, SimOutcome},
};

use crate::{
    config::RunnerConfig,
    output::{RESULTS_FILE, RETURNS_FILE, RUN_FILE, RunLabels, TRADES_FILE},
};

/// Format identifier of `run.json`.
const RUN_FORMAT: &str = "nautilus-backtest-run/1";

#[derive(Debug, Parser)]
#[command(about = "Run a bar backtest of strategies loaded from compiled plug-ins")]
struct Cli {
    /// Path to the backtest TOML config.
    #[arg(long)]
    config: PathBuf,
    /// Directory to write the output files to. Created if missing.
    #[arg(long)]
    output: PathBuf,
    /// Engine log level on stdout: off, error, warn, info, debug, or trace.
    #[arg(long, default_value = "warn")]
    log_level: LevelFilter,
}

fn main() -> anyhow::Result<()> {
    nautilus_rt::ensure_host_build(nautilus_rt::BUILD_FINGERPRINT)?;

    let cli = Cli::parse();
    let text = fs::read_to_string(&cli.config)
        .with_context(|| format!("failed to read config '{}'", cli.config.display()))?;
    let config = RunnerConfig::from_toml(&text)
        .with_context(|| format!("invalid config '{}'", cli.config.display()))?;

    let data = &config.data;
    let schedule = SessionSchedule::read_parquet(&data.sessions)?;
    let (start, end) = session_range(&schedule, &config)?;
    let instrument = catalog::read_instrument(&data.catalog, data.instrument_id)?;
    let bars = catalog::read_bars(&data.catalog, data.bar_type, Some(start), Some(end))?;
    anyhow::ensure!(
        !bars.is_empty(),
        "catalog '{}' has no {} bars from {} to {}",
        data.catalog.display(),
        data.bar_type,
        data.start,
        data.end
    );

    let engine_config = BacktestEngineConfig {
        logging: LoggerConfig {
            stdout_level: cli.log_level,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut sim = BarSimulation::with_engine_config(
        &config.sim_config(),
        engine_config,
        instrument,
        bars,
        &schedule,
    )?;
    let mut loader = PluginLoader::new();
    for strategy in &config.strategies {
        loader.add_backtest_strategy(sim.engine_mut(), strategy)?;
    }

    let started = Instant::now();
    let outcome = sim.run()?;
    let elapsed = started.elapsed();

    write_outputs(&cli.output, &config, &text, &loader, &outcome)?;

    let metrics = &outcome.metrics;
    println!(
        "Simulated {} bars over {} sessions in {elapsed:.3?}: {} trades, net PnL {}, return {:.2}%, Sharpe {}, max drawdown {:.2}%",
        metrics.bars,
        metrics.sessions,
        metrics.trade_count,
        metrics.net_pnl.round_dp(2),
        metrics.return_pct,
        metrics
            .sharpe
            .map_or_else(|| "n/a".to_string(), |s| format!("{s:.3}")),
        metrics.max_drawdown_pct,
    );
    println!("Wrote results to {}", cli.output.display());
    Ok(())
}

// Time range of the configured trading days, for the catalog query
fn session_range(
    schedule: &SessionSchedule,
    config: &RunnerConfig,
) -> anyhow::Result<(UnixNanos, UnixNanos)> {
    let data = &config.data;
    let windows: Vec<_> = schedule
        .windows()
        .iter()
        .filter(|w| w.date >= data.start && w.date <= data.end)
        .collect();
    let (Some(first), Some(last)) = (windows.first(), windows.last()) else {
        anyhow::bail!(
            "sessions file '{}' has no trading days from {} to {}",
            data.sessions.display(),
            data.start,
            data.end
        );
    };
    // Bars are stamped at their close, so the first one closes after the first window opens
    Ok((
        first.open + nautilus_rt::core::DurationNanos::new(1),
        last.close,
    ))
}

fn write_outputs(
    dir: &Path,
    config: &RunnerConfig,
    config_text: &str,
    loader: &PluginLoader,
    outcome: &SimOutcome,
) -> anyhow::Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create output directory '{}'", dir.display()))?;

    let instrument_id = config.data.instrument_id.to_string();
    let bar_type = config.data.bar_type.to_string();
    let overnight = serde_json::to_value(config.session.overnight)?;
    let labels = RunLabels {
        instrument_id: &instrument_id,
        bar_type: &bar_type,
        session_profile: &config.session.profile,
        overnight: overnight.as_str().unwrap_or_default(),
        seed: config.seed,
        build_fingerprint: nautilus_rt::BUILD_FINGERPRINT,
    };
    output::write_results(&dir.join(RESULTS_FILE), &outcome.metrics, &labels)?;
    output::write_trades(&dir.join(TRADES_FILE), outcome)?;
    output::write_returns(&dir.join(RETURNS_FILE), outcome)?;

    let mut outputs = BTreeMap::new();
    for name in [RESULTS_FILE, TRADES_FILE, RETURNS_FILE] {
        outputs.insert(name, digest::file(&dir.join(name))?);
    }
    let plugins = loader
        .plugins()
        .iter()
        .map(|plugin| {
            Ok(json!({
                "name": plugin.name,
                "version": plugin.version,
                "strategies": plugin.type_names(),
                "blake3": digest::file(&plugin.path)?,
            }))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut denials: BTreeMap<&str, usize> = BTreeMap::new();
    for denial in &outcome.denials {
        *denials.entry(denial.reason.as_str()).or_default() += 1;
    }
    let totals = &outcome.totals;
    let run = json!({
        "format": RUN_FORMAT,
        "engine": {
            "version": env!("CARGO_PKG_VERSION"),
            "build_fingerprint": nautilus_rt::BUILD_FINGERPRINT,
            "rustc": nautilus_rt::RUSTC_VERSION,
        },
        "config": config,
        "inputs": {
            "config_blake3": nautilus_rt::blake3::hash(config_text.as_bytes()).to_hex().as_str(),
            "catalog_blake3": digest::tree(&config.data.catalog)?,
            "sessions_blake3": digest::file(&config.data.sessions)?,
            "plugins": plugins,
        },
        "data": {
            "first_bar": outcome.daily.first().map(|_| outcome.metrics.start_date.to_string()),
            "last_bar": outcome.daily.last().map(|_| outcome.metrics.end_date.to_string()),
            "sessions": outcome.metrics.sessions,
            "bars": outcome.metrics.bars,
        },
        "engine_totals": {
            "iterations": totals.iterations,
            "events": totals.total_events,
            "orders": totals.total_orders,
            "positions": totals.total_positions,
        },
        "denied_orders": denials,
        "outputs": outputs,
    });
    let mut text = serde_json::to_string_pretty(&run)?;
    text.push('\n');
    fs::write(dir.join(RUN_FILE), text)
        .with_context(|| format!("failed to write '{}'", dir.join(RUN_FILE).display()))?;
    Ok(())
}
