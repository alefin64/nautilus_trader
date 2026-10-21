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

//! Runs a backtest whose strategies are loaded from compiled plug-ins at startup.
//!
//! Prototype scope: a fixed simulated FX venue (`SIM`) with synthetic AUD/USD quotes.
//!
//! ```text
//! nautilus-backtest --config backtest.toml
//! ```

use std::{fs, path::PathBuf, time::Instant};

use clap::Parser;
use nautilus_rt::{
    anyhow::{self, Context},
    backtest::{config::BacktestEngineConfig, engine::BacktestEngine},
    loader::{PluginLoader, StrategyPluginConfig},
    model::enums::OmsType,
    serde::Deserialize,
    synthetic,
};

#[derive(Debug, Parser)]
#[command(about = "Run a backtest with strategies loaded from compiled plug-ins")]
struct Cli {
    /// Path to the backtest TOML config.
    #[arg(long)]
    config: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(crate = "nautilus_rt::serde", deny_unknown_fields)]
struct RunnerConfig {
    /// Number of synthetic up/down price cycles to generate.
    #[serde(default = "default_synthetic_cycles")]
    synthetic_cycles: usize,
    /// Order management system of the simulated venue.
    #[serde(default = "default_oms_type")]
    oms_type: OmsType,
    /// Strategies to load from plug-ins.
    strategies: Vec<StrategyPluginConfig>,
}

const fn default_synthetic_cycles() -> usize {
    6
}

const fn default_oms_type() -> OmsType {
    OmsType::Hedging
}

fn main() -> anyhow::Result<()> {
    nautilus_rt::ensure_host_build(nautilus_rt::BUILD_FINGERPRINT)?;

    let cli = Cli::parse();
    let text = fs::read_to_string(&cli.config)
        .with_context(|| format!("failed to read config '{}'", cli.config.display()))?;
    let config: RunnerConfig = toml::from_str(&text)
        .with_context(|| format!("invalid config '{}'", cli.config.display()))?;

    let mut engine = BacktestEngine::new(BacktestEngineConfig::default())?;
    let instrument_id = synthetic::add_venue_and_instrument(&mut engine, config.oms_type)?;

    let mut loader = PluginLoader::new();
    for strategy in &config.strategies {
        loader.add_backtest_strategy(&mut engine, strategy)?;
    }

    let quotes = synthetic::generate_quotes(instrument_id, config.synthetic_cycles);
    let count = quotes.len();
    engine.add_data(quotes, None, true, true)?;

    let start = Instant::now();
    engine.run(None, None, None, false)?;
    let elapsed = start.elapsed();

    println!(
        "Processed {count} quotes in {elapsed:.3?} ({:.0} quotes/s)",
        count as f64 / elapsed.as_secs_f64()
    );
    Ok(())
}
