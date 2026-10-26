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

//! Bar backtests with the Bullpen fill model and account and session rules.
//!
//! [`BarSimulation`] runs strategies on bars of one instrument:
//!
//! - **Fills at the next bar's open.** The venue never sees bars. One nanosecond after each bar
//!   closes, it receives a quote built from the *next* bar's open, widened by that bar's
//!   segment slippage (ask rounded up, bid rounded down to the price increment). Orders arrive at
//!   the venue one nanosecond after they are sent, so an order sent on a bar close fills against
//!   that quote: at the next bar's open plus slippage, never on its own bar. Resting limit and
//!   stop orders are likewise only matched against bar opens, not intrabar highs and lows. The
//!   next bar is the next one in the data, so an order sent on a session's last bar fills at the
//!   next session's first bar, overnight gap included.
//! - **Costs.** Commission per share with a minimum per order ([`fees::PerShareFeeModel`]),
//!   slippage as above, and borrow on short positions charged at each session close.
//! - **Account rules.** Shorting and position-size limits are checked before each order (see
//!   the gate module docs).
//! - **Sessions.** Bars must lie inside the [`SessionSchedule`]. With
//!   [`Overnight::Flatten`], the simulation cancels working orders and closes positions at each
//!   session's second-to-last bar close, so the exit fills on the session's last bar, and
//!   denies strategy orders until the next session.
//!
//! The [`Ledger`] records fills and cash exactly, and [`SimOutcome`] reports on them.

pub mod config;
pub mod fees;
pub mod ledger;
pub mod report;

mod flatten;
mod gate;
#[cfg(test)]
mod tests;

use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use anyhow::Context;
use jiff::civil::Date;
use nautilus_backtest::{
    config::{BacktestEngineConfig, SimulatedVenueConfig},
    engine::BacktestEngine,
    modules::SimulationModuleHandle,
};
use nautilus_core::{DurationNanos, UnixNanos};
use nautilus_execution::models::{
    fee::FeeModelHandle,
    fill::{DefaultFillModel, FillModelHandle},
    latency::{LatencyModelHandle, StaticLatencyModel},
};
use nautilus_model::{
    accounts::Account,
    data::{Bar, BarType, Data, QuoteTick},
    enums::{BookType, OmsType},
    instruments::{Instrument, InstrumentAny},
    types::{Money, Price, Quantity},
};
use rust_decimal::{Decimal, RoundingStrategy};

pub use self::{
    config::{
        CommissionConfig, FillModelConfig, FillModelType, Overnight, SimConfig, SlippageConfig,
        VenueConfig,
    },
    ledger::{BorrowCharge, FLATTEN_ORDER_PREFIX, Fill, Ledger},
    report::{
        DailyRecord, Denial, EngineTotals, FillRecord, Metrics, RoundTrip, SimOutcome,
        TRADING_DAYS_PER_YEAR,
    },
};
use self::{
    flatten::SessionFlattener,
    gate::{BarSlot, GateRules, OrderGate},
    ledger::{BorrowModule, SessionClose},
    report::FillQuote,
};
use crate::sessions::{Segment, SessionSchedule};

const ONE_NANO: DurationNanos = DurationNanos::new(1);

/// Size of the fill quotes. Large enough that no order is limited by it.
const QUOTE_SIZE: f64 = 1_000_000_000.0;

/// A bar with the trading day and segment it belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionBar {
    /// The bar.
    pub bar: Bar,
    /// Trading day.
    pub date: Date,
    /// Session segment.
    pub segment: Segment,
}

/// A backtest of strategies on bars of one instrument. See the module docs.
///
/// Create it, add strategies through [`Self::engine_mut`] (for example with
/// [`StrategyRegistrar`](crate::sdk::StrategyRegistrar)), then [`run`](Self::run) it.
#[derive(Debug)]
pub struct BarSimulation {
    engine: BacktestEngine,
    starting_capital: Decimal,
    instrument: InstrumentAny,
    bars: Vec<SessionBar>,
    quotes: Vec<FillQuote>,
    ledger: Rc<RefCell<Ledger>>,
    gate: Rc<OrderGate>,
}

impl std::fmt::Debug for FillQuote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(FillQuote))
            .field("ts", &self.ts)
            .field("open", &self.open)
            .finish()
    }
}

impl std::fmt::Debug for OrderGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(OrderGate)).finish()
    }
}

impl BarSimulation {
    /// Creates a simulation with the default engine config.
    ///
    /// # Errors
    ///
    /// See [`Self::with_engine_config`].
    pub fn new(
        config: &SimConfig,
        instrument: InstrumentAny,
        bars: Vec<Bar>,
        schedule: &SessionSchedule,
    ) -> anyhow::Result<Self> {
        Self::with_engine_config(
            config,
            BacktestEngineConfig::default(),
            instrument,
            bars,
            schedule,
        )
    }

    /// Creates a simulation of `bars` of `instrument`.
    ///
    /// # Errors
    ///
    /// Returns an error if the config is invalid, the account currency is not the instrument's
    /// quote currency, there are no bars, the bars are of mixed types or out of order, a bar
    /// lies outside every session window, or the engine rejects the setup.
    pub fn with_engine_config(
        config: &SimConfig,
        engine_config: BacktestEngineConfig,
        instrument: InstrumentAny,
        bars: Vec<Bar>,
        schedule: &SessionSchedule,
    ) -> anyhow::Result<Self> {
        config.validate()?;
        let venue_config = &config.venue;
        let currency = venue_config.base_currency;
        anyhow::ensure!(
            instrument.quote_currency() == currency,
            "venue.base_currency {currency} is not the quote currency {} of {}",
            instrument.quote_currency(),
            instrument.id()
        );
        let bars = assign_sessions(&instrument, bars, schedule)?;
        let quotes = fill_quotes(&instrument, &bars, &config.fill_model.slippage_bps)?;
        let ledger = Rc::new(RefCell::new(Ledger::new(
            currency,
            venue_config.starting_capital,
        )));

        let borrow = BorrowModule::new(
            Rc::clone(&ledger),
            session_closes(&bars, schedule),
            venue_config.short_borrow_bps_per_year,
            currency,
        );
        let mut engine = BacktestEngine::new(engine_config)?;
        engine.add_venue(
            SimulatedVenueConfig::builder()
                .venue(instrument.id().venue)
                .oms_type(OmsType::Netting)
                .account_type(venue_config.account_type)
                .book_type(BookType::L1_MBP)
                .starting_balances(vec![Money::from_decimal(
                    venue_config.starting_capital,
                    currency,
                )?])
                .base_currency(currency)
                .modules(vec![SimulationModuleHandle::new(borrow)])
                .fill_model(FillModelHandle::new(DefaultFillModel::new(
                    1.0,
                    0.0,
                    Some(config.seed),
                )?))
                .fee_model(FeeModelHandle::new(fees::PerShareFeeModel::new(
                    &config.fill_model.commission,
                )))
                .latency_model(LatencyModelHandle::new(StaticLatencyModel::new(
                    DurationNanos::new(0),
                    DurationNanos::new(1), // insert: fill at the next bar's open
                    DurationNanos::new(1), // update: same as insert
                    DurationNanos::new(0), // cancel: before the next open
                )))
                .bar_execution(false)
                .trade_execution(false)
                // The gate limits exposure; a fill above the last close must not abort the run
                .allow_cash_borrowing(true)
                .build()?,
        )?;
        engine.add_instrument(&instrument)?;
        engine.add_data(feed(&instrument, &bars, &quotes)?, None, true, true)?;

        let kernel = engine.kernel_mut();
        let gate = Rc::new(OrderGate::new(
            GateRules {
                instrument_id: instrument.id(),
                overnight: config.overnight,
                allow_short: venue_config.allow_short,
                max_exposure_fraction: venue_config.max_exposure_fraction(),
            },
            bar_slots(&bars),
            Rc::clone(&kernel.cache),
            Rc::clone(&kernel.clock),
            Rc::clone(&ledger),
        ));
        OrderGate::install(&gate);
        Ledger::subscribe(&ledger);
        if config.overnight == Overnight::Flatten {
            engine.add_actor(SessionFlattener::new(
                instrument.id(),
                flatten_triggers(&bars),
                Rc::clone(&ledger),
            ))?;
        }

        Ok(Self {
            engine,
            starting_capital: venue_config.starting_capital,
            instrument,
            bars,
            quotes,
            ledger,
            gate,
        })
    }

    /// Returns the engine, to add strategies.
    pub fn engine_mut(&mut self) -> &mut BacktestEngine {
        &mut self.engine
    }

    /// Returns the bars with their sessions.
    #[must_use]
    pub fn bars(&self) -> &[SessionBar] {
        &self.bars
    }

    /// Runs the backtest to the last bar and reports on it.
    ///
    /// # Errors
    ///
    /// Returns an error if the engine fails or the ledger found an inconsistency.
    pub fn run(mut self) -> anyhow::Result<SimOutcome> {
        self.engine.run(None, None, None, false)?;
        if let Some(error) = self.ledger.borrow().error() {
            anyhow::bail!("{error}");
        }

        let result = self.engine.get_result();
        let totals = EngineTotals {
            iterations: result.iterations,
            total_events: result.total_events,
            total_orders: result.total_orders,
            total_positions: result.total_positions,
        };
        let venue = self.instrument.id().venue;
        let currency = self.instrument.quote_currency();
        let account_balance = self
            .engine
            .kernel_mut()
            .cache
            .borrow()
            .account_for_venue(&venue)
            .and_then(|account| account.balance_total(Some(currency)))
            .map(|money| money.as_decimal())
            .context("no venue account balance")?;
        let denials = self
            .gate
            .denials()
            .into_iter()
            .map(|(ts, client_order_id, reason)| report::Denial {
                ts,
                client_order_id,
                reason,
            })
            .collect();

        report::build(
            self.starting_capital,
            &self.bars,
            &self.quotes,
            &self.ledger.borrow(),
            denials,
            totals,
            account_balance,
        )
    }
}

// Validates the bars and finds the session window of each
fn assign_sessions(
    instrument: &InstrumentAny,
    bars: Vec<Bar>,
    schedule: &SessionSchedule,
) -> anyhow::Result<Vec<SessionBar>> {
    let first = bars.first().context("no bars to simulate")?;
    let bar_type = first.bar_type;
    anyhow::ensure!(
        bar_type.instrument_id() == instrument.id(),
        "bar type {bar_type} is not for instrument {}",
        instrument.id()
    );
    let interval = bar_interval(bar_type)?;

    let mut previous: Option<UnixNanos> = None;
    bars.into_iter()
        .map(|bar| {
            anyhow::ensure!(
                bar.bar_type == bar_type,
                "mixed bar types {bar_type} and {}",
                bar.bar_type
            );
            anyhow::ensure!(
                previous.is_none_or(|ts| bar.ts_init > ts),
                "bars are not in strictly increasing time order at {}",
                bar.ts_init
            );
            previous = Some(bar.ts_init);
            let start = UnixNanos::from(bar.ts_init.as_u64().saturating_sub(interval));
            let window = schedule.locate(start, bar.ts_init).with_context(|| {
                format!(
                    "bar closing at {} is outside every session window",
                    bar.ts_init
                )
            })?;
            Ok(SessionBar {
                bar,
                date: window.date,
                segment: window.segment,
            })
        })
        .collect()
}

fn bar_interval(bar_type: BarType) -> anyhow::Result<u64> {
    let spec = bar_type.spec();
    let interval = u64::try_from(spec.timedelta().as_nanos())
        .ok()
        .filter(|ns| *ns > 0)
        .with_context(|| format!("bar type {bar_type} is not time-based"))?;
    Ok(interval)
}

// One fill quote per bar after the first: its open widened by its segment's slippage, sent one
// nanosecond after the previous bar closes
fn fill_quotes(
    instrument: &InstrumentAny,
    bars: &[SessionBar],
    slippage: &SlippageConfig,
) -> anyhow::Result<Vec<FillQuote>> {
    let precision = instrument.price_precision();
    let dp = u32::from(precision);
    bars.windows(2)
        .map(|pair| {
            let (previous, next) = (&pair[0], &pair[1]);
            let open = next.bar.open.as_decimal();
            let fraction = slippage.bps(next.segment) / Decimal::from(10_000);
            let ask = (open * (Decimal::ONE + fraction))
                .round_dp_with_strategy(dp, RoundingStrategy::AwayFromZero);
            let bid = (open * (Decimal::ONE - fraction))
                .round_dp_with_strategy(dp, RoundingStrategy::ToZero);
            anyhow::ensure!(
                bid > Decimal::ZERO,
                "slippage makes the bid of the bar closing at {} non-positive",
                next.bar.ts_init
            );
            Ok(FillQuote {
                ts: previous.bar.ts_init + ONE_NANO,
                open,
                bid: Price::from_decimal_dp(bid, precision)?,
                ask: Price::from_decimal_dp(ask, precision)?,
                segment: next.segment,
            })
        })
        .collect()
}

// Bars for strategies and fill quotes for the venue, in time order
fn feed(
    instrument: &InstrumentAny,
    bars: &[SessionBar],
    quotes: &[FillQuote],
) -> anyhow::Result<Vec<Data>> {
    let size = Quantity::new(QUOTE_SIZE, instrument.size_precision());
    let mut data = Vec::with_capacity(bars.len() + quotes.len());
    let mut quotes = quotes.iter().peekable();
    for bar in bars {
        data.push(Data::Bar(bar.bar));
        if let Some(quote) = quotes.next_if(|q| q.ts == bar.bar.ts_init + ONE_NANO) {
            data.push(Data::Quote(QuoteTick::new_checked(
                instrument.id(),
                quote.bid,
                quote.ask,
                size,
                size,
                quote.ts,
                quote.ts,
            )?));
        }
    }
    Ok(data)
}

fn bar_slots(bars: &[SessionBar]) -> Vec<BarSlot> {
    let mut session_end = vec![false; bars.len()];
    for index in session_last_indices(bars) {
        session_end[index] = true;
        if index > 0 {
            session_end[index - 1] = true;
        }
    }
    bars.iter()
        .enumerate()
        .map(|(index, bar)| BarSlot {
            close_ts: bar.bar.ts_init,
            close_price: bar.bar.close.as_decimal(),
            session_end: session_end[index],
            last: index + 1 == bars.len(),
        })
        .collect()
}

// Second-to-last bar close of every session
fn flatten_triggers(bars: &[SessionBar]) -> Vec<UnixNanos> {
    let mut triggers: Vec<_> = session_last_indices(bars)
        .filter_map(|index| index.checked_sub(1))
        .map(|index| bars[index].bar.ts_init)
        .collect();
    triggers.dedup();
    triggers
}

fn session_closes(
    bars: &[SessionBar],
    schedule: &SessionSchedule,
) -> BTreeMap<UnixNanos, SessionClose> {
    session_last_indices(bars)
        .map(|index| {
            let bar = &bars[index];
            let days = schedule
                .next_date_after(bar.date)
                .and_then(|next| bar.date.until(next).ok())
                .map_or(1, |span| i64::from(span.get_days()));
            (
                bar.bar.ts_init,
                SessionClose {
                    date: bar.date,
                    price: bar.bar.close.as_decimal(),
                    days,
                },
            )
        })
        .collect()
}

fn session_last_indices(bars: &[SessionBar]) -> impl Iterator<Item = usize> + '_ {
    (0..bars.len()).filter(|&index| {
        bars.get(index + 1)
            .is_none_or(|next| next.date != bars[index].date)
    })
}
