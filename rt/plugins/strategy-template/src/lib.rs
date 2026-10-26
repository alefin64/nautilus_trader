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

//! Strategy slot: the strategy compiled into `libstrategy_plugin.so`.
//!
//! Edit only `src/`. `Cargo.toml` is frozen: the crate depends on `nautilus-rt` alone, and
//! reaches engine and third-party crates through its re-exports (`nautilus_rt::model`,
//! `nautilus_rt::indicators`, `nautilus_rt::serde_json`, ...).
//!
//! The plug-in exports one strategy type, `"Strategy"`. The runner creates it from the
//! `[strategies.config]` table of its config, passed to [`from_json`](SmaCross::from_json) as
//! JSON. Keep the `strategy_plugin!` block and the type name when replacing the template.
//!
//! # How the backtest treats orders
//!
//! The runner simulates fills on bars (`nautilus_rt::sim`):
//!
//! - An order sent from `on_bar` fills at the **next bar's open**, plus slippage, never on the
//!   bar it was sent on. Resting limit and stop orders are only matched against bar opens.
//! - Orders are accepted only at a bar close. Orders sent from timers between bars are denied.
//! - Orders that would exceed the account's position limits, or sell short when shorting is
//!   not allowed, are denied (`on_order_denied`).
//! - With `overnight = flatten`, the runner closes positions on each session's last bar and
//!   denies orders on the last two bars of a session.
//!
//! # Template
//!
//! [`SmaCross`] holds `trade_size` shares long while the fast simple moving average of bar
//! closes is above the slow one, and short (or flat, if `go_short` is false) otherwise.

use std::fmt::Debug;

use nautilus_rt::{
    anyhow,
    common::actor::DataActor,
    indicators::{
        average::sma::SimpleMovingAverage,
        indicator::{Indicator, MovingAverage},
    },
    model::{
        data::{Bar, BarType},
        enums::{OrderSide, PriceType},
        identifiers::{InstrumentId, StrategyId},
        types::Quantity,
    },
    rust_decimal::Decimal,
    serde::Deserialize,
    serde_json,
    trading::{
        nautilus_strategy,
        strategy::{Strategy, StrategyConfig, StrategyCore},
    },
};

#[cfg(test)]
mod tests;

nautilus_rt::strategy_plugin! {
    name: "strategy",
    version: env!("CARGO_PKG_VERSION"),
    strategies: {
        "Strategy" => SmaCross::from_json,
    },
}

/// Configuration for [`SmaCross`].
#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "nautilus_rt::serde", deny_unknown_fields)]
pub struct SmaCrossConfig {
    /// Base strategy configuration (`strategy_id`, `order_id_tag`, ...).
    #[serde(default = "default_base")]
    pub base: StrategyConfig,
    /// Instrument to trade.
    pub instrument_id: InstrumentId,
    /// Bars to subscribe to.
    pub bar_type: BarType,
    /// Fast moving average period, in bars.
    pub fast_period: usize,
    /// Slow moving average period, in bars. Must exceed `fast_period`.
    pub slow_period: usize,
    /// Position size, in shares.
    pub trade_size: Quantity,
    /// Whether to go short when the fast average is below the slow one, rather than flat.
    pub go_short: bool,
}

fn default_base() -> StrategyConfig {
    StrategyConfig {
        strategy_id: Some(StrategyId::from("STRATEGY-001")),
        order_id_tag: Some("001".to_string()),
        ..Default::default()
    }
}

/// Moving-average crossover on bar closes.
pub struct SmaCross {
    core: StrategyCore,
    instrument_id: InstrumentId,
    bar_type: BarType,
    trade_size: Quantity,
    go_short: bool,
    fast: SimpleMovingAverage,
    slow: SimpleMovingAverage,
}

impl SmaCross {
    /// Creates the strategy from its config.
    ///
    /// # Errors
    ///
    /// Returns an error if a period is zero, `fast_period` is not below `slow_period`, or
    /// `trade_size` is zero.
    pub fn from_config(config: SmaCrossConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(config.fast_period > 0, "fast_period must be positive");
        anyhow::ensure!(
            config.fast_period < config.slow_period,
            "fast_period must be less than slow_period"
        );
        anyhow::ensure!(
            config.trade_size.is_positive(),
            "trade_size must be positive"
        );
        Ok(Self {
            core: StrategyCore::new(config.base),
            instrument_id: config.instrument_id,
            bar_type: config.bar_type,
            trade_size: config.trade_size,
            go_short: config.go_short,
            fast: SimpleMovingAverage::new(config.fast_period, Some(PriceType::Last)),
            slow: SimpleMovingAverage::new(config.slow_period, Some(PriceType::Last)),
        })
    }

    /// Creates the strategy from its JSON config.
    ///
    /// # Errors
    ///
    /// Returns an error if `config` does not deserialize into [`SmaCrossConfig`] or is invalid.
    pub fn from_json(config: serde_json::Value) -> anyhow::Result<Self> {
        Self::from_config(serde_json::from_value(config)?)
    }

    // Position the strategy wants, in shares (negative for short)
    fn target_position(&self) -> Decimal {
        let size = self.trade_size.as_decimal();
        if self.fast.value() > self.slow.value() {
            size
        } else if self.go_short {
            -size
        } else {
            Decimal::ZERO
        }
    }

    fn has_working_orders(&self) -> bool {
        let cache = self.cache();
        let strategy_id = self.strategy_id();
        let strategy_id = strategy_id.as_ref();
        let instrument_id = Some(&self.instrument_id);
        !cache
            .client_order_ids_open(None, instrument_id, strategy_id, None)
            .is_empty()
            || !cache
                .client_order_ids_inflight(None, instrument_id, strategy_id, None)
                .is_empty()
    }
}

nautilus_strategy!(SmaCross);

impl Debug for SmaCross {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(SmaCross))
            .field("instrument_id", &self.instrument_id)
            .field("bar_type", &self.bar_type)
            .field("trade_size", &self.trade_size)
            .field("fast_period", &self.fast.period)
            .field("slow_period", &self.slow.period)
            .field("go_short", &self.go_short)
            .finish()
    }
}

impl DataActor for SmaCross {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.unsubscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        self.fast.handle_bar(bar);
        self.slow.handle_bar(bar);
        if !self.slow.initialized() || self.has_working_orders() {
            return Ok(());
        }

        let position = self.portfolio().net_position(&self.instrument_id);
        let delta = self.target_position() - position;
        if delta.is_zero() {
            return Ok(());
        }

        let side = if delta > Decimal::ZERO {
            OrderSide::Buy
        } else {
            OrderSide::Sell
        };
        let order = self.order().market(
            self.instrument_id,
            side,
            Quantity::from_decimal_dp(delta.abs(), self.trade_size.precision)?,
            None, // time_in_force
            None, // reduce_only
            None, // quote_quantity
            None, // exec_algorithm_id
            None, // exec_algorithm_params
            None, // tags
            None, // client_order_id
        );
        self.submit_order(order, None, None, None)
    }
}
