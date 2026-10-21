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

//! Synthetic FX backtest scenario.
//!
//! A simulated `SIM` venue, an AUD/USD currency pair, and a deterministic quote series of
//! repeated up/down ramps. With six cycles it reproduces the `engine-ema-cross` example in
//! `nautilus-backtest`, so plug-in and in-process runs can be compared with it exactly.

use nautilus_backtest::{config::SimulatedVenueConfig, engine::BacktestEngine};
use nautilus_core::UnixNanos;
use nautilus_execution::models::fee::{FeeModelAny, MakerTakerFeeModel};
use nautilus_model::{
    data::{Data, QuoteTick},
    enums::{AccountType, BookType, OmsType},
    identifiers::{InstrumentId, Symbol, Venue},
    instruments::{CurrencyPair, Instrument, InstrumentAny},
    types::{Currency, Money, Price, Quantity},
};
use rust_decimal::Decimal;
use ustr::Ustr;

/// Name of the simulated venue.
pub const VENUE: &str = "SIM";

/// Number of flat quotes before the first cycle.
pub const WARMUP_QUOTES: usize = 25;

/// Number of quotes in each up/down cycle.
pub const QUOTES_PER_CYCLE: usize = 120;

/// Adds the simulated venue and the AUD/USD instrument to `engine`.
///
/// Returns the instrument ID.
///
/// # Errors
///
/// Returns an error if the engine rejects the venue or instrument.
pub fn add_venue_and_instrument(
    engine: &mut BacktestEngine,
    oms_type: OmsType,
) -> anyhow::Result<InstrumentId> {
    engine.add_venue(
        SimulatedVenueConfig::builder()
            .venue(Venue::from(VENUE))
            .oms_type(oms_type)
            .account_type(AccountType::Margin)
            .book_type(BookType::L1_MBP)
            .starting_balances(vec![Money::from("1_000_000 USD")])
            .fee_model(FeeModelAny::MakerTaker(MakerTakerFeeModel::zero()).into())
            .build()?,
    )?;

    let instrument = InstrumentAny::CurrencyPair(audusd_sim()?);
    engine.add_instrument(&instrument)?;
    Ok(instrument.id())
}

/// Returns the AUD/USD currency pair on the simulated venue.
///
/// Same definition as `nautilus_model::instruments::stubs::audusd_sim`, which requires the
/// model's `test-support` feature.
///
/// # Errors
///
/// Returns an error if the instrument fails validation.
pub fn audusd_sim() -> anyhow::Result<CurrencyPair> {
    let symbol = Symbol::from("AUD/USD");
    let instrument = CurrencyPair::builder()
        .instrument_id(InstrumentId::new(symbol, Venue::from(VENUE)))
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

/// Generates the synthetic quote series for `instrument_id`.
///
/// Produces [`WARMUP_QUOTES`] flat quotes followed by `cycles` cycles of
/// [`QUOTES_PER_CYCLE`] quotes, one per second from 2025-01-01T00:00:00Z. The cycle base
/// price repeats every six cycles so long runs stay in range.
#[must_use]
pub fn generate_quotes(instrument_id: InstrumentId, cycles: usize) -> Vec<Data> {
    let spread = 0.00020;
    let base_ts: u64 = 1_735_689_600_000_000_000; // 2025-01-01T00:00:00Z
    let interval: u64 = 1_000_000_000;
    let mut quotes = Vec::with_capacity(WARMUP_QUOTES + cycles * QUOTES_PER_CYCLE);
    let mut tick: u64 = 0;

    let mut add = |mid: f64| {
        let bid = format!("{mid:.5}");
        let ask = format!("{:.5}", mid + spread);
        quotes.push(Data::Quote(QuoteTick::new(
            instrument_id,
            Price::from(bid.as_str()),
            Price::from(ask.as_str()),
            Quantity::from("100000"),
            Quantity::from("100000"),
            (base_ts + tick * interval).into(),
            (base_ts + tick * interval).into(),
        )));
        tick += 1;
    };

    // Flat initialization: both EMAs converge around 0.65000
    for _ in 0..WARMUP_QUOTES {
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

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(0)]
    #[case(1)]
    #[case(6)]
    fn generate_quotes_count(#[case] cycles: usize) {
        let quotes = generate_quotes(InstrumentId::from("AUD/USD.SIM"), cycles);

        assert_eq!(quotes.len(), WARMUP_QUOTES + cycles * QUOTES_PER_CYCLE);
    }

    #[rstest]
    fn generate_quotes_are_one_second_apart() {
        let quotes = generate_quotes(InstrumentId::from("AUD/USD.SIM"), 2);

        let timestamps: Vec<u64> = quotes
            .iter()
            .map(|data| match data {
                Data::Quote(quote) => quote.ts_event.as_u64(),
                other => panic!("unexpected data {other:?}"),
            })
            .collect();
        assert!(timestamps.windows(2).all(|w| w[1] - w[0] == 1_000_000_000));
    }

    #[rstest]
    fn audusd_sim_is_on_sim_venue() {
        let instrument = audusd_sim().unwrap();

        assert_eq!(instrument.id, InstrumentId::from("AUD/USD.SIM"));
        assert_eq!(instrument.price_precision, 5);
    }
}
