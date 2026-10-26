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

//! Runner config, read from TOML.
//!
//! Every field is required, so the config file alone describes a run. Relative paths resolve
//! from the process working directory.

use std::path::PathBuf;

use nautilus_rt::{
    jiff::civil::Date,
    loader::StrategyPluginConfig,
    model::{data::BarType, identifiers::InstrumentId},
    serde::{Deserialize, Deserializer, Serialize, de::Error},
    sim::{FillModelConfig, Overnight, SimConfig, VenueConfig},
};

/// Complete runner config.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(crate = "nautilus_rt::serde", deny_unknown_fields)]
pub(crate) struct RunnerConfig {
    /// Seed recorded with the run and passed to the engine's fill model.
    pub(crate) seed: u64,
    /// Market data to run on.
    pub(crate) data: DataConfig,
    /// Session rules.
    pub(crate) session: SessionConfig,
    /// Venue, account, and position rules.
    pub(crate) venue: VenueConfig,
    /// How orders fill and what they cost.
    pub(crate) fill_model: FillModelConfig,
    /// Strategies to load from plug-ins.
    pub(crate) strategies: Vec<StrategyPluginConfig>,
}

/// Where the bars come from and which to use.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(crate = "nautilus_rt::serde", deny_unknown_fields)]
pub(crate) struct DataConfig {
    /// Nautilus Parquet data catalog directory.
    pub(crate) catalog: PathBuf,
    /// Session schedule (`sessions.parquet`).
    pub(crate) sessions: PathBuf,
    /// Instrument to trade, defined in the catalog.
    pub(crate) instrument_id: InstrumentId,
    /// Bars to run on, stored in the catalog.
    pub(crate) bar_type: BarType,
    /// First trading day, inclusive.
    #[serde(deserialize_with = "deserialize_date")]
    pub(crate) start: Date,
    /// Last trading day, inclusive.
    #[serde(deserialize_with = "deserialize_date")]
    pub(crate) end: Date,
}

/// Session rules.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(crate = "nautilus_rt::serde", deny_unknown_fields)]
pub(crate) struct SessionConfig {
    /// Name of the session profile the catalog was built for. Recorded only.
    pub(crate) profile: String,
    /// `hold` or `flatten`.
    pub(crate) overnight: Overnight,
}

impl RunnerConfig {
    /// Parses and checks a config.
    pub(crate) fn from_toml(text: &str) -> nautilus_rt::anyhow::Result<Self> {
        let config: Self = toml::from_str(text)?;
        nautilus_rt::anyhow::ensure!(
            config.data.start <= config.data.end,
            "data.start {} is after data.end {}",
            config.data.start,
            config.data.end
        );
        nautilus_rt::anyhow::ensure!(
            !config.strategies.is_empty(),
            "no [[strategies]] configured"
        );
        nautilus_rt::anyhow::ensure!(
            config.data.bar_type.instrument_id() == config.data.instrument_id,
            "data.bar_type {} is not for data.instrument_id {}",
            config.data.bar_type,
            config.data.instrument_id
        );
        config.sim_config().validate()?;
        Ok(config)
    }

    /// Returns the simulation part of the config.
    pub(crate) fn sim_config(&self) -> SimConfig {
        SimConfig {
            venue: self.venue.clone(),
            fill_model: self.fill_model.clone(),
            overnight: self.session.overnight,
            seed: self.seed,
        }
    }
}

// Accepts a TOML date (`2016-10-03`) or a string holding one
fn deserialize_date<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Date, D::Error> {
    let text = match toml::Value::deserialize(deserializer)? {
        toml::Value::Datetime(datetime) if datetime.time.is_none() && datetime.offset.is_none() => {
            datetime.to_string()
        }
        toml::Value::String(text) => text,
        other => {
            return Err(D::Error::custom(format!(
                "expected a date such as 2016-10-03, found {other:?}"
            )));
        }
    };
    text.parse().map_err(D::Error::custom)
}

#[cfg(test)]
pub(crate) mod tests {
    use rstest::rstest;

    use super::*;

    pub(crate) const CONFIG: &str = r#"
seed = 0

[data]
catalog = "data/train/catalog"
sessions = "data/train/sessions.parquet"
instrument_id = "SYNTH.XNYS"
bar_type = "SYNTH.XNYS-5-MINUTE-LAST-EXTERNAL"
start = 2016-10-03
end = 2022-12-31

[session]
profile = "rth"
overnight = "hold"

[venue]
oms_type = "NETTING"
account_type = "MARGIN"
base_currency = "USD"
starting_capital = 100000
max_leverage = 1.0
max_position_pct_equity = 100
allow_short = true
short_borrow_bps_per_year = 50

[fill_model]
type = "next_bar_open"

[fill_model.slippage_bps]
regular = 2
pre_market = 10
after_hours = 10

[fill_model.commission]
per_share = 0.005
min_per_order = 1.0

[[strategies]]
path = "target/debug/libstrategy_plugin.so"
type_name = "Strategy"

[strategies.config]
fast_period = 10
"#;

    #[rstest]
    fn parses_full_config() {
        let config = RunnerConfig::from_toml(CONFIG).unwrap();

        assert_eq!(config.data.start, Date::constant(2016, 10, 3));
        assert_eq!(config.data.end, Date::constant(2022, 12, 31));
        assert_eq!(config.session.overnight, Overnight::Hold);
        assert_eq!(config.venue.starting_capital.to_string(), "100000");
        assert_eq!(config.fill_model.commission.per_share.to_string(), "0.005");
        assert_eq!(config.fill_model.slippage_bps.regular.to_string(), "2");
        assert_eq!(config.strategies.len(), 1);
    }

    #[rstest]
    fn accepts_quoted_dates() {
        let text = CONFIG.replace("start = 2016-10-03", "start = \"2016-10-03\"");

        let config = RunnerConfig::from_toml(&text).unwrap();

        assert_eq!(config.data.start, Date::constant(2016, 10, 3));
    }

    #[rstest]
    #[case::missing_field("seed = 0\n", "", "missing field `seed`")]
    #[case::unknown_field("seed = 0", "seed = 0\ncycles = 3", "unknown field `cycles`")]
    #[case::reversed_dates("end = 2022-12-31", "end = 2015-12-31", "is after data.end")]
    #[case::datetime("start = 2016-10-03", "start = 2016-10-03T09:30:00", "expected a date")]
    #[case::bad_overnight("overnight = \"hold\"", "overnight = \"close\"", "unknown variant")]
    #[case::hedging(
        "oms_type = \"NETTING\"",
        "oms_type = \"HEDGING\"",
        "oms_type must be NETTING"
    )]
    #[case::fill_type(
        "type = \"next_bar_open\"",
        "type = \"same_bar_close\"",
        "unknown variant"
    )]
    #[case::wrong_bar_type(
        "bar_type = \"SYNTH.XNYS-5-MINUTE-LAST-EXTERNAL\"",
        "bar_type = \"OTHER.XNYS-5-MINUTE-LAST-EXTERNAL\"",
        "is not for data.instrument_id"
    )]
    fn rejects_invalid_config(#[case] from: &str, #[case] to: &str, #[case] message: &str) {
        let text = CONFIG.replace(from, to);

        let err = RunnerConfig::from_toml(&text).unwrap_err();

        assert!(format!("{err:#}").contains(message), "{err:#}");
    }

    #[rstest]
    fn every_value_is_required() {
        for key in [
            "starting_capital",
            "max_leverage",
            "allow_short",
            "short_borrow_bps_per_year",
            "pre_market",
            "min_per_order",
            "profile",
            "overnight",
            "sessions",
        ] {
            let text: String = CONFIG
                .lines()
                .filter(|line| !line.starts_with(&format!("{key} =")))
                .map(|line| format!("{line}\n"))
                .collect();

            let err = RunnerConfig::from_toml(&text).unwrap_err();

            assert!(
                format!("{err:#}").contains(&format!("missing field `{key}`")),
                "{key}: {err:#}"
            );
        }
    }
}
