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

//! Bar simulation tests with a scripted strategy on the synthetic equity scenario.

use std::{cell::RefCell, collections::BTreeMap, fmt::Debug, rc::Rc};

use nautilus_common::actor::DataActor;
use nautilus_model::{
    data::Bar,
    enums::OrderSide,
    events::OrderDenied,
    identifiers::{InstrumentId, StrategyId},
    types::Quantity,
};
use nautilus_trading::{
    nautilus_strategy,
    strategy::{Strategy, StrategyConfig, StrategyCore},
};
use rstest::rstest;
use rust_decimal::{Decimal, RoundingStrategy};
use rust_decimal_macros::dec;

use super::{config::tests::sim_config, *};
use crate::{
    sdk::StrategyRegistrar,
    synthetic::{BARS_PER_SESSION, EquityScenario},
};

// Submits market orders at given bar indices and records denials
struct Scripted {
    core: StrategyCore,
    instrument_id: InstrumentId,
    bar_type: BarType,
    orders: BTreeMap<usize, Vec<(OrderSide, u64)>>,
    bars_seen: usize,
    denied: Rc<RefCell<Vec<(usize, String)>>>,
}

impl Scripted {
    fn new(
        scenario: &EquityScenario,
        orders: &[(usize, OrderSide, u64)],
        denied: Rc<RefCell<Vec<(usize, String)>>>,
    ) -> Self {
        let mut by_bar: BTreeMap<usize, Vec<(OrderSide, u64)>> = BTreeMap::new();
        for (bar, side, quantity) in orders {
            by_bar.entry(*bar).or_default().push((*side, *quantity));
        }
        Self {
            core: StrategyCore::new(StrategyConfig {
                strategy_id: Some(StrategyId::from("SCRIPTED-001")),
                order_id_tag: Some("001".to_string()),
                ..Default::default()
            }),
            instrument_id: scenario.instrument.id(),
            bar_type: scenario.bar_type,
            orders: by_bar,
            bars_seen: 0,
            denied,
        }
    }
}

nautilus_strategy!(Scripted, {
    fn on_order_denied(&mut self, event: OrderDenied) {
        self.denied
            .borrow_mut()
            .push((self.bars_seen - 1, event.reason.to_string()));
    }
});

impl Debug for Scripted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scripted").finish()
    }
}

impl DataActor for Scripted {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_bar(&mut self, _bar: &Bar) -> anyhow::Result<()> {
        let index = self.bars_seen;
        self.bars_seen += 1;
        for (side, quantity) in self.orders.get(&index).cloned().unwrap_or_default() {
            let order = self.order().market(
                self.instrument_id,
                side,
                Quantity::from(quantity),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            );
            self.submit_order(order, None, None, None)?;
        }
        Ok(())
    }
}

struct Run {
    scenario: EquityScenario,
    outcome: SimOutcome,
    denied: Vec<(usize, String)>,
}

fn run(config: &SimConfig, sessions: usize, orders: &[(usize, OrderSide, u64)]) -> Run {
    let scenario = EquityScenario::new(sessions).unwrap();
    let denied = Rc::new(RefCell::new(Vec::new()));
    let mut sim = BarSimulation::new(
        config,
        scenario.instrument.clone(),
        scenario.bars.clone(),
        &scenario.schedule,
    )
    .unwrap();
    StrategyRegistrar::backtest(sim.engine_mut())
        .add_strategy(Scripted::new(&scenario, orders, Rc::clone(&denied)))
        .unwrap();
    let outcome = sim.run().unwrap();
    let denied = denied.borrow().clone();
    Run {
        scenario,
        outcome,
        denied,
    }
}

fn open(scenario: &EquityScenario, index: usize) -> Decimal {
    scenario.bars[index].open.as_decimal()
}

fn with_slippage(open: Decimal, bps: Decimal, side: OrderSide) -> Decimal {
    let fraction = bps / dec!(10000);
    match side {
        OrderSide::Buy => (open * (Decimal::ONE + fraction))
            .round_dp_with_strategy(4, RoundingStrategy::AwayFromZero),
        _ => (open * (Decimal::ONE - fraction)).round_dp_with_strategy(4, RoundingStrategy::ToZero),
    }
}

#[rstest]
#[case::buy(OrderSide::Buy)]
#[case::sell(OrderSide::Sell)]
fn market_order_fills_at_next_bar_open_with_slippage(#[case] side: OrderSide) {
    let run = run(&sim_config(), 1, &[(10, side, 100)]);

    let fills = &run.outcome.fills;
    assert_eq!(fills.len(), 1);
    let record = &fills[0];
    assert_eq!(record.fill.side, side);
    assert_eq!(record.fill.quantity, dec!(100));
    assert_eq!(record.open_price, open(&run.scenario, 11));
    assert_eq!(
        record.fill.price,
        with_slippage(open(&run.scenario, 11), dec!(2), side)
    );
    assert_eq!(
        record.fill.ts_event,
        run.scenario.bars[10].ts_init + DurationNanos::new(1)
    );
    assert_eq!(record.fill.commission, dec!(1.00));
    assert!(record.slippage > Decimal::ZERO);
    assert!(run.denied.is_empty());
}

#[rstest]
fn order_on_session_last_bar_fills_at_next_session_open() {
    let last = BARS_PER_SESSION - 1;

    let run = run(&sim_config(), 2, &[(last, OrderSide::Buy, 100)]);

    let record = &run.outcome.fills[0];
    assert_eq!(record.open_price, open(&run.scenario, last + 1));
    assert_ne!(
        record.open_price,
        run.scenario.bars[last].close.as_decimal(),
        "scenario has an overnight gap"
    );
    assert_eq!(
        record.fill.ts_event,
        run.scenario.bars[last].ts_init + DurationNanos::new(1)
    );
}

#[rstest]
fn commission_applies_per_share_above_minimum() {
    let run = run(&sim_config(), 1, &[(10, OrderSide::Buy, 1001)]);

    // 1001 * 0.005 = 5.005, rounded half away from zero
    assert_eq!(run.outcome.fills[0].fill.commission, dec!(5.01));
}

#[rstest]
fn short_sale_denied_when_shorting_is_not_allowed() {
    let mut config = sim_config();
    config.venue.allow_short = false;

    let run = run(&config, 1, &[(10, OrderSide::Sell, 100)]);

    assert!(run.outcome.fills.is_empty());
    assert_eq!(
        run.denied,
        vec![(10, "short selling is not allowed".to_string())]
    );
    assert_eq!(run.outcome.metrics.denied_orders, 1);
}

#[rstest]
fn selling_a_long_position_is_allowed_without_shorting() {
    let mut config = sim_config();
    config.venue.allow_short = false;

    let run = run(
        &config,
        1,
        &[(10, OrderSide::Buy, 100), (20, OrderSide::Sell, 100)],
    );

    assert_eq!(run.outcome.fills.len(), 2);
    assert!(run.denied.is_empty());
}

#[rstest]
fn exposure_limit_denies_oversized_orders() {
    // About $150k of a $50 stock against $100k equity at 1x leverage
    let run = run(
        &sim_config(),
        1,
        &[(10, OrderSide::Buy, 3000), (11, OrderSide::Buy, 1000)],
    );

    assert_eq!(run.outcome.fills.len(), 1);
    assert_eq!(run.outcome.fills[0].fill.quantity, dec!(1000));
    assert_eq!(run.denied.len(), 1);
    assert_eq!(run.denied[0].0, 10);
    assert!(
        run.denied[0].1.contains("would exceed the limit"),
        "{:?}",
        run.denied
    );
}

#[rstest]
fn max_position_pct_limits_exposure() {
    let mut config = sim_config();
    config.venue.max_leverage = dec!(4);
    config.venue.max_position_pct_equity = dec!(10);

    // About $25k against a $10k limit
    let run = run(&config, 1, &[(10, OrderSide::Buy, 500)]);

    assert!(run.outcome.fills.is_empty());
    assert!(run.denied[0].1.contains("would exceed the limit"));
}

#[rstest]
fn order_on_final_bar_is_denied() {
    let last = BARS_PER_SESSION - 1;

    let run = run(&sim_config(), 1, &[(last, OrderSide::Buy, 100)]);

    assert!(run.outcome.fills.is_empty());
    assert!(run.denied[0].1.contains("no later bar"), "{:?}", run.denied);
}

#[rstest]
fn flatten_exits_on_session_last_bar_and_blocks_orders() {
    let mut config = sim_config();
    config.overnight = Overnight::Flatten;
    let last = BARS_PER_SESSION - 1;

    let run = run(
        &config,
        2,
        &[
            (10, OrderSide::Buy, 100),
            (last - 1, OrderSide::Buy, 100),
            (last, OrderSide::Buy, 100),
            (last + 1, OrderSide::Buy, 100),
        ],
    );

    let fills = &run.outcome.fills;
    assert_eq!(fills.len(), 4, "{fills:?}");
    // Flatten order fills at the open of the session's last bar
    assert!(fills[1].fill.flatten);
    assert_eq!(fills[1].fill.side, OrderSide::Sell);
    assert_eq!(fills[1].fill.quantity, dec!(100));
    assert_eq!(fills[1].open_price, open(&run.scenario, last));
    // Strategy orders on the last two bars are denied; the next session trades again
    assert_eq!(
        run.denied.iter().map(|d| d.0).collect::<Vec<_>>(),
        vec![last - 1, last]
    );
    assert!(run.denied[0].1.contains("overnight = flatten"));
    assert!(!fills[2].fill.flatten);
    assert_eq!(fills[2].open_price, open(&run.scenario, last + 2));
    // The second session is flattened too
    assert!(fills[3].fill.flatten);
    assert_eq!(
        fills[3].open_price,
        open(&run.scenario, 2 * BARS_PER_SESSION - 1)
    );
    assert!(run.outcome.daily.iter().all(|d| d.position.is_zero()));
    assert!(run.outcome.borrow.is_empty());
}

#[rstest]
fn flatten_cancels_working_orders_and_closes_shorts() {
    let mut config = sim_config();
    config.overnight = Overnight::Flatten;

    let run = run(&config, 1, &[(10, OrderSide::Sell, 200)]);

    let fills = &run.outcome.fills;
    assert_eq!(fills.len(), 2);
    assert_eq!(fills[1].fill.side, OrderSide::Buy);
    assert!(fills[1].fill.flatten);
    assert_eq!(run.outcome.metrics.final_position, Decimal::ZERO);
    assert_eq!(run.outcome.metrics.trade_count, 1);
}

#[rstest]
fn borrow_is_charged_on_overnight_shorts() {
    // Short from the first Friday through the weekend: scenario day 5 is Friday 2025-01-10
    let friday_last = 5 * BARS_PER_SESSION - 1;

    let run = run(&sim_config(), 6, &[(friday_last - 5, OrderSide::Sell, 100)]);

    let charges = &run.outcome.borrow;
    assert_eq!(charges.len(), 2, "{charges:?}");
    let friday = &charges[0];
    assert_eq!(friday.date.to_string(), "2025-01-10");
    assert_eq!(friday.days, 3);
    let close = run.scenario.bars[friday_last].close.as_decimal();
    let expected = (dec!(100) * close * dec!(50) / dec!(10000) * dec!(3) / dec!(365))
        .round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
    assert_eq!(friday.amount, expected);
    assert_eq!(charges[1].days, 1);
    assert_eq!(
        run.outcome.metrics.borrow,
        charges[0].amount + charges[1].amount
    );
}

#[rstest]
fn ledger_matches_engine_account() {
    let run = run(
        &sim_config(),
        2,
        &[
            (10, OrderSide::Sell, 300),
            (100, OrderSide::Buy, 500),
            (120, OrderSide::Sell, 200),
        ],
    );

    let metrics = &run.outcome.metrics;
    assert_eq!(metrics.final_position, Decimal::ZERO);
    assert_eq!(run.outcome.account_balance, metrics.ending_equity);
    assert_eq!(metrics.trade_count, 2);
    assert_eq!(metrics.fill_count, 3);
}

#[rstest]
fn metrics_add_up() {
    let run = run(
        &sim_config(),
        3,
        &[
            (5, OrderSide::Buy, 1000),
            (90, OrderSide::Sell, 1500),
            (200, OrderSide::Buy, 500),
        ],
    );

    let outcome = &run.outcome;
    let metrics = &outcome.metrics;
    assert_eq!(outcome.daily.len(), 3);
    assert_eq!(metrics.sessions, 3);
    assert_eq!(metrics.bars, 3 * BARS_PER_SESSION);
    let daily_pnl: Decimal = outcome.daily.iter().map(|d| d.pnl).sum();
    assert_eq!(daily_pnl, metrics.net_pnl);
    assert_eq!(
        metrics.total_costs,
        metrics.commissions + metrics.slippage + metrics.borrow
    );
    assert_eq!(metrics.gross_pnl, metrics.net_pnl + metrics.total_costs);
    assert_eq!(outcome.daily.last().unwrap().equity, metrics.ending_equity);
    assert!(metrics.exposure_pct > 0.0 && metrics.exposure_pct < 100.0);
    assert!(metrics.max_drawdown_pct >= 0.0);
    assert!(metrics.sharpe.is_some());
    // Flat at the end, so realized profit less borrow is the whole result
    assert_eq!(metrics.final_position, Decimal::ZERO);
    let trips: Decimal = outcome.round_trips.iter().map(|t| t.pnl_net).sum();
    assert_eq!(trips - metrics.borrow, metrics.net_pnl);
}

#[rstest]
fn rejects_bars_outside_sessions() {
    let scenario = EquityScenario::new(1).unwrap();
    let mut bars = scenario.bars.clone();
    let mut late = *bars.last().unwrap();
    late.ts_event += DurationNanos::new(6 * 3_600_000_000_000);
    late.ts_init = late.ts_event;
    bars.push(late);

    let err = BarSimulation::new(
        &sim_config(),
        scenario.instrument.clone(),
        bars,
        &scenario.schedule,
    )
    .unwrap_err();

    assert!(
        err.to_string().contains("outside every session window"),
        "{err}"
    );
}

#[rstest]
fn rejects_currency_mismatch() {
    let scenario = EquityScenario::new(1).unwrap();
    let mut config = sim_config();
    config.venue.base_currency = nautilus_model::types::Currency::EUR();

    let err = BarSimulation::new(
        &config,
        scenario.instrument.clone(),
        scenario.bars.clone(),
        &scenario.schedule,
    )
    .unwrap_err();

    assert!(err.to_string().contains("not the quote currency"), "{err}");
}
