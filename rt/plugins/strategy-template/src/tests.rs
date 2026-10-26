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

//! Strategy tests that run in-process, without loading the plug-in library.
//!
//! The backtest uses the runner's bar simulation (`nautilus_rt::sim`) on the synthetic equity
//! scenario, so fills, costs, and rules match a real run.

use nautilus_rt::{
    model::{enums::OrderSide, identifiers::StrategyId},
    sdk::StrategyRegistrar,
    serde_json::{Value, json},
    sim::{BarSimulation, SimConfig, SimOutcome},
    synthetic::{EQUITY_BAR_TYPE, EQUITY_INSTRUMENT_ID, EquityScenario},
};
use rstest::rstest;

use super::*;

fn config_json() -> Value {
    json!({
        "instrument_id": EQUITY_INSTRUMENT_ID,
        "bar_type": EQUITY_BAR_TYPE,
        "fast_period": 10,
        "slow_period": 30,
        "trade_size": "500",
        "go_short": true,
    })
}

fn sim_config() -> SimConfig {
    serde_json::from_value(json!({
        "venue": {
            "oms_type": "NETTING",
            "account_type": "MARGIN",
            "base_currency": "USD",
            "starting_capital": "100000",
            "max_leverage": "1",
            "max_position_pct_equity": "100",
            "allow_short": true,
            "short_borrow_bps_per_year": "50",
        },
        "fill_model": {
            "type": "next_bar_open",
            "slippage_bps": { "pre_market": "10", "regular": "2", "after_hours": "10" },
            "commission": { "per_share": "0.005", "min_per_order": "1" },
        },
        "overnight": "hold",
        "seed": 0,
    }))
    .unwrap()
}

fn backtest(config: Value, sessions: usize) -> (EquityScenario, SimOutcome) {
    let scenario = EquityScenario::new(sessions).unwrap();
    let mut sim = BarSimulation::new(
        &sim_config(),
        scenario.instrument.clone(),
        scenario.bars.clone(),
        &scenario.schedule,
    )
    .unwrap();
    StrategyRegistrar::backtest(sim.engine_mut())
        .add_strategy(SmaCross::from_json(config).unwrap())
        .unwrap();
    let outcome = sim.run().unwrap();
    (scenario, outcome)
}

#[rstest]
fn from_json_parses_config() {
    let strategy = SmaCross::from_json(config_json()).unwrap();

    assert_eq!(
        strategy.instrument_id,
        InstrumentId::from(EQUITY_INSTRUMENT_ID)
    );
    assert_eq!(strategy.bar_type, BarType::from(EQUITY_BAR_TYPE));
    assert_eq!(strategy.trade_size, Quantity::from(500));
    assert_eq!(strategy.fast.period, 10);
    assert_eq!(strategy.slow.period, 30);
    assert!(strategy.go_short);
}

#[rstest]
fn from_json_defaults_base_config() {
    let strategy = SmaCross::from_json(config_json()).unwrap();

    assert_eq!(
        strategy.core.config().strategy_id,
        Some(StrategyId::from("STRATEGY-001"))
    );
}

#[rstest]
fn from_json_accepts_base_config() {
    let mut config = config_json();
    config["base"] = json!({ "strategy_id": "STRATEGY-002", "order_id_tag": "002" });

    let strategy = SmaCross::from_json(config).unwrap();

    assert_eq!(
        strategy.core.config().strategy_id,
        Some(StrategyId::from("STRATEGY-002"))
    );
}

#[rstest]
#[case::unknown_field("fast", json!(10))]
#[case::wrong_type("fast_period", json!("ten"))]
#[case::fast_not_below_slow("fast_period", json!(30))]
#[case::zero_period("fast_period", json!(0))]
#[case::zero_size("trade_size", json!("0"))]
fn from_json_rejects_invalid_config(#[case] key: &str, #[case] value: Value) {
    let mut config = config_json();
    config[key] = value;

    assert!(SmaCross::from_json(config).is_err());
}

#[rstest]
fn from_json_rejects_missing_go_short() {
    let mut config = config_json();
    config.as_object_mut().unwrap().remove("go_short");

    assert!(SmaCross::from_json(config).is_err());
}

#[rstest]
fn backtest_on_synthetic_bars() {
    let (scenario, outcome) = backtest(config_json(), 5);

    let metrics = &outcome.metrics;
    assert_eq!(metrics.bars, scenario.bars.len());
    assert_eq!(metrics.sessions, 5);
    // One bar per bar close, plus one fill quote per following bar open
    assert_eq!(outcome.totals.iterations, 2 * scenario.bars.len() - 1);
    assert_eq!(outcome.totals.total_orders, 8);
    assert_eq!(metrics.fill_count, 8);
    assert_eq!(metrics.trade_count, 7);
    assert_eq!(metrics.denied_orders, 0);
    // Reversals take the position from long to short in one order, never beyond the size
    for daily in &outcome.daily {
        assert!(daily.position.abs() <= Decimal::from(500));
    }
}

#[rstest]
fn backtest_fills_at_next_bar_open() {
    let (scenario, outcome) = backtest(config_json(), 5);

    for record in &outcome.fills {
        // Each fill follows a bar close by one nanosecond, at the following bar's open
        let index = scenario
            .bars
            .iter()
            .position(|bar| bar.ts_init.as_u64() + 1 == record.fill.ts_event.as_u64())
            .unwrap();
        assert_eq!(
            record.open_price,
            scenario.bars[index + 1].open.as_decimal()
        );
        match record.fill.side {
            OrderSide::Buy => assert!(record.fill.price > record.open_price),
            _ => assert!(record.fill.price < record.open_price),
        }
    }
}

#[rstest]
fn backtest_long_only_stays_flat_or_long() {
    let mut config = config_json();
    config["go_short"] = json!(false);

    let (_, outcome) = backtest(config, 5);

    assert!(outcome.metrics.trade_count > 0);
    for daily in &outcome.daily {
        assert!(daily.position >= Decimal::ZERO);
    }
}
