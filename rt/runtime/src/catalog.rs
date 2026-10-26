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

//! Reads and writes instruments and bars in a Nautilus Parquet data catalog.

use std::path::Path;

use anyhow::Context;
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::{Bar, BarType},
    identifiers::InstrumentId,
    instruments::{Equity, Instrument, InstrumentAny},
    types::{Currency, Price, Quantity},
};
use nautilus_persistence::backend::parquet::catalog::ParquetDataCatalog;

/// Returns a whole-share equity instrument quoted in `currency` with `price_precision`
/// decimals.
///
/// Margin rates are left at zero: the bar simulation enforces leverage and position limits
/// itself (see [`crate::sim`]).
///
/// # Errors
///
/// Returns an error if the instrument fails validation.
pub fn equity(
    instrument_id: InstrumentId,
    currency: Currency,
    price_precision: u8,
    ts_init: UnixNanos,
) -> anyhow::Result<InstrumentAny> {
    let equity = Equity::builder()
        .instrument_id(instrument_id)
        .raw_symbol(instrument_id.symbol)
        .currency(currency)
        .price_precision(price_precision)
        .price_increment(Price::new(
            10f64.powi(-i32::from(price_precision)),
            price_precision,
        ))
        .lot_size(Quantity::from(1))
        .min_quantity(Quantity::from(1))
        .ts_event(ts_init)
        .ts_init(ts_init)
        .build()?;
    Ok(InstrumentAny::Equity(equity))
}

fn open(path: &Path) -> ParquetDataCatalog {
    ParquetDataCatalog::new(path, None, None, None, None)
}

/// Writes `instrument` and its `bars` to the catalog at `path`.
///
/// # Errors
///
/// Returns an error if the bars are not in ascending time order, belong to another instrument,
/// or the files cannot be written.
pub fn write_bars(path: &Path, instrument: &InstrumentAny, bars: &[Bar]) -> anyhow::Result<()> {
    if let Some(bar) = bars
        .iter()
        .find(|bar| bar.bar_type.instrument_id() != instrument.id())
    {
        anyhow::bail!(
            "bar type {} does not belong to instrument {}",
            bar.bar_type,
            instrument.id()
        );
    }
    std::fs::create_dir_all(path)?;
    let catalog = open(path);
    catalog
        .write_instruments(vec![instrument.clone()])
        .with_context(|| format!("failed to write instrument to '{}'", path.display()))?;
    catalog
        .write_to_parquet(bars, None, None, None)
        .with_context(|| format!("failed to write bars to '{}'", path.display()))?;
    Ok(())
}

/// Reads the instrument `instrument_id` from the catalog at `path`.
///
/// # Errors
///
/// Returns an error if the catalog cannot be read or does not hold exactly one definition of
/// the instrument.
pub fn read_instrument(path: &Path, instrument_id: InstrumentId) -> anyhow::Result<InstrumentAny> {
    ensure_exists(path)?;
    let mut instruments = open(path)
        .instruments(Some(&[instrument_id.to_string()]), None, None)
        .with_context(|| format!("failed to read instruments from '{}'", path.display()))?;
    instruments.retain(|instrument| instrument.id() == instrument_id);
    match instruments.len() {
        1 => Ok(instruments.remove(0)),
        0 => anyhow::bail!(
            "catalog '{}' has no instrument {instrument_id}",
            path.display()
        ),
        n => anyhow::bail!(
            "catalog '{}' has {n} definitions of instrument {instrument_id}",
            path.display()
        ),
    }
}

/// Reads the bars of `bar_type` from the catalog at `path`, optionally limited to
/// `ts_init` in `[start, end]`.
///
/// # Errors
///
/// Returns an error if the catalog cannot be read.
pub fn read_bars(
    path: &Path,
    bar_type: BarType,
    start: Option<UnixNanos>,
    end: Option<UnixNanos>,
) -> anyhow::Result<Vec<Bar>> {
    ensure_exists(path)?;
    let mut bars = open(path)
        .query::<Bar>(
            Some(vec![bar_type.to_string()]),
            start,
            end,
            None,
            None,
            true,
        )
        .with_context(|| format!("failed to read bars from '{}'", path.display()))?;
    bars.retain(|bar| bar.bar_type == bar_type);
    Ok(bars)
}

fn ensure_exists(path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        path.is_dir(),
        "catalog directory '{}' not found",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tempfile::TempDir;

    use super::*;
    use crate::synthetic;

    #[rstest]
    fn bars_round_trip() {
        let dir = TempDir::new().unwrap();
        let scenario = synthetic::EquityScenario::new(2).unwrap();

        write_bars(dir.path(), &scenario.instrument, &scenario.bars).unwrap();
        let instrument = read_instrument(dir.path(), scenario.instrument.id()).unwrap();
        let bars = read_bars(dir.path(), scenario.bar_type, None, None).unwrap();

        assert_eq!(instrument, scenario.instrument);
        assert_eq!(bars, scenario.bars);
    }

    #[rstest]
    fn read_bars_filters_time_range() {
        let dir = TempDir::new().unwrap();
        let scenario = synthetic::EquityScenario::new(2).unwrap();
        write_bars(dir.path(), &scenario.instrument, &scenario.bars).unwrap();
        let start = scenario.bars[10].ts_init;
        let end = scenario.bars[19].ts_init;

        let bars = read_bars(dir.path(), scenario.bar_type, Some(start), Some(end)).unwrap();

        assert_eq!(bars, scenario.bars[10..20]);
    }

    #[rstest]
    fn read_instrument_reports_unknown_instrument() {
        let dir = TempDir::new().unwrap();
        let scenario = synthetic::EquityScenario::new(1).unwrap();
        write_bars(dir.path(), &scenario.instrument, &scenario.bars).unwrap();

        let err = read_instrument(dir.path(), InstrumentId::from("OTHER.XNYS")).unwrap_err();

        assert!(err.to_string().contains("no instrument OTHER.XNYS"));
    }

    #[rstest]
    fn read_bars_reports_missing_catalog() {
        let err = read_bars(
            Path::new("does/not/exist"),
            BarType::from("SYNTH.XNYS-5-MINUTE-LAST-EXTERNAL"),
            None,
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("not found"));
    }
}
