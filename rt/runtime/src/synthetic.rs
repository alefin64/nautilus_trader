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

//! Synthetic backtest scenarios for tests.
//!
//! - FX quotes: a simulated `SIM` venue, an AUD/USD currency pair, and a deterministic quote
//!   series of repeated up/down ramps. With six cycles it reproduces the `engine-ema-cross`
//!   example in `nautilus-backtest`, so plug-in and in-process runs can be compared with it
//!   exactly.
//! - Equity bars ([`EquityScenario`]): a `SYNTH.XNYS` equity with deterministic 5-minute bars
//!   and a matching session schedule, for bar strategies run with [`crate::sim`].

use jiff::civil::Date;
use nautilus_backtest::{config::SimulatedVenueConfig, engine::BacktestEngine};
use nautilus_core::UnixNanos;
use nautilus_execution::models::fee::{FeeModelAny, MakerTakerFeeModel};
use nautilus_model::{
    data::{Bar, BarType, Data, QuoteTick},
    enums::{AccountType, BookType, OmsType},
    identifiers::{InstrumentId, Symbol, Venue},
    instruments::{CurrencyPair, Instrument, InstrumentAny},
    types::{Currency, Money, Price, Quantity},
};
use rust_decimal::Decimal;
use ustr::Ustr;

use crate::sessions::{Segment, SessionSchedule, SessionWindow, date_to_utc_nanos};

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

/// Instrument ID of the synthetic equity.
pub const EQUITY_INSTRUMENT_ID: &str = "SYNTH.XNYS";

/// Bar type of the synthetic equity bars.
pub const EQUITY_BAR_TYPE: &str = "SYNTH.XNYS-5-MINUTE-LAST-EXTERNAL";

/// Price precision of the synthetic equity.
pub const EQUITY_PRICE_PRECISION: u8 = 4;

/// Number of 5-minute bars in each synthetic regular session (09:30 to 16:00).
pub const BARS_PER_SESSION: usize = 78;

const MINUTE_NS: u64 = 60_000_000_000;
const BAR_NS: u64 = 5 * MINUTE_NS;

/// A synthetic US equity with 5-minute bars over consecutive weekdays.
///
/// Sessions start on Monday 2025-01-06 and skip weekends. Each day has a regular segment from
/// 14:30 to 21:00 UTC with [`BARS_PER_SESSION`] bars, plus pre-market (09:00 to 14:30) and
/// after-hours (21:00 to 01:00) segments without bars. Prices follow a slow wave with an
/// overnight gap every day, so moving-average crossovers happen within and across sessions.
#[derive(Debug, Clone)]
pub struct EquityScenario {
    /// The `SYNTH.XNYS` equity.
    pub instrument: InstrumentAny,
    /// The bar type of [`Self::bars`].
    pub bar_type: BarType,
    /// Session windows for every scenario day.
    pub schedule: SessionSchedule,
    /// Bars in time order, `ts_event` and `ts_init` at the bar close.
    pub bars: Vec<Bar>,
}

impl EquityScenario {
    /// Creates the scenario with `sessions` trading days.
    ///
    /// # Errors
    ///
    /// Returns an error if a bar or the instrument fails validation.
    pub fn new(sessions: usize) -> anyhow::Result<Self> {
        let instrument = crate::catalog::equity(
            InstrumentId::from(EQUITY_INSTRUMENT_ID),
            Currency::USD(),
            EQUITY_PRICE_PRECISION,
            UnixNanos::default(),
        )?;
        let bar_type = BarType::from(EQUITY_BAR_TYPE);

        let mut windows = Vec::new();
        let mut bars = Vec::new();
        let mut date = Date::constant(2025, 1, 6);
        let mut close = 50.0;
        for day in 0..sessions {
            while date.weekday().to_monday_one_offset() > 5 {
                date = date.tomorrow()?;
            }
            let midnight = date_to_utc_nanos(date)?.as_u64();
            let at = |minutes: u64| UnixNanos::from(midnight + minutes * MINUTE_NS);
            let open = 14 * 60 + 30;
            let close_minutes = 21 * 60;
            windows.extend([
                SessionWindow {
                    date,
                    segment: Segment::PreMarket,
                    open: at(9 * 60),
                    close: at(open),
                },
                SessionWindow {
                    date,
                    segment: Segment::Regular,
                    open: at(open),
                    close: at(close_minutes),
                },
                SessionWindow {
                    date,
                    segment: Segment::AfterHours,
                    open: at(close_minutes),
                    close: at(25 * 60),
                },
            ]);

            // Overnight gap, alternating in sign
            let gap = if day % 2 == 0 { 0.30 } else { -0.20 };
            let mut prev = close + if day == 0 { 0.0 } else { gap };
            for i in 0..BARS_PER_SESSION {
                let n = (day * BARS_PER_SESSION + i) as f64;
                let next = 50.0 + 3.0 * (n / 40.0).sin() + 0.5 * (n / 7.0).sin() + 0.002 * n;
                let (o, c) = (round_cents(prev), round_cents(next));
                let ts = UnixNanos::from(at(open).as_u64() + (i as u64 + 1) * BAR_NS);
                bars.push(Bar::new_checked(
                    bar_type,
                    price(o),
                    price(o.max(c) + 0.05),
                    price(o.min(c) - 0.05),
                    price(c),
                    Quantity::from(10_000 + (i as u64 % 13) * 500),
                    ts,
                    ts,
                )?);
                prev = next;
            }
            close = prev;
            date = date.tomorrow()?;
        }

        Ok(Self {
            instrument,
            bar_type,
            schedule: SessionSchedule::new(windows)?,
            bars,
        })
    }
}

fn round_cents(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn price(value: f64) -> Price {
    Price::new(value, EQUITY_PRICE_PRECISION)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn equity_scenario_bars_fill_regular_sessions() {
        let scenario = EquityScenario::new(3).unwrap();

        assert_eq!(scenario.bars.len(), 3 * BARS_PER_SESSION);
        for bar in &scenario.bars {
            let start = UnixNanos::from(bar.ts_event.as_u64() - BAR_NS);
            let window = scenario.schedule.locate(start, bar.ts_event).unwrap();
            assert_eq!(window.segment, Segment::Regular);
            assert!(bar.high >= bar.open.max(bar.close));
            assert!(bar.low <= bar.open.min(bar.close));
        }
    }

    #[rstest]
    fn equity_scenario_skips_weekends() {
        let scenario = EquityScenario::new(6).unwrap();

        let days: Vec<_> = scenario
            .schedule
            .windows()
            .iter()
            .filter(|w| w.segment == Segment::Regular)
            .map(|w| w.date.to_string())
            .collect();
        assert_eq!(
            days,
            [
                "2025-01-06",
                "2025-01-07",
                "2025-01-08",
                "2025-01-09",
                "2025-01-10",
                "2025-01-13"
            ]
        );
    }

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
