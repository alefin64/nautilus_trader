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

//! Per-share commission with a minimum per order.

use nautilus_execution::models::fee::FeeModel;
use nautilus_model::{
    instruments::{Instrument, InstrumentAny},
    orders::{Order, OrderAny},
    types::{Money, Price, Quantity},
};
use rust_decimal::{Decimal, RoundingStrategy};

use super::config::CommissionConfig;

/// Charges `per_share` for every share filled, with at least `min_per_order` per order.
///
/// The engine calls the model once per fill. Each fill is charged the increase in the order's
/// total commission, so an order filled in parts pays the same as one filled at once. Totals
/// are rounded to the currency's precision, half away from zero.
#[derive(Clone, Debug)]
pub struct PerShareFeeModel {
    per_share: Decimal,
    min_per_order: Decimal,
}

impl PerShareFeeModel {
    /// Creates the model from `config`.
    #[must_use]
    pub fn new(config: &CommissionConfig) -> Self {
        Self {
            per_share: config.per_share,
            min_per_order: config.min_per_order,
        }
    }

    fn order_total(&self, filled: Decimal, precision: u32) -> Decimal {
        if filled.is_zero() {
            return Decimal::ZERO;
        }
        (self.per_share * filled)
            .max(self.min_per_order)
            .round_dp_with_strategy(precision, RoundingStrategy::MidpointAwayFromZero)
    }
}

impl FeeModel for PerShareFeeModel {
    fn get_commission(
        &self,
        order: &OrderAny,
        fill_quantity: Quantity,
        _fill_px: Price,
        instrument: &InstrumentAny,
    ) -> anyhow::Result<Money> {
        let currency = instrument.quote_currency();
        let precision = u32::from(currency.precision);
        // The engine passes the order with its quantity filled before this fill
        let before = order.filled_qty().as_decimal();
        let after = before + fill_quantity.as_decimal();
        let commission = self.order_total(after, precision) - self.order_total(before, precision);
        Ok(Money::from_decimal(commission, currency)?)
    }
}

#[cfg(test)]
mod tests {
    use nautilus_core::{UUID4, UnixNanos};
    use nautilus_model::{
        enums::{OrderSide, TimeInForce},
        identifiers::{ClientOrderId, InstrumentId, StrategyId, TraderId},
        orders::MarketOrder,
        types::Currency,
    };
    use rstest::rstest;
    use rust_decimal_macros::dec;

    use super::*;
    use crate::catalog;

    fn model() -> PerShareFeeModel {
        PerShareFeeModel::new(&CommissionConfig {
            per_share: dec!(0.005),
            min_per_order: dec!(1),
        })
    }

    fn instrument() -> InstrumentAny {
        catalog::equity(
            InstrumentId::from("SYNTH.XNYS"),
            Currency::USD(),
            4,
            UnixNanos::default(),
        )
        .unwrap()
    }

    fn order(quantity: u64) -> OrderAny {
        OrderAny::Market(MarketOrder::new(
            TraderId::from("TESTER-001"),
            StrategyId::from("S-001"),
            InstrumentId::from("SYNTH.XNYS"),
            ClientOrderId::from("O-1"),
            OrderSide::Buy,
            Quantity::from(quantity),
            TimeInForce::Gtc,
            UUID4::new(),
            UnixNanos::default(),
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ))
    }

    fn commission(quantity: u64) -> Money {
        model()
            .get_commission(
                &order(quantity),
                Quantity::from(quantity),
                Price::from("50.0000"),
                &instrument(),
            )
            .unwrap()
    }

    #[rstest]
    #[case(100, "1.00 USD")] // 0.50 raised to the minimum
    #[case(200, "1.00 USD")]
    #[case(1000, "5.00 USD")]
    #[case(1001, "5.01 USD")] // 5.005 rounds half away from zero
    fn charges_per_share_with_minimum(#[case] quantity: u64, #[case] expected: &str) {
        assert_eq!(commission(quantity), Money::from(expected));
    }
}
