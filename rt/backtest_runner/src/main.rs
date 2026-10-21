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
    backtest::{
        config::{BacktestEngineConfig, SimulatedVenueConfig},
        engine::BacktestEngine,
    },
    core::UnixNanos,
    execution::models::fee::{FeeModelAny, MakerTakerFeeModel},
    loader::{PluginLoader, StrategyPluginConfig},
    model::{
        data::{Data, QuoteTick},
        enums::{AccountType, BookType, OmsType},
        identifiers::{InstrumentId, Symbol, Venue},
        instruments::{CurrencyPair, Instrument, InstrumentAny},
        types::{Currency, Money, Price, Quantity},
    },
    rust_decimal::Decimal,
    serde::Deserialize,
    ustr::Ustr,
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

    engine.add_venue(
        SimulatedVenueConfig::builder()
            .venue(Venue::from("SIM"))
            .oms_type(config.oms_type)
            .account_type(AccountType::Margin)
            .book_type(BookType::L1_MBP)
            .starting_balances(vec![Money::from("1_000_000 USD")])
            .fee_model(FeeModelAny::MakerTaker(MakerTakerFeeModel::zero()).into())
            .build()?,
    )?;

    let instrument = InstrumentAny::CurrencyPair(audusd_sim()?);
    let instrument_id = instrument.id();
    engine.add_instrument(&instrument)?;

    let mut loader = PluginLoader::new();
    for strategy in &config.strategies {
        loader.add_backtest_strategy(&mut engine, strategy)?;
    }

    let quotes = generate_quotes(instrument_id, config.synthetic_cycles);
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

// Same definition as `nautilus_model::instruments::stubs::audusd_sim`, which requires the
// model's `test-support` feature
fn audusd_sim() -> anyhow::Result<CurrencyPair> {
    let symbol = Symbol::from("AUD/USD");
    let instrument = CurrencyPair::builder()
        .instrument_id(InstrumentId::new(symbol, Venue::from("SIM")))
        .raw_symbol(symbol)
        .base_currency(Currency::from("AUD"))
        .quote_currency(Currency::from("USD"))
        .price_precision(5)
        .size_precision(0)
        .price_increment(Price::from("0.00001"))
        .size_increment(Quantity::from("1"))
        .lot_size(Quantity::from("1000"))
        .max_quantity(Quantity::from("1000000"))
        .min_quantity(Quantity::from("100"))
        .margin_init(Decimal::new(3, 2))
        .margin_maint(Decimal::new(3, 2))
        .tick_scheme(Ustr::from("FOREX_5DECIMAL"))
        .ts_event(UnixNanos::default())
        .ts_init(UnixNanos::default())
        .build()?;
    Ok(instrument)
}

fn quote(instrument_id: InstrumentId, bid: &str, ask: &str, ts: u64) -> Data {
    Data::Quote(QuoteTick::new(
        instrument_id,
        Price::from(bid),
        Price::from(ask),
        Quantity::from("100000"),
        Quantity::from("100000"),
        ts.into(),
        ts.into(),
    ))
}

// Same series as the `engine-ema-cross` example in `nautilus-backtest`
fn generate_quotes(instrument_id: InstrumentId, cycles: usize) -> Vec<Data> {
    let spread = 0.00020;
    let base_ts: u64 = 1_735_689_600_000_000_000; // 2025-01-01T00:00:00Z
    let interval: u64 = 1_000_000_000;
    let mut quotes = Vec::new();
    let mut tick: u64 = 0;

    let mut add = |mid: f64| {
        let bid = format!("{mid:.5}");
        let ask = format!("{:.5}", mid + spread);
        quotes.push(quote(instrument_id, &bid, &ask, base_ts + tick * interval));
        tick += 1;
    };

    // Flat initialization - both EMAs converge around 0.65000
    for _ in 0..25 {
        add(0.65000);
    }

    // Repeated up/down cycles to generate multiple crossovers
    for cycle in 0..cycles {
        let base = 0.65000 + ((cycle % 6) as f64 * 0.00100);

        for i in 0..40 {
            add(base + (i as f64 * 0.00050));
        }

        for i in 0..80 {
            let peak = base + 39.0 * 0.00050;
            add(peak - (i as f64 * 0.00050));
        }
    }

    quotes
}
