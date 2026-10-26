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

//! Closes positions at the end of every session when `overnight = flatten`.

use std::{cell::RefCell, fmt::Debug, rc::Rc};

use nautilus_common::{
    actor::{DataActor, DataActorConfig, DataActorCore, DataActorNative},
    messages::execution::{CancelOrder, SubmitOrder, TradingCommand},
    msgbus::{self, MessagingSwitchboard, switchboard::get_event_order_topic},
    nautilus_actor,
    runner::{TradingCommandMessage, try_get_trading_cmd_sender},
    timer::TimeEvent,
};
use nautilus_core::{UUID4, UnixNanos};
use nautilus_model::{
    enums::{OrderSide, TimeInForce},
    events::OrderEventAny,
    identifiers::{ActorId, ClientOrderId, InstrumentId, StrategyId},
    instruments::Instrument,
    orders::{MarketOrder, Order, OrderAny},
    types::Quantity,
};
use rust_decimal::Decimal;
use ustr::Ustr;

use super::ledger::{FLATTEN_ORDER_PREFIX, Ledger};

/// Actor that flattens every strategy's position at each session's second-to-last bar close.
///
/// At each trigger time it cancels all working orders on the instrument and submits a
/// reduce-only market order per open position, on behalf of the owning strategy. The orders
/// fill at the open of the session's last bar. The [`gate`](super::gate) blocks strategy orders
/// from that close until the next session.
///
/// Its time alerts fire after the strategies have handled the bar at the same timestamp.
pub(crate) struct SessionFlattener {
    core: DataActorCore,
    instrument_id: InstrumentId,
    triggers: Vec<UnixNanos>,
    next: usize,
    ledger: Rc<RefCell<Ledger>>,
    orders: u64,
}

impl SessionFlattener {
    pub(crate) fn new(
        instrument_id: InstrumentId,
        triggers: Vec<UnixNanos>,
        ledger: Rc<RefCell<Ledger>>,
    ) -> Self {
        let config = DataActorConfig {
            actor_id: Some(ActorId::from("SESSION-FLATTENER")),
            ..Default::default()
        };
        Self {
            core: DataActorCore::new(config),
            instrument_id,
            triggers,
            next: 0,
            ledger,
            orders: 0,
        }
    }

    fn schedule_next(&mut self) -> anyhow::Result<()> {
        let now = self.clock().timestamp_ns();
        while self.triggers.get(self.next).is_some_and(|ts| *ts < now) {
            self.next += 1;
        }
        if let Some(&ts) = self.triggers.get(self.next) {
            let name = format!("flatten-{}", self.next);
            self.clock()
                .set_time_alert_ns(&name, ts, None, Some(false))?;
        }
        Ok(())
    }

    fn flatten(&mut self) -> anyhow::Result<()> {
        let now = self.clock().timestamp_ns();
        let trader_id = self
            .trader_id()
            .ok_or_else(|| anyhow::anyhow!("session flattener is not registered"))?;
        let instrument_id = self.instrument_id;
        let cache = self.cache_rc();

        let (working, size_precision) = {
            let cache = cache.borrow();
            let working: Vec<_> = cache
                .orders_open(None, Some(&instrument_id), None, None, None)
                .iter()
                .map(|order| {
                    (
                        order.strategy_id(),
                        order.client_order_id(),
                        order.venue_order_id(),
                    )
                })
                .collect();
            let size_precision = cache
                .instrument(&instrument_id)
                .map(Instrument::size_precision)
                .ok_or_else(|| anyhow::anyhow!("instrument {instrument_id} not in cache"))?;
            (working, size_precision)
        };

        for (strategy_id, client_order_id, venue_order_id) in working {
            send_to_risk_engine(TradingCommand::CancelOrder(CancelOrder::new(
                trader_id,
                None,
                strategy_id,
                instrument_id,
                client_order_id,
                venue_order_id,
                UUID4::new(),
                now,
                None,
                None,
            )));
        }

        let positions: Vec<(StrategyId, Decimal)> = self
            .ledger
            .borrow()
            .positions()
            .filter(|(_, quantity)| !quantity.is_zero())
            .collect();
        for (strategy_id, quantity) in positions {
            self.orders += 1;
            let client_order_id = ClientOrderId::new(format!(
                "{FLATTEN_ORDER_PREFIX}{strategy_id}-{}",
                self.orders
            ));
            let side = if quantity > Decimal::ZERO {
                OrderSide::Sell
            } else {
                OrderSide::Buy
            };
            let order = OrderAny::Market(MarketOrder::new_checked(
                trader_id,
                strategy_id,
                instrument_id,
                client_order_id,
                side,
                Quantity::from_decimal_dp(quantity.abs(), size_precision)?,
                TimeInForce::Gtc,
                UUID4::new(),
                now,
                true, // reduce_only
                false,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(vec![Ustr::from("FLATTEN")]),
            )?);
            let init = order.init_event().clone();
            cache.borrow_mut().add_order(order, None, None, false)?;
            msgbus::publish_order_event(
                get_event_order_topic(strategy_id),
                &OrderEventAny::Initialized(init.clone()),
            );
            send_to_risk_engine(TradingCommand::SubmitOrder(SubmitOrder::new(
                trader_id,
                None,
                strategy_id,
                instrument_id,
                client_order_id,
                init,
                None,
                None,
                None,
                UUID4::new(),
                now,
                None,
            )));
        }
        Ok(())
    }
}

// Bypasses the gate: flatten orders are the simulation's own
fn send_to_risk_engine(command: TradingCommand) {
    let endpoint = MessagingSwitchboard::risk_engine_execute();
    match try_get_trading_cmd_sender() {
        Some(sender) => sender.execute(TradingCommandMessage::new(endpoint, command)),
        None => msgbus::send_trading_command(endpoint, command),
    }
}

nautilus_actor!(SessionFlattener);

impl Debug for SessionFlattener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(SessionFlattener))
            .field("instrument_id", &self.instrument_id)
            .field("next", &self.next)
            .finish()
    }
}

impl DataActor for SessionFlattener {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.schedule_next()
    }

    fn on_time_event(&mut self, _event: &TimeEvent) -> anyhow::Result<()> {
        self.flatten()?;
        self.next += 1;
        self.schedule_next()
    }
}
