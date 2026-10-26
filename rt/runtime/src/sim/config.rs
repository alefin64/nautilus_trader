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

//! Configuration of a bar simulation: venue and account rules, fill model, and session rules.
//!
//! Every field is required. There are no defaults, so a run's behavior is fully described by
//! its config.

use nautilus_model::{
    enums::{AccountType, OmsType},
    types::Currency,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::sessions::Segment;

/// Complete configuration of a bar simulation.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SimConfig {
    /// Venue, account, and position rules.
    pub venue: VenueConfig,
    /// How orders fill and what they cost.
    pub fill_model: FillModelConfig,
    /// What happens to positions at the end of each session.
    pub overnight: Overnight,
    /// Seed for the engine's random fill model. The next-bar-open fill model draws no random
    /// numbers, so the seed does not change results today. It is recorded for reproducibility.
    pub seed: u64,
}

/// Venue, account, and position rules.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VenueConfig {
    /// Order management system. Only `NETTING` is supported.
    pub oms_type: OmsType,
    /// `MARGIN` (allows shorting and leverage) or `CASH`.
    pub account_type: AccountType,
    /// Account currency. Must be the instrument's quote currency.
    pub base_currency: Currency,
    /// Starting cash balance, in `base_currency`.
    pub starting_capital: Decimal,
    /// Maximum gross exposure as a multiple of equity.
    pub max_leverage: Decimal,
    /// Maximum position value as a percentage of equity.
    pub max_position_pct_equity: Decimal,
    /// Whether short positions are allowed.
    pub allow_short: bool,
    /// Annual borrow cost of short positions, in basis points of position value.
    pub short_borrow_bps_per_year: Decimal,
}

impl VenueConfig {
    /// Returns the largest allowed position value as a fraction of equity: the tighter of
    /// `max_leverage` and `max_position_pct_equity`. Strategies trade a single instrument, so
    /// gross exposure equals position value.
    #[must_use]
    pub fn max_exposure_fraction(&self) -> Decimal {
        self.max_leverage
            .min(self.max_position_pct_equity / Decimal::ONE_HUNDRED)
    }
}

/// Fill model type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FillModelType {
    /// Every order fills at the open of the next bar after it is submitted, never on the bar
    /// it was submitted on.
    NextBarOpen,
}

/// How orders fill and what they cost.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FillModelConfig {
    /// Fill model type. Only `next_bar_open` exists.
    #[serde(rename = "type")]
    pub model_type: FillModelType,
    /// Slippage against the next bar's open, in basis points, by session segment.
    pub slippage_bps: SlippageConfig,
    /// Broker commission.
    pub commission: CommissionConfig,
}

/// Slippage in basis points of the fill bar's open, by the fill bar's session segment.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SlippageConfig {
    /// Pre-market bars.
    pub pre_market: Decimal,
    /// Regular-hours bars.
    pub regular: Decimal,
    /// After-hours bars.
    pub after_hours: Decimal,
}

impl SlippageConfig {
    /// Returns the slippage for `segment`, in basis points.
    #[must_use]
    pub const fn bps(&self, segment: Segment) -> Decimal {
        match segment {
            Segment::PreMarket => self.pre_market,
            Segment::Regular => self.regular,
            Segment::AfterHours => self.after_hours,
        }
    }
}

/// Broker commission: a rate per share with a minimum per order.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommissionConfig {
    /// Commission per share, in the account currency.
    pub per_share: Decimal,
    /// Minimum commission per order, in the account currency.
    pub min_per_order: Decimal,
}

/// What happens to positions at the end of each session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Overnight {
    /// Positions carry through the close and take the overnight gap.
    Hold,
    /// The simulation cancels working orders and closes positions so the exit fills on the
    /// session's last bar, and blocks new orders until the next session.
    Flatten,
}

impl SimConfig {
    /// Checks the config for values the simulation does not support.
    ///
    /// # Errors
    ///
    /// Returns an error describing every invalid value.
    pub fn validate(&self) -> anyhow::Result<()> {
        let venue = &self.venue;
        let fill = &self.fill_model;
        let mut errors = Vec::new();
        let mut check = |ok: bool, message: &str| {
            if !ok {
                errors.push(message.to_string());
            }
        };

        check(
            venue.oms_type == OmsType::Netting,
            "venue.oms_type must be NETTING",
        );
        check(
            matches!(venue.account_type, AccountType::Margin | AccountType::Cash),
            "venue.account_type must be MARGIN or CASH",
        );
        check(
            venue.starting_capital > Decimal::ZERO,
            "venue.starting_capital must be positive",
        );
        check(
            venue.max_leverage > Decimal::ZERO,
            "venue.max_leverage must be positive",
        );
        check(
            venue.max_position_pct_equity > Decimal::ZERO,
            "venue.max_position_pct_equity must be positive",
        );
        check(
            venue.short_borrow_bps_per_year >= Decimal::ZERO,
            "venue.short_borrow_bps_per_year must not be negative",
        );
        if venue.account_type == AccountType::Cash {
            check(
                !venue.allow_short,
                "venue.allow_short must be false for a CASH account",
            );
            check(
                venue.max_leverage <= Decimal::ONE,
                "venue.max_leverage must be at most 1 for a CASH account",
            );
        }
        for segment in Segment::ALL {
            let bps = fill.slippage_bps.bps(segment);
            check(
                bps >= Decimal::ZERO && bps < Decimal::from(10_000),
                &format!("fill_model.slippage_bps.{segment} must be in [0, 10000)"),
            );
        }
        check(
            fill.commission.per_share >= Decimal::ZERO,
            "fill_model.commission.per_share must not be negative",
        );
        check(
            fill.commission.min_per_order >= Decimal::ZERO,
            "fill_model.commission.min_per_order must not be negative",
        );

        anyhow::ensure!(errors.is_empty(), "invalid config: {}", errors.join("; "));
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use rstest::rstest;
    use rust_decimal_macros::dec;

    use super::*;

    pub(crate) fn sim_config() -> SimConfig {
        SimConfig {
            venue: VenueConfig {
                oms_type: OmsType::Netting,
                account_type: AccountType::Margin,
                base_currency: Currency::USD(),
                starting_capital: dec!(100000),
                max_leverage: dec!(1),
                max_position_pct_equity: dec!(100),
                allow_short: true,
                short_borrow_bps_per_year: dec!(50),
            },
            fill_model: FillModelConfig {
                model_type: FillModelType::NextBarOpen,
                slippage_bps: SlippageConfig {
                    pre_market: dec!(10),
                    regular: dec!(2),
                    after_hours: dec!(10),
                },
                commission: CommissionConfig {
                    per_share: dec!(0.005),
                    min_per_order: dec!(1),
                },
            },
            overnight: Overnight::Hold,
            seed: 0,
        }
    }

    #[rstest]
    fn valid_config_passes() {
        sim_config().validate().unwrap();
    }

    #[rstest]
    fn validate_reports_every_error() {
        let mut config = sim_config();
        config.venue.oms_type = OmsType::Hedging;
        config.venue.starting_capital = Decimal::ZERO;
        config.fill_model.slippage_bps.regular = dec!(-1);

        let err = config.validate().unwrap_err().to_string();

        assert!(err.contains("oms_type must be NETTING"), "{err}");
        assert!(err.contains("starting_capital must be positive"), "{err}");
        assert!(err.contains("slippage_bps.regular"), "{err}");
    }

    #[rstest]
    fn cash_account_cannot_short_or_lever() {
        let mut config = sim_config();
        config.venue.account_type = AccountType::Cash;
        config.venue.max_leverage = dec!(2);

        let err = config.validate().unwrap_err().to_string();

        assert!(err.contains("allow_short must be false"), "{err}");
        assert!(err.contains("max_leverage must be at most 1"), "{err}");
    }

    #[rstest]
    #[case(dec!(2), dec!(100), dec!(1))]
    #[case(dec!(1), dec!(50), dec!(0.5))]
    #[case(dec!(0.25), dec!(100), dec!(0.25))]
    fn max_exposure_fraction_takes_tighter_limit(
        #[case] leverage: Decimal,
        #[case] pct: Decimal,
        #[case] expected: Decimal,
    ) {
        let mut config = sim_config();
        config.venue.max_leverage = leverage;
        config.venue.max_position_pct_equity = pct;

        assert_eq!(config.venue.max_exposure_fraction(), expected);
    }
}
