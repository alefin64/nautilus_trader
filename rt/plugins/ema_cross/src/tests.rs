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

use nautilus_rt::{
    backtest::{config::BacktestEngineConfig, engine::BacktestEngine},
    model::{enums::OmsType, identifiers::StrategyId},
    sdk::StrategyRegistrar,
    serde_json::{Value, json},
    synthetic,
};
use rstest::rstest;

use super::*;

fn config_json() -> Value {
    json!({
        "instrument_id": "AUD/USD.SIM",
        "trade_size": "100000",
        "fast_period": 10,
        "slow_period": 20,
    })
}

#[rstest]
fn from_json_parses_config() {
    let strategy = EmaCross::from_json(config_json()).unwrap();

    assert_eq!(strategy.instrument_id, InstrumentId::from("AUD/USD.SIM"));
    assert_eq!(strategy.trade_size, Quantity::from("100000"));
    assert_eq!(strategy.ema_fast.period, 10);
    assert_eq!(strategy.ema_slow.period, 20);
}

#[rstest]
fn from_json_defaults_base_config() {
    let strategy = EmaCross::from_json(config_json()).unwrap();

    assert_eq!(
        strategy.core.config().strategy_id,
        Some(StrategyId::from("EMA_CROSS-001"))
    );
}

#[rstest]
fn from_json_accepts_base_config() {
    let mut config = config_json();
    config["base"] = json!({ "strategy_id": "EMA_CROSS-002", "order_id_tag": "002" });

    let strategy = EmaCross::from_json(config).unwrap();

    assert_eq!(
        strategy.core.config().strategy_id,
        Some(StrategyId::from("EMA_CROSS-002"))
    );
}

#[rstest]
#[case::unknown_field("fast", json!(10))]
#[case::wrong_type("fast_period", json!("ten"))]
fn from_json_rejects_invalid_config(#[case] key: &str, #[case] value: Value) {
    let mut config = config_json();
    config[key] = value;

    assert!(EmaCross::from_json(config).is_err());
}

#[rstest]
fn from_json_rejects_missing_instrument() {
    let mut config = config_json();
    config.as_object_mut().unwrap().remove("instrument_id");

    assert!(EmaCross::from_json(config).is_err());
}

// Same scenario and expected totals as the `engine-ema-cross` example in `nautilus-backtest`
#[rstest]
fn backtest_on_synthetic_quotes() {
    let mut engine = BacktestEngine::new(BacktestEngineConfig::default()).unwrap();
    let instrument_id = synthetic::add_venue_and_instrument(&mut engine, OmsType::Hedging).unwrap();
    StrategyRegistrar::backtest(&mut engine)
        .add_strategy(EmaCross::from_json(config_json()).unwrap())
        .unwrap();
    engine
        .add_data(
            synthetic::generate_quotes(instrument_id, 6),
            None,
            true,
            true,
        )
        .unwrap();

    engine.run(None, None, None, false).unwrap();

    let result = engine.get_result();
    assert_eq!(result.iterations, 745);
    assert_eq!(result.total_events, 24);
    assert_eq!(result.total_orders, 12);
    assert_eq!(result.total_positions, 12);
}
