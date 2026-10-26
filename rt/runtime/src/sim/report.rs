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

//! Results of a bar simulation: fills, round trips, daily returns, and summary metrics.
//!
//! Everything is computed from the [`Ledger`] and the bars, with exact decimal arithmetic for
//! money. Ratios (returns, Sharpe, drawdown, exposure) are `f64`.

use std::collections::BTreeMap;

use jiff::civil::Date;
use nautilus_core::UnixNanos;
use nautilus_model::{
    enums::{OrderSide, PositionSide},
    identifiers::{ClientOrderId, InstrumentId, StrategyId},
    types::Price,
};
use rust_decimal::{
    Decimal,
    prelude::{Signed, ToPrimitive},
};

use super::{
    SessionBar,
    ledger::{BorrowCharge, Fill, Ledger},
};
use crate::sessions::Segment;

/// Trading days per year, for annualizing daily returns.
pub const TRADING_DAYS_PER_YEAR: f64 = 252.0;

/// Counts reported by the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineTotals {
    /// Data points processed (bars and fill quotes).
    pub iterations: usize,
    /// Order and position events.
    pub total_events: usize,
    /// Orders created, including denied ones.
    pub total_orders: usize,
    /// Positions opened.
    pub total_positions: usize,
}

/// A fill with its reference price.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FillRecord {
    /// The fill.
    pub fill: Fill,
    /// Open of the bar the fill happened on, before slippage.
    pub open_price: Decimal,
    /// Session segment of that bar.
    pub segment: Segment,
    /// Cost of the fill price against `open_price`: positive when worse than the open.
    pub slippage: Decimal,
}

/// A position from open to flat, per strategy. A fill that reverses the position closes one
/// round trip and opens the next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundTrip {
    /// Strategy that held the position.
    pub strategy_id: StrategyId,
    /// Instrument traded.
    pub instrument_id: InstrumentId,
    /// Long or short.
    pub side: PositionSide,
    /// Time of the opening fill.
    pub ts_opened: UnixNanos,
    /// Time of the fill that took the position flat, or `None` if still open at the end.
    pub ts_closed: Option<UnixNanos>,
    /// Largest absolute position, in shares.
    pub peak_quantity: Decimal,
    /// Average entry price.
    pub avg_px_open: Decimal,
    /// Average exit price of the closed quantity, if any was closed.
    pub avg_px_close: Option<Decimal>,
    /// Realized profit on the closed quantity, before commissions.
    pub pnl_gross: Decimal,
    /// Commissions of the round trip's fills (pro rata for a reversing fill).
    pub commissions: Decimal,
    /// Slippage cost of the round trip's fills (pro rata for a reversing fill).
    pub slippage: Decimal,
    /// `pnl_gross` less `commissions`.
    pub pnl_net: Decimal,
}

/// Equity at the close of one trading day.
#[derive(Clone, Debug, PartialEq)]
pub struct DailyRecord {
    /// Trading day.
    pub date: Date,
    /// Time of the day's last bar close.
    pub ts: UnixNanos,
    /// Closing price of the day's last bar.
    pub close: Decimal,
    /// Net position at the close, in shares.
    pub position: Decimal,
    /// Cash plus the position marked at `close`.
    pub equity: Decimal,
    /// Change in equity since the previous close (or the starting capital).
    pub pnl: Decimal,
    /// `pnl` as a fraction of the previous equity.
    pub return_: f64,
    /// Borrow charged at this close.
    pub borrow: Decimal,
}

/// An order the simulation denied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denial {
    /// Time of the denial.
    pub ts: UnixNanos,
    /// The denied order.
    pub client_order_id: ClientOrderId,
    /// Why it was denied.
    pub reason: String,
}

/// Summary metrics of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct Metrics {
    /// First trading day with bars.
    pub start_date: Date,
    /// Last trading day with bars.
    pub end_date: Date,
    /// Trading days with bars.
    pub sessions: usize,
    /// Bars simulated.
    pub bars: usize,
    /// Starting cash.
    pub starting_capital: Decimal,
    /// Equity at the last bar close.
    pub ending_equity: Decimal,
    /// `ending_equity - starting_capital`, after all costs.
    pub net_pnl: Decimal,
    /// `net_pnl + total_costs`.
    pub gross_pnl: Decimal,
    /// Commissions, slippage, and borrow.
    pub total_costs: Decimal,
    /// Commissions paid.
    pub commissions: Decimal,
    /// Slippage cost against bar opens.
    pub slippage: Decimal,
    /// Borrow cost of short positions.
    pub borrow: Decimal,
    /// `net_pnl` as a percentage of `starting_capital`.
    pub return_pct: f64,
    /// Annualized Sharpe ratio of daily returns (risk-free rate zero, sample standard
    /// deviation), or `None` with fewer than two days or no variation.
    pub sharpe: Option<f64>,
    /// Largest peak-to-trough fall in equity, marked at every bar close, as a percentage of
    /// the peak.
    pub max_drawdown_pct: f64,
    /// Closed round trips.
    pub trade_count: usize,
    /// Closed round trips with positive net profit.
    pub winning_trades: usize,
    /// Fills.
    pub fill_count: usize,
    /// Orders with at least one fill.
    pub order_count: usize,
    /// Orders denied by the simulation's rules.
    pub denied_orders: usize,
    /// Total value of all fills.
    pub traded_notional: Decimal,
    /// `traded_notional` divided by the average daily equity.
    pub turnover: f64,
    /// Percentage of bar closes with an open position.
    pub exposure_pct: f64,
    /// Average of position value over equity at bar closes.
    pub avg_gross_exposure: f64,
    /// Net position at the end, in shares.
    pub final_position: Decimal,
}

/// Everything a bar simulation produces.
#[derive(Clone, Debug)]
pub struct SimOutcome {
    /// Engine counts.
    pub totals: EngineTotals,
    /// Fills in the order they happened.
    pub fills: Vec<FillRecord>,
    /// Round trips in the order they opened.
    pub round_trips: Vec<RoundTrip>,
    /// One record per trading day.
    pub daily: Vec<DailyRecord>,
    /// Borrow charges.
    pub borrow: Vec<BorrowCharge>,
    /// Orders denied by the simulation.
    pub denials: Vec<Denial>,
    /// Summary metrics.
    pub metrics: Metrics,
    /// Venue account balance at the end, as the engine sees it.
    pub account_balance: Decimal,
}

// Quote the venue fills against: the next bar's open with slippage
pub(crate) struct FillQuote {
    pub(crate) ts: UnixNanos,
    pub(crate) open: Decimal,
    pub(crate) bid: Price,
    pub(crate) ask: Price,
    pub(crate) segment: Segment,
}

pub(crate) fn build(
    starting_capital: Decimal,
    bars: &[SessionBar],
    quotes: &[FillQuote],
    ledger: &Ledger,
    denials: Vec<Denial>,
    totals: EngineTotals,
    account_balance: Decimal,
) -> anyhow::Result<SimOutcome> {
    let fills = fill_records(ledger.fills(), quotes)?;
    let round_trips = round_trips(&fills);
    let borrow = ledger.borrow_charges().to_vec();
    let (daily, marks) = mark_to_market(starting_capital, bars, &fills, &borrow);
    let metrics = metrics(
        starting_capital,
        bars,
        &fills,
        &round_trips,
        &daily,
        &marks,
        &borrow,
        denials.len(),
    )?;
    Ok(SimOutcome {
        totals,
        fills,
        round_trips,
        daily,
        borrow,
        denials,
        metrics,
        account_balance,
    })
}

fn fill_records(fills: &[Fill], quotes: &[FillQuote]) -> anyhow::Result<Vec<FillRecord>> {
    fills
        .iter()
        .map(|fill| {
            let index = quotes.partition_point(|q| q.ts <= fill.ts_event);
            let quote = index
                .checked_sub(1)
                .map(|i| &quotes[i])
                .ok_or_else(|| anyhow::anyhow!("fill {} before any bar open", fill.trade_id))?;
            let slippage = match fill.side {
                OrderSide::Sell => (quote.open - fill.price) * fill.quantity,
                _ => (fill.price - quote.open) * fill.quantity,
            };
            Ok(FillRecord {
                fill: fill.clone(),
                open_price: quote.open,
                segment: quote.segment,
                slippage,
            })
        })
        .collect()
}

#[derive(Default)]
struct TripState {
    quantity: Decimal,
    trip: Option<RoundTrip>,
    closed_quantity: Decimal,
    exit_notional: Decimal,
}

fn round_trips(fills: &[FillRecord]) -> Vec<RoundTrip> {
    let mut states: BTreeMap<(StrategyId, InstrumentId), TripState> = BTreeMap::new();
    let mut done: Vec<RoundTrip> = Vec::new();

    for record in fills {
        let fill = &record.fill;
        let state = states
            .entry((fill.strategy_id, fill.instrument_id))
            .or_default();
        let signed = fill.signed_quantity();
        let mut remaining = fill.quantity;

        // Close (part of) the current position
        if let Some(trip) = state.trip.as_mut()
            && state.quantity.signum() != signed.signum()
        {
            let closing = remaining.min(state.quantity.abs());
            let share = closing / fill.quantity;
            trip.pnl_gross += (fill.price - trip.avg_px_open) * closing * state.quantity.signum();
            trip.commissions += fill.commission * share;
            trip.slippage += record.slippage * share;
            state.closed_quantity += closing;
            state.exit_notional += fill.price * closing;
            state.quantity += closing * signed.signum();
            remaining -= closing;
            if state.quantity.is_zero() {
                let mut trip = state.trip.take().expect("open trip");
                trip.ts_closed = Some(fill.ts_event);
                trip.avg_px_close = Some(state.exit_notional / state.closed_quantity);
                trip.pnl_net = trip.pnl_gross - trip.commissions;
                done.push(trip);
                state.closed_quantity = Decimal::ZERO;
                state.exit_notional = Decimal::ZERO;
            }
        }

        if remaining.is_zero() {
            continue;
        }
        let share = remaining / fill.quantity;
        match state.trip.as_mut() {
            // Add to the position
            Some(trip) => {
                let held = state.quantity.abs();
                trip.avg_px_open =
                    (trip.avg_px_open * held + fill.price * remaining) / (held + remaining);
                trip.commissions += fill.commission * share;
                trip.slippage += record.slippage * share;
                state.quantity += remaining * signed.signum();
                trip.peak_quantity = trip.peak_quantity.max(state.quantity.abs());
            }
            // Open a new position
            None => {
                state.quantity = remaining * signed.signum();
                state.trip = Some(RoundTrip {
                    strategy_id: fill.strategy_id,
                    instrument_id: fill.instrument_id,
                    side: if signed > Decimal::ZERO {
                        PositionSide::Long
                    } else {
                        PositionSide::Short
                    },
                    ts_opened: fill.ts_event,
                    ts_closed: None,
                    peak_quantity: remaining,
                    avg_px_open: fill.price,
                    avg_px_close: None,
                    pnl_gross: Decimal::ZERO,
                    commissions: fill.commission * share,
                    slippage: record.slippage * share,
                    pnl_net: Decimal::ZERO,
                });
            }
        }
    }

    // Positions still open at the end
    for state in states.into_values() {
        if let Some(mut trip) = state.trip {
            if !state.closed_quantity.is_zero() {
                trip.avg_px_close = Some(state.exit_notional / state.closed_quantity);
            }
            trip.pnl_net = trip.pnl_gross - trip.commissions;
            done.push(trip);
        }
    }
    done.sort_by(|a, b| {
        (a.ts_opened, a.strategy_id.as_str()).cmp(&(b.ts_opened, b.strategy_id.as_str()))
    });
    done
}

// Equity and position value at one bar close
struct Mark {
    equity: Decimal,
    position_value: Decimal,
}

fn mark_to_market(
    starting_capital: Decimal,
    bars: &[SessionBar],
    fills: &[FillRecord],
    borrow: &[BorrowCharge],
) -> (Vec<DailyRecord>, Vec<Mark>) {
    let mut cash = starting_capital;
    let mut position = Decimal::ZERO;
    let mut next_fill = 0;
    let mut next_borrow = 0;
    let mut prev_equity = starting_capital;
    let mut daily = Vec::new();
    let mut marks = Vec::with_capacity(bars.len());

    for (index, bar) in bars.iter().enumerate() {
        let ts = bar.bar.ts_init;
        while let Some(record) = fills.get(next_fill).filter(|r| r.fill.ts_event <= ts) {
            let signed = record.fill.signed_quantity();
            cash -= signed * record.fill.price + record.fill.commission;
            position += signed;
            next_fill += 1;
        }
        let mut borrowed = Decimal::ZERO;
        while let Some(charge) = borrow.get(next_borrow).filter(|c| c.ts <= ts) {
            cash -= charge.amount;
            borrowed += charge.amount;
            next_borrow += 1;
        }

        let close = bar.bar.close.as_decimal();
        let equity = cash + position * close;
        marks.push(Mark {
            equity,
            position_value: position * close,
        });

        let session_last = bars.get(index + 1).is_none_or(|next| next.date != bar.date);
        if session_last {
            let pnl = equity - prev_equity;
            daily.push(DailyRecord {
                date: bar.date,
                ts,
                close,
                position,
                equity,
                pnl,
                return_: ratio(pnl, prev_equity),
                borrow: borrowed,
            });
            prev_equity = equity;
        }
    }
    (daily, marks)
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal helper over the run's parts"
)]
fn metrics(
    starting_capital: Decimal,
    bars: &[SessionBar],
    fills: &[FillRecord],
    round_trips: &[RoundTrip],
    daily: &[DailyRecord],
    marks: &[Mark],
    borrow: &[BorrowCharge],
    denied_orders: usize,
) -> anyhow::Result<Metrics> {
    let (first, last) = match (bars.first(), bars.last()) {
        (Some(first), Some(last)) => (first, last),
        _ => anyhow::bail!("no bars to report on"),
    };
    let ending_equity = marks.last().map_or(starting_capital, |m| m.equity);
    let commissions: Decimal = fills.iter().map(|r| r.fill.commission).sum();
    let slippage: Decimal = fills.iter().map(|r| r.slippage).sum();
    let borrow_total: Decimal = borrow.iter().map(|c| c.amount).sum();
    let total_costs = commissions + slippage + borrow_total;
    let net_pnl = ending_equity - starting_capital;
    let traded_notional: Decimal = fills.iter().map(|r| r.fill.price * r.fill.quantity).sum();

    let returns: Vec<f64> = daily.iter().map(|d| d.return_).collect();
    let mean_equity = daily.iter().map(|d| d.equity).sum::<Decimal>() / Decimal::from(daily.len());

    let mut peak = starting_capital;
    let mut max_drawdown = 0.0_f64;
    let mut in_market = 0usize;
    let mut gross_exposure = 0.0_f64;
    for mark in marks {
        peak = peak.max(mark.equity);
        max_drawdown = max_drawdown.max(ratio(peak - mark.equity, peak));
        if !mark.position_value.is_zero() {
            in_market += 1;
        }
        gross_exposure += ratio(mark.position_value.abs(), mark.equity);
    }

    let mut order_ids: Vec<_> = fills.iter().map(|r| r.fill.client_order_id).collect();
    order_ids.sort_unstable();
    order_ids.dedup();
    let closed: Vec<_> = round_trips
        .iter()
        .filter(|t| t.ts_closed.is_some())
        .collect();

    Ok(Metrics {
        start_date: first.date,
        end_date: last.date,
        sessions: daily.len(),
        bars: bars.len(),
        starting_capital,
        ending_equity,
        net_pnl,
        gross_pnl: net_pnl + total_costs,
        total_costs,
        commissions,
        slippage,
        borrow: borrow_total,
        return_pct: ratio(net_pnl, starting_capital) * 100.0,
        sharpe: sharpe(&returns),
        max_drawdown_pct: max_drawdown * 100.0,
        trade_count: closed.len(),
        winning_trades: closed.iter().filter(|t| t.pnl_net > Decimal::ZERO).count(),
        fill_count: fills.len(),
        order_count: order_ids.len(),
        denied_orders,
        traded_notional,
        turnover: ratio(traded_notional, mean_equity),
        exposure_pct: in_market as f64 / marks.len() as f64 * 100.0,
        avg_gross_exposure: gross_exposure / marks.len() as f64,
        final_position: fills.iter().map(|r| r.fill.signed_quantity()).sum(),
    })
}

/// Returns the annualized Sharpe ratio of daily `returns`, or `None` with fewer than two
/// returns or zero variance.
#[must_use]
pub fn sharpe(returns: &[f64]) -> Option<f64> {
    if returns.len() < 2 {
        return None;
    }
    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;
    let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let std = variance.sqrt();
    (std > 0.0).then(|| mean / std * TRADING_DAYS_PER_YEAR.sqrt())
}

fn ratio(numerator: Decimal, denominator: Decimal) -> f64 {
    if denominator.is_zero() {
        return 0.0;
    }
    (numerator / denominator).to_f64().unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    use nautilus_model::identifiers::{TradeId, VenueOrderId};
    use rstest::rstest;
    use rust_decimal_macros::dec;

    use super::*;

    fn record(ts: u64, side: OrderSide, quantity: Decimal, price: Decimal) -> FillRecord {
        FillRecord {
            fill: Fill {
                ts_event: UnixNanos::from(ts),
                strategy_id: StrategyId::from("S-001"),
                instrument_id: InstrumentId::from("SYNTH.XNYS"),
                client_order_id: ClientOrderId::new(format!("O-{ts}")),
                venue_order_id: VenueOrderId::from("V-1"),
                trade_id: TradeId::from("T-1"),
                side,
                quantity,
                price,
                commission: dec!(1),
                flatten: false,
            },
            open_price: price,
            segment: Segment::Regular,
            slippage: dec!(0.5),
        }
    }

    #[rstest]
    fn round_trip_long_then_flat() {
        let fills = [
            record(1, OrderSide::Buy, dec!(100), dec!(10)),
            record(2, OrderSide::Buy, dec!(100), dec!(12)),
            record(3, OrderSide::Sell, dec!(200), dec!(13)),
        ];

        let trips = round_trips(&fills);

        assert_eq!(trips.len(), 1);
        let trip = &trips[0];
        assert_eq!(trip.side, PositionSide::Long);
        assert_eq!(trip.avg_px_open, dec!(11));
        assert_eq!(trip.avg_px_close, Some(dec!(13)));
        assert_eq!(trip.peak_quantity, dec!(200));
        assert_eq!(trip.pnl_gross, dec!(400));
        assert_eq!(trip.commissions, dec!(3));
        assert_eq!(trip.pnl_net, dec!(397));
        assert_eq!(trip.ts_closed, Some(UnixNanos::from(3)));
    }

    #[rstest]
    fn reversing_fill_splits_round_trips() {
        let fills = [
            record(1, OrderSide::Buy, dec!(100), dec!(10)),
            record(2, OrderSide::Sell, dec!(300), dec!(9)),
            record(3, OrderSide::Buy, dec!(200), dec!(8)),
        ];

        let trips = round_trips(&fills);

        assert_eq!(trips.len(), 2);
        assert_eq!(trips[0].side, PositionSide::Long);
        assert_eq!(trips[0].pnl_gross, dec!(-100));
        // One third of the reversing fill's commission closes the long
        assert_eq!(trips[0].commissions, dec!(1) + dec!(1) / dec!(3));
        assert_eq!(trips[1].side, PositionSide::Short);
        assert_eq!(trips[1].avg_px_open, dec!(9));
        assert_eq!(trips[1].pnl_gross, dec!(200));
        assert_eq!(trips[1].peak_quantity, dec!(200));
    }

    #[rstest]
    fn open_round_trip_is_reported() {
        let fills = [record(1, OrderSide::Sell, dec!(50), dec!(10))];

        let trips = round_trips(&fills);

        assert_eq!(trips.len(), 1);
        assert_eq!(trips[0].ts_closed, None);
        assert_eq!(trips[0].avg_px_close, None);
    }

    #[rstest]
    fn sharpe_annualizes_daily_returns() {
        let returns = [0.01_f64, -0.005, 0.02, 0.0];
        let n = 4.0_f64;
        let mean = 0.025 / n;
        let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);

        let value = sharpe(&returns).unwrap();

        assert!((value - mean / var.sqrt() * 252f64.sqrt()).abs() < 1e-12);
    }

    #[rstest]
    #[case(&[])]
    #[case(&[0.01])]
    #[case(&[0.01, 0.01])]
    fn sharpe_undefined(#[case] returns: &[f64]) {
        assert_eq!(sharpe(returns), None);
    }
}
