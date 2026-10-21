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

//! Dual-EMA crossover strategy built as a strategy plug-in.
//!
//! Mirrors `nautilus_trading::examples::strategies::EmaCross`, but is compiled outside the
//! runtime library and loaded at startup.

use std::fmt::Debug;

use nautilus_rt::{
    common::actor::DataActor,
    indicators::{
        average::ema::ExponentialMovingAverage,
        indicator::{Indicator, MovingAverage},
    },
    model::{
        data::QuoteTick,
        enums::{OrderSide, PriceType},
        identifiers::{InstrumentId, StrategyId},
        types::Quantity,
    },
    serde::Deserialize,
    trading::{
        nautilus_strategy,
        strategy::{Strategy, StrategyConfig, StrategyCore},
    },
};

nautilus_rt::strategy_plugin! {
    name: "ema-cross",
    version: env!("CARGO_PKG_VERSION"),
    strategies: {
        "EmaCross" => EmaCross::from_json,
    },
}

/// Configuration for [`EmaCross`].
#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "nautilus_rt::serde", deny_unknown_fields)]
pub struct EmaCrossConfig {
    /// Base strategy configuration.
    #[serde(default = "default_base")]
    pub base: StrategyConfig,
    /// Instrument to subscribe to and trade.
    pub instrument_id: InstrumentId,
    /// Order quantity for each crossover signal.
    pub trade_size: Quantity,
    /// Fast EMA period.
    pub fast_period: usize,
    /// Slow EMA period.
    pub slow_period: usize,
}

fn default_base() -> StrategyConfig {
    StrategyConfig {
        strategy_id: Some(StrategyId::from("EMA_CROSS-001")),
        order_id_tag: Some("001".to_string()),
        ..Default::default()
    }
}

/// Dual-EMA crossover strategy.
pub struct EmaCross {
    core: StrategyCore,
    instrument_id: InstrumentId,
    trade_size: Quantity,
    ema_fast: ExponentialMovingAverage,
    ema_slow: ExponentialMovingAverage,
    prev_fast_above: Option<bool>,
}

impl EmaCross {
    /// Creates a new [`EmaCross`] instance from config.
    #[must_use]
    pub fn from_config(config: EmaCrossConfig) -> Self {
        Self {
            core: StrategyCore::new(config.base),
            instrument_id: config.instrument_id,
            trade_size: config.trade_size,
            ema_fast: ExponentialMovingAverage::new(config.fast_period, Some(PriceType::Mid)),
            ema_slow: ExponentialMovingAverage::new(config.slow_period, Some(PriceType::Mid)),
            prev_fast_above: None,
        }
    }

    /// Creates a new [`EmaCross`] instance from JSON config.
    ///
    /// # Errors
    ///
    /// Returns an error if `config` does not deserialize into [`EmaCrossConfig`].
    pub fn from_json(config: nautilus_rt::serde_json::Value) -> nautilus_rt::anyhow::Result<Self> {
        let config: EmaCrossConfig = nautilus_rt::serde_json::from_value(config)?;
        Ok(Self::from_config(config))
    }

    fn enter(&mut self, side: OrderSide) -> nautilus_rt::anyhow::Result<()> {
        let order = self.order().market(
            self.instrument_id,
            side,
            self.trade_size,
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

nautilus_strategy!(EmaCross);

impl Debug for EmaCross {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(EmaCross))
            .field("instrument_id", &self.instrument_id)
            .field("trade_size", &self.trade_size)
            .field("fast_period", &self.ema_fast.period)
            .field("slow_period", &self.ema_slow.period)
            .finish()
    }
}

impl DataActor for EmaCross {
    fn on_start(&mut self) -> nautilus_rt::anyhow::Result<()> {
        self.subscribe_quotes(self.instrument_id, None, None);
        Ok(())
    }

    fn on_stop(&mut self) -> nautilus_rt::anyhow::Result<()> {
        self.unsubscribe_quotes(self.instrument_id, None, None);
        Ok(())
    }

    fn on_quote(&mut self, quote: &QuoteTick) -> nautilus_rt::anyhow::Result<()> {
        self.ema_fast.handle_quote(quote)?;
        self.ema_slow.handle_quote(quote)?;

        if !self.ema_fast.initialized() || !self.ema_slow.initialized() {
            return Ok(());
        }

        let fast_above = self.ema_fast.value() > self.ema_slow.value();

        if let Some(prev) = self.prev_fast_above {
            if fast_above && !prev {
                self.enter(OrderSide::Buy)?;
            } else if !fast_above && prev {
                self.enter(OrderSide::Sell)?;
            }
        }

        self.prev_fast_above = Some(fast_above);
        Ok(())
    }
}
