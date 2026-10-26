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

//! Pre-trade gate enforcing the simulation's timing, session, and account rules.
//!
//! The gate sits in front of the engine's risk engine: it takes over the
//! `RiskEngine.queue_execute` endpoint that strategies send commands to (and the order
//! emulator's endpoint), checks each order, and either forwards it to `RiskEngine.execute` or
//! denies it with an `OrderDenied` event. Checks run deferred, after the strategy callback that
//! sent the command returns, like the risk engine's own queued path.
//!
//! An order is accepted only if:
//!
//! - it is sent at a bar close (or in a callback one nanosecond later, when fills of orders
//!   from that close arrive). It then fills at the next bar's open. Orders sent between bars
//!   would fill at a stale open, so they are denied.
//! - a later bar exists to fill it.
//! - with `overnight = flatten`, it is not sent at either of a session's last two bar closes.
//! - it is not emulated.
//! - it keeps the position within the short-selling and exposure limits, valued at the last
//!   bar close. Reduce-only orders and contingent child orders are not limited.

use std::{cell::RefCell, rc::Rc};

use nautilus_common::{
    cache::Cache,
    clock::Clock,
    messages::execution::{ModifyOrder, TradingCommand},
    msgbus::{self, MessagingSwitchboard, TypedIntoHandler},
    runner::{TradingCommandMessage, try_get_trading_cmd_sender},
};
use nautilus_core::{UUID4, UnixNanos};
use nautilus_model::{
    enums::{OrderSide, OrderStatus},
    events::{OrderDenied, OrderEventAny, OrderModifyRejected},
    identifiers::{ClientOrderId, InstrumentId},
    orders::{Order, OrderAny},
};
use rust_decimal::Decimal;

use super::{config::Overnight, ledger::Ledger};

const GATE_ENDPOINT: &str = "BarSimulation.gate";

/// What the gate knows about one bar close.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BarSlot {
    pub(crate) close_ts: UnixNanos,
    pub(crate) close_price: Decimal,
    /// One of the session's last two bars, when orders are blocked under `flatten`.
    pub(crate) session_end: bool,
    /// The final bar of the run, after which nothing can fill.
    pub(crate) last: bool,
}

/// Account and session rules applied by the gate.
#[derive(Clone, Debug)]
pub(crate) struct GateRules {
    pub(crate) instrument_id: InstrumentId,
    pub(crate) overnight: Overnight,
    pub(crate) allow_short: bool,
    pub(crate) max_exposure_fraction: Decimal,
}

pub(crate) struct OrderGate {
    rules: GateRules,
    slots: Vec<BarSlot>,
    cache: Rc<RefCell<Cache>>,
    clock: Rc<RefCell<dyn Clock>>,
    ledger: Rc<RefCell<Ledger>>,
    denials: RefCell<Vec<(UnixNanos, ClientOrderId, String)>>,
}

impl OrderGate {
    pub(crate) fn new(
        rules: GateRules,
        slots: Vec<BarSlot>,
        cache: Rc<RefCell<Cache>>,
        clock: Rc<RefCell<dyn Clock>>,
        ledger: Rc<RefCell<Ledger>>,
    ) -> Self {
        Self {
            rules,
            slots,
            cache,
            clock,
            ledger,
            denials: RefCell::new(Vec::new()),
        }
    }

    /// Routes strategy commands through `gate`. Call after the engine is created, since
    /// creating the kernel registers the risk engine's own handlers.
    pub(crate) fn install(gate: &Rc<Self>) {
        let defer = TypedIntoHandler::from(|command: TradingCommand| {
            let endpoint = GATE_ENDPOINT.into();
            match try_get_trading_cmd_sender() {
                Some(sender) => sender.execute(TradingCommandMessage::new(endpoint, command)),
                None => msgbus::send_trading_command(endpoint, command),
            }
        });
        msgbus::register_trading_command_endpoint(
            MessagingSwitchboard::risk_engine_queue_execute(),
            defer.clone(),
        );
        msgbus::register_trading_command_endpoint(
            MessagingSwitchboard::order_emulator_execute(),
            defer,
        );

        let gate = Rc::clone(gate);
        msgbus::register_trading_command_endpoint(
            GATE_ENDPOINT.into(),
            TypedIntoHandler::from(move |command: TradingCommand| gate.execute(command)),
        );
    }

    /// Returns every denied order with its time and reason.
    pub(crate) fn denials(&self) -> Vec<(UnixNanos, ClientOrderId, String)> {
        self.denials.borrow().clone()
    }

    fn execute(&self, command: TradingCommand) {
        match &command {
            TradingCommand::SubmitOrder(submit) => {
                let orders = self.orders(&[submit.client_order_id]);
                if let Err(reason) = self.check_submit(&orders) {
                    self.deny(&orders, &reason);
                    return;
                }
            }
            TradingCommand::SubmitOrderList(submit) => {
                let orders = self.orders(&submit.order_list.client_order_ids);
                if let Err(reason) = self.check_submit(&orders) {
                    self.deny(&orders, &reason);
                    return;
                }
            }
            TradingCommand::ModifyOrder(modify) => {
                if let Err(reason) = self.check_modify(std::slice::from_ref(modify)) {
                    self.reject_modify(modify, &reason);
                    return;
                }
            }
            TradingCommand::ModifyOrders(batch) => {
                if let Err(reason) = self.check_modify(&batch.modifies) {
                    for modify in &batch.modifies {
                        self.reject_modify(modify, &reason);
                    }
                    return;
                }
            }
            _ => {}
        }
        msgbus::send_trading_command(MessagingSwitchboard::risk_engine_execute(), command);
    }

    fn orders(&self, client_order_ids: &[ClientOrderId]) -> Vec<OrderAny> {
        let cache = self.cache.borrow();
        client_order_ids
            .iter()
            .filter_map(|id| cache.order(id).map(|order| order.clone()))
            .collect()
    }

    fn now(&self) -> UnixNanos {
        self.clock.borrow().timestamp_ns()
    }

    // The bar close the current time belongs to, if orders may be sent now
    fn current_slot(&self) -> Result<BarSlot, String> {
        let now = self.now();
        let index = self.slots.partition_point(|slot| slot.close_ts <= now);
        let slot = index
            .checked_sub(1)
            .map(|i| self.slots[i])
            .ok_or_else(|| "no bar has closed yet".to_string())?;
        if now.as_u64() - slot.close_ts.as_u64() > 1 {
            return Err(format!(
                "orders are accepted only at a bar close; the last bar closed at {}",
                slot.close_ts
            ));
        }
        if slot.last {
            return Err("no later bar to fill at; the backtest ends at this bar".to_string());
        }
        if slot.session_end && self.rules.overnight == Overnight::Flatten {
            return Err(
                "session closing (overnight = flatten): no orders from the session's second-to-last bar close until the next session"
                    .to_string(),
            );
        }
        Ok(slot)
    }

    fn check_submit(&self, orders: &[OrderAny]) -> Result<(), String> {
        let slot = self.current_slot()?;
        let mut additions = Vec::new();
        for order in orders {
            if order.instrument_id() != self.rules.instrument_id {
                return Err(format!(
                    "instrument {} is not simulated; only {} is",
                    order.instrument_id(),
                    self.rules.instrument_id
                ));
            }
            if order.emulation_trigger().is_some() {
                return Err("order emulation is not supported".to_string());
            }
            if is_limited(order) {
                additions.push((order.order_side(), order.quantity().as_decimal()));
            }
        }
        self.check_exposure(slot, &additions)
    }

    fn check_modify(&self, modifies: &[ModifyOrder]) -> Result<(), String> {
        let slot = self.current_slot()?;
        let mut additions = Vec::new();
        for modify in modifies {
            let Some(quantity) = modify.quantity else {
                continue;
            };
            let Some(order) = self.orders(&[modify.client_order_id]).pop() else {
                continue;
            };
            if is_limited(&order) && quantity > order.quantity() {
                additions.push((
                    order.order_side(),
                    quantity.as_decimal() - order.quantity().as_decimal(),
                ));
            }
        }
        self.check_exposure(slot, &additions)
    }

    // Checks the worst case position if all working orders and `additions` fill
    fn check_exposure(
        &self,
        slot: BarSlot,
        additions: &[(OrderSide, Decimal)],
    ) -> Result<(), String> {
        if additions.is_empty() {
            return Ok(());
        }
        let ledger = self.ledger.borrow();
        let price = slot.close_price;
        let equity = ledger.equity(price);
        if equity <= Decimal::ZERO {
            return Err(format!("equity {equity} is not positive"));
        }
        let limit = self.rules.max_exposure_fraction * equity;
        let position = ledger.net_quantity();

        let (mut buys, mut sells) = self.working_quantities();
        for (side, quantity) in additions {
            match side {
                OrderSide::Buy => {
                    buys += quantity;
                    let worst = position + buys;
                    if worst > Decimal::ZERO && worst * price > limit {
                        return Err(format!(
                            "long position of up to {worst} shares (value {}) would exceed the limit of {} at equity {}",
                            (worst * price).round_dp(2),
                            limit.round_dp(2),
                            equity.round_dp(2)
                        ));
                    }
                }
                _ => {
                    sells += quantity;
                    let worst = position - sells;
                    if worst < Decimal::ZERO {
                        if !self.rules.allow_short {
                            return Err("short selling is not allowed".to_string());
                        }
                        if -worst * price > limit {
                            return Err(format!(
                                "short position of up to {} shares (value {}) would exceed the limit of {} at equity {}",
                                -worst,
                                (-worst * price).round_dp(2),
                                limit.round_dp(2),
                                equity.round_dp(2)
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // Unfilled quantity of working orders that can add to the position, by side
    fn working_quantities(&self) -> (Decimal, Decimal) {
        let cache = self.cache.borrow();
        let instrument_id = Some(&self.rules.instrument_id);
        let open = cache.orders_open(None, instrument_id, None, None, None);
        let inflight = cache.orders_inflight(None, instrument_id, None, None, None);
        let mut buys = Decimal::ZERO;
        let mut sells = Decimal::ZERO;
        for order in open.iter().chain(&inflight) {
            if !is_limited(order) {
                continue;
            }
            let leaves = order.leaves_qty().as_decimal();
            match order.order_side() {
                OrderSide::Buy => buys += leaves,
                _ => sells += leaves,
            }
        }
        (buys, sells)
    }

    fn deny(&self, orders: &[OrderAny], reason: &str) {
        let ts = self.now();
        for order in orders {
            log::warn!(
                "Order {} DENIED by bar simulation: {reason}",
                order.client_order_id()
            );
            self.denials
                .borrow_mut()
                .push((ts, order.client_order_id(), reason.to_string()));
            if order.status() != OrderStatus::Initialized {
                continue;
            }
            let denied = OrderEventAny::Denied(OrderDenied::new(
                order.trader_id(),
                order.strategy_id(),
                order.instrument_id(),
                order.client_order_id(),
                reason.into(),
                UUID4::new(),
                ts,
                ts,
            ));
            msgbus::send_order_event(MessagingSwitchboard::exec_engine_process(), denied);
        }
    }

    fn reject_modify(&self, modify: &ModifyOrder, reason: &str) {
        log::warn!(
            "Modify of {} REJECTED by bar simulation: {reason}",
            modify.client_order_id
        );
        let ts = self.now();
        let account_id = self
            .orders(&[modify.client_order_id])
            .pop()
            .and_then(|order| order.account_id());
        let rejected = OrderEventAny::ModifyRejected(OrderModifyRejected::new(
            modify.trader_id,
            modify.strategy_id,
            modify.instrument_id,
            modify.client_order_id,
            reason.into(),
            UUID4::new(),
            ts,
            ts,
            false,
            modify.venue_order_id,
            account_id,
        ));
        msgbus::send_order_event(MessagingSwitchboard::exec_engine_process(), rejected);
    }
}

// Whether an order can add to the position: not reduce-only and not a contingent child order
fn is_limited(order: &OrderAny) -> bool {
    !order.is_reduce_only() && order.parent_order_id().is_none()
}
