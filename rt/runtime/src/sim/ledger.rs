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

//! Exact record of fills, cash, positions, and borrow charges during a bar simulation.
//!
//! The ledger is the simulation's source of truth for equity. It is fed by the engine's order
//! fill events, holds every amount as a [`Decimal`], and works the same for margin and cash
//! accounts: equity is cash plus the net position marked at a price.

use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    rc::Rc,
};

use jiff::civil::Date;
use nautilus_backtest::modules::{
    AccountAdjustmentOutcome, ExchangeContext, SimulationModule, SimulationModuleResult,
};
use nautilus_common::msgbus::{self, TypedHandler};
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::Data,
    enums::OrderSide,
    events::{OrderEventAny, OrderFilled},
    identifiers::{ClientOrderId, InstrumentId, StrategyId, TradeId, VenueOrderId},
    types::{Currency, Money},
};
use rust_decimal::{Decimal, RoundingStrategy};

/// Client order ID prefix of the orders the simulation submits to flatten positions.
pub const FLATTEN_ORDER_PREFIX: &str = "FLATTEN-";

/// One fill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fill {
    /// Fill time.
    pub ts_event: UnixNanos,
    /// Strategy that owns the order.
    pub strategy_id: StrategyId,
    /// Instrument filled.
    pub instrument_id: InstrumentId,
    /// Order ID assigned by the strategy (or the simulation, for flatten orders).
    pub client_order_id: ClientOrderId,
    /// Order ID assigned by the venue.
    pub venue_order_id: VenueOrderId,
    /// Trade ID assigned by the venue.
    pub trade_id: TradeId,
    /// Buy or sell.
    pub side: OrderSide,
    /// Shares filled.
    pub quantity: Decimal,
    /// Fill price.
    pub price: Decimal,
    /// Commission charged for this fill, in the account currency.
    pub commission: Decimal,
    /// Whether the simulation submitted the order to flatten a position at the session end.
    pub flatten: bool,
}

impl Fill {
    /// Returns the quantity signed by side: positive for buys.
    #[must_use]
    pub fn signed_quantity(&self) -> Decimal {
        match self.side {
            OrderSide::Sell => -self.quantity,
            _ => self.quantity,
        }
    }
}

/// Borrow cost charged on a short position at a session close.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BorrowCharge {
    /// Time of the session's last bar close.
    pub ts: UnixNanos,
    /// Trading day charged.
    pub date: Date,
    /// Shares short at the close.
    pub short_quantity: Decimal,
    /// Closing price the position was valued at.
    pub price: Decimal,
    /// Calendar days charged: until the next trading day.
    pub days: i64,
    /// Amount charged, in the account currency.
    pub amount: Decimal,
}

/// Fills, cash, and positions of one simulation.
#[derive(Debug)]
pub struct Ledger {
    currency: Currency,
    cash: Decimal,
    positions: BTreeMap<StrategyId, Decimal>,
    fills: Vec<Fill>,
    borrow: Vec<BorrowCharge>,
    error: Option<String>,
}

impl Ledger {
    /// Creates a ledger holding `starting_cash` in `currency`.
    #[must_use]
    pub fn new(currency: Currency, starting_cash: Decimal) -> Self {
        Self {
            currency,
            cash: starting_cash,
            positions: BTreeMap::new(),
            fills: Vec::new(),
            borrow: Vec::new(),
            error: None,
        }
    }

    /// Returns the cash balance: starting cash less net purchases, commissions, and borrow.
    #[must_use]
    pub const fn cash(&self) -> Decimal {
        self.cash
    }

    /// Returns the net position across all strategies, in shares (negative when short).
    #[must_use]
    pub fn net_quantity(&self) -> Decimal {
        self.positions.values().sum()
    }

    /// Returns each strategy's position, in shares.
    pub fn positions(&self) -> impl Iterator<Item = (StrategyId, Decimal)> + '_ {
        self.positions.iter().map(|(id, qty)| (*id, *qty))
    }

    /// Returns equity with the net position marked at `price`.
    #[must_use]
    pub fn equity(&self, price: Decimal) -> Decimal {
        self.cash + self.net_quantity() * price
    }

    /// Returns all fills in the order they happened.
    #[must_use]
    pub fn fills(&self) -> &[Fill] {
        &self.fills
    }

    /// Returns all borrow charges in time order.
    #[must_use]
    pub fn borrow_charges(&self) -> &[BorrowCharge] {
        &self.borrow
    }

    /// Returns the first inconsistency found while recording, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Records a fill.
    pub fn record_fill(&mut self, fill: Fill) {
        let signed = fill.signed_quantity();
        self.cash -= signed * fill.price + fill.commission;
        *self.positions.entry(fill.strategy_id).or_default() += signed;
        self.fills.push(fill);
    }

    /// Records a borrow charge.
    pub fn record_borrow(&mut self, charge: BorrowCharge) {
        self.cash -= charge.amount;
        self.borrow.push(charge);
    }

    fn record_fill_event(&mut self, event: &OrderFilled) {
        let commission = match event.commission {
            Some(money) if money.currency != self.currency => {
                self.error.get_or_insert_with(|| {
                    format!(
                        "fill {} charged commission in {}, not the account currency {}",
                        event.trade_id, money.currency, self.currency
                    )
                });
                return;
            }
            Some(money) => money.as_decimal(),
            None => Decimal::ZERO,
        };
        self.record_fill(Fill {
            ts_event: event.ts_event,
            strategy_id: event.strategy_id,
            instrument_id: event.instrument_id,
            client_order_id: event.client_order_id,
            venue_order_id: event.venue_order_id,
            trade_id: event.trade_id,
            side: event.order_side,
            quantity: event.last_qty.as_decimal(),
            price: event.last_px.as_decimal(),
            commission,
            flatten: event
                .client_order_id
                .as_str()
                .starts_with(FLATTEN_ORDER_PREFIX),
        });
    }

    /// Subscribes `ledger` to the fills of every strategy.
    ///
    /// The handler runs before strategies see the fill, so their callbacks observe the updated
    /// ledger.
    pub fn subscribe(ledger: &Rc<RefCell<Self>>) {
        let ledger = Rc::clone(ledger);
        msgbus::subscribe_order_events(
            "events.order.*".into(),
            TypedHandler::from(move |event: &OrderEventAny| {
                if let OrderEventAny::Filled(fill) = event {
                    ledger.borrow_mut().record_fill_event(fill);
                }
            }),
            Some(100),
        );
    }
}

/// The last bar close of a session, where borrow is charged.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SessionClose {
    pub(crate) date: Date,
    pub(crate) price: Decimal,
    pub(crate) days: i64,
}

/// Simulation module charging borrow cost on short positions at each session close.
///
/// The charge is `short shares * close * bps / 10_000 * days / 365`, where `days` counts
/// calendar days to the next trading day, rounded to the currency's precision. The module
/// applies it to the venue account and records it in the ledger.
pub(crate) struct BorrowModule {
    ledger: Rc<RefCell<Ledger>>,
    closes: BTreeMap<UnixNanos, SessionClose>,
    bps_per_year: Decimal,
    currency: Currency,
    pending: RefCell<VecDeque<BorrowCharge>>,
}

impl BorrowModule {
    pub(crate) fn new(
        ledger: Rc<RefCell<Ledger>>,
        closes: BTreeMap<UnixNanos, SessionClose>,
        bps_per_year: Decimal,
        currency: Currency,
    ) -> Self {
        Self {
            ledger,
            closes,
            bps_per_year,
            currency,
            pending: RefCell::new(VecDeque::new()),
        }
    }

    fn charge(&self, ts: UnixNanos) -> Option<BorrowCharge> {
        let close = self.closes.get(&ts)?;
        let short_quantity = (-self.ledger.borrow().net_quantity()).max(Decimal::ZERO);
        let amount = (short_quantity * close.price * self.bps_per_year / Decimal::from(10_000)
            * Decimal::from(close.days)
            / Decimal::from(365))
        .round_dp_with_strategy(
            u32::from(self.currency.precision),
            RoundingStrategy::MidpointAwayFromZero,
        );
        (amount > Decimal::ZERO).then_some(BorrowCharge {
            ts,
            date: close.date,
            short_quantity,
            price: close.price,
            days: close.days,
            amount,
        })
    }
}

impl SimulationModule for BorrowModule {
    fn pre_process(&self, _data: &Data) -> anyhow::Result<()> {
        Ok(())
    }

    fn process(
        &self,
        ts_now: UnixNanos,
        _ctx: &ExchangeContext,
    ) -> anyhow::Result<SimulationModuleResult> {
        let Some(charge) = self.charge(ts_now) else {
            return Ok(SimulationModuleResult::Completed(Vec::new()));
        };
        let adjustment = Money::from_decimal(-charge.amount, self.currency)?;
        self.pending.borrow_mut().push_back(charge);
        Ok(SimulationModuleResult::Completed(vec![adjustment]))
    }

    fn acknowledge(&self, outcomes: &[AccountAdjustmentOutcome]) -> anyhow::Result<()> {
        if outcomes.is_empty() {
            return Ok(());
        }
        let charge = self
            .pending
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("borrow outcome without a pending charge"))?;
        match outcomes {
            [AccountAdjustmentOutcome::Applied] => {
                self.ledger.borrow_mut().record_borrow(charge);
                Ok(())
            }
            [AccountAdjustmentOutcome::Failed(e)] => {
                anyhow::bail!("borrow charge on {} failed: {e}", charge.date)
            }
            _ => anyhow::bail!("expected one borrow outcome, got {}", outcomes.len()),
        }
    }

    fn log_diagnostics(&self) -> anyhow::Result<()> {
        let ledger = self.ledger.borrow();
        let total: Decimal = ledger.borrow_charges().iter().map(|c| c.amount).sum();
        log::info!(
            "Borrow charged {total} {} over {} sessions",
            self.currency,
            ledger.borrow_charges().len()
        );
        Ok(())
    }

    fn reset(&self) -> anyhow::Result<()> {
        self.pending.borrow_mut().clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rust_decimal_macros::dec;

    use super::*;

    fn fill(strategy: &str, side: OrderSide, quantity: Decimal, price: Decimal) -> Fill {
        Fill {
            ts_event: UnixNanos::from(1),
            strategy_id: StrategyId::from(strategy),
            instrument_id: InstrumentId::from("SYNTH.XNYS"),
            client_order_id: ClientOrderId::from("O-1"),
            venue_order_id: VenueOrderId::from("V-1"),
            trade_id: TradeId::from("T-1"),
            side,
            quantity,
            price,
            commission: dec!(1),
            flatten: false,
        }
    }

    #[rstest]
    fn record_fill_updates_cash_and_positions() {
        let mut ledger = Ledger::new(Currency::USD(), dec!(10000));

        ledger.record_fill(fill("A-1", OrderSide::Buy, dec!(100), dec!(50)));
        ledger.record_fill(fill("B-1", OrderSide::Sell, dec!(30), dec!(51)));

        assert_eq!(
            ledger.cash(),
            dec!(10000) - dec!(5000) - dec!(1) + dec!(1530) - dec!(1)
        );
        assert_eq!(ledger.net_quantity(), dec!(70));
        assert_eq!(
            ledger.positions().collect::<Vec<_>>(),
            vec![
                (StrategyId::from("A-1"), dec!(100)),
                (StrategyId::from("B-1"), dec!(-30))
            ]
        );
        assert_eq!(ledger.equity(dec!(52)), ledger.cash() + dec!(70) * dec!(52));
    }

    #[rstest]
    fn borrow_module_charges_short_positions() {
        let ledger = Rc::new(RefCell::new(Ledger::new(Currency::USD(), dec!(10000))));
        ledger
            .borrow_mut()
            .record_fill(fill("A-1", OrderSide::Sell, dec!(100), dec!(50)));
        let ts = UnixNanos::from(10);
        let close = SessionClose {
            date: Date::constant(2025, 1, 10),
            price: dec!(40),
            days: 3,
        };
        let module = BorrowModule::new(
            Rc::clone(&ledger),
            BTreeMap::from([(ts, close)]),
            dec!(50),
            Currency::USD(),
        );

        let charge = module.charge(ts).unwrap();

        // 100 * 40 * 0.005 * 3 / 365 = 0.1643..., rounded to cents
        assert_eq!(charge.amount, dec!(0.16));
        assert_eq!(charge.days, 3);
        assert!(module.charge(UnixNanos::from(11)).is_none());
    }

    #[rstest]
    fn borrow_module_ignores_long_positions() {
        let ledger = Rc::new(RefCell::new(Ledger::new(Currency::USD(), dec!(10000))));
        ledger
            .borrow_mut()
            .record_fill(fill("A-1", OrderSide::Buy, dec!(100), dec!(50)));
        let ts = UnixNanos::from(10);
        let close = SessionClose {
            date: Date::constant(2025, 1, 10),
            price: dec!(40),
            days: 1,
        };
        let module = BorrowModule::new(
            ledger,
            BTreeMap::from([(ts, close)]),
            dec!(50),
            Currency::USD(),
        );

        assert!(module.charge(ts).is_none());
    }
}
