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

//! Output files of a run.
//!
//! All files are deterministic: the same inputs give byte-identical files. They hold no
//! wall-clock times, random IDs, or paths other than those in the config.

use std::{fs::File, path::Path, sync::Arc};

use nautilus_rt::{
    anyhow::{self, Context},
    arrow::{
        array::{
            ArrayRef, BooleanArray, Date32Array, Float64Array, RecordBatch, StringArray,
            TimestampNanosecondArray, UInt64Array,
        },
        datatypes::{Field, Schema},
    },
    core::UnixNanos,
    jiff::civil::Date,
    model::enums::{OrderSide, PositionSide},
    parquet::{
        arrow::ArrowWriter,
        basic::{Compression, ZstdLevel},
        file::properties::WriterProperties,
    },
    rust_decimal::{Decimal, prelude::ToPrimitive},
    sessions::date_to_days,
    sim::{Metrics, SimOutcome},
};

/// Summary metrics file.
pub(crate) const RESULTS_FILE: &str = "results.parquet";
/// Fills and positions file.
pub(crate) const TRADES_FILE: &str = "trades.parquet";
/// Daily equity and returns file.
pub(crate) const RETURNS_FILE: &str = "returns.parquet";
/// Run description file.
pub(crate) const RUN_FILE: &str = "run.json";

/// Identification columns repeated in `results.parquet`.
pub(crate) struct RunLabels<'a> {
    pub(crate) instrument_id: &'a str,
    pub(crate) bar_type: &'a str,
    pub(crate) session_profile: &'a str,
    pub(crate) overnight: &'a str,
    pub(crate) seed: u64,
    pub(crate) build_fingerprint: &'a str,
}

/// Writes `results.parquet`: one row of summary metrics.
pub(crate) fn write_results(
    path: &Path,
    metrics: &Metrics,
    labels: &RunLabels<'_>,
) -> anyhow::Result<()> {
    let m = metrics;
    let win_rate = (m.trade_count > 0).then(|| m.winning_trades as f64 / m.trade_count as f64);
    let columns: Vec<(&str, ArrayRef)> = vec![
        ("instrument_id", utf8([labels.instrument_id])),
        ("bar_type", utf8([labels.bar_type])),
        ("session_profile", utf8([labels.session_profile])),
        ("overnight", utf8([labels.overnight])),
        ("seed", uint([labels.seed])),
        ("build_fingerprint", utf8([labels.build_fingerprint])),
        ("start_date", dates(&[m.start_date])?),
        ("end_date", dates(&[m.end_date])?),
        ("sessions", uint([m.sessions as u64])),
        ("bars", uint([m.bars as u64])),
        ("starting_capital", float([m.starting_capital])),
        ("ending_equity", float([m.ending_equity])),
        ("net_pnl", float([m.net_pnl])),
        ("gross_pnl", float([m.gross_pnl])),
        ("total_costs", float([m.total_costs])),
        ("commissions", float([m.commissions])),
        ("slippage", float([m.slippage])),
        ("borrow", float([m.borrow])),
        (
            "return_pct",
            Arc::new(Float64Array::from(vec![m.return_pct])),
        ),
        ("sharpe", Arc::new(Float64Array::from(vec![m.sharpe]))),
        (
            "max_drawdown_pct",
            Arc::new(Float64Array::from(vec![m.max_drawdown_pct])),
        ),
        ("trade_count", uint([m.trade_count as u64])),
        ("winning_trades", uint([m.winning_trades as u64])),
        ("win_rate", Arc::new(Float64Array::from(vec![win_rate]))),
        ("fill_count", uint([m.fill_count as u64])),
        ("order_count", uint([m.order_count as u64])),
        ("denied_orders", uint([m.denied_orders as u64])),
        ("traded_notional", float([m.traded_notional])),
        ("turnover", Arc::new(Float64Array::from(vec![m.turnover]))),
        (
            "exposure_pct",
            Arc::new(Float64Array::from(vec![m.exposure_pct])),
        ),
        (
            "avg_gross_exposure",
            Arc::new(Float64Array::from(vec![m.avg_gross_exposure])),
        ),
        ("final_position", float([m.final_position])),
    ];
    write(path, columns)
}

/// Writes `trades.parquet`: one row per fill (`record = "fill"`) followed by one row per
/// position from open to flat (`record = "position"`).
pub(crate) fn write_trades(path: &Path, outcome: &SimOutcome) -> anyhow::Result<()> {
    let fills = &outcome.fills;
    let trips = &outcome.round_trips;

    let mut record = vec!["fill"; fills.len()];
    record.extend(vec!["position"; trips.len()]);
    let strategy_id = fills
        .iter()
        .map(|r| r.fill.strategy_id.to_string())
        .chain(trips.iter().map(|t| t.strategy_id.to_string()))
        .collect::<Vec<_>>();
    let instrument_id = fills
        .iter()
        .map(|r| r.fill.instrument_id.to_string())
        .chain(trips.iter().map(|t| t.instrument_id.to_string()))
        .collect::<Vec<_>>();
    let side = fills
        .iter()
        .map(|r| order_side(r.fill.side))
        .chain(trips.iter().map(|t| position_side(t.side)))
        .collect::<Vec<_>>();
    let quantity = fills
        .iter()
        .map(|r| r.fill.quantity)
        .chain(trips.iter().map(|t| t.peak_quantity));
    let price = fills
        .iter()
        .map(|r| r.fill.price)
        .chain(trips.iter().map(|t| t.avg_px_open));
    let ts = fills
        .iter()
        .map(|r| Some(r.fill.ts_event))
        .chain(trips.iter().map(|t| Some(t.ts_opened)));
    let ts_closed = nones(fills.len()).chain(trips.iter().map(|t| t.ts_closed));
    let price_close = nones(fills.len()).chain(trips.iter().map(|t| t.avg_px_close));
    let commission = fills
        .iter()
        .map(|r| r.fill.commission)
        .chain(trips.iter().map(|t| t.commissions));
    let slippage = fills
        .iter()
        .map(|r| r.slippage)
        .chain(trips.iter().map(|t| t.slippage));
    let pnl_gross = nones(fills.len()).chain(trips.iter().map(|t| Some(t.pnl_gross)));
    let pnl_net = nones(fills.len()).chain(trips.iter().map(|t| Some(t.pnl_net)));
    let open_price = fills
        .iter()
        .map(|r| Some(r.open_price))
        .chain(nones(trips.len()));
    let segment = fills
        .iter()
        .map(|r| Some(r.segment.as_str()))
        .chain(nones(trips.len()));
    let client_order_id = fills
        .iter()
        .map(|r| Some(r.fill.client_order_id.to_string()))
        .chain(nones(trips.len()));
    let venue_order_id = fills
        .iter()
        .map(|r| Some(r.fill.venue_order_id.to_string()))
        .chain(nones(trips.len()));
    let trade_id = fills
        .iter()
        .map(|r| Some(r.fill.trade_id.to_string()))
        .chain(nones(trips.len()));
    let flatten = fills
        .iter()
        .map(|r| Some(r.fill.flatten))
        .chain(nones(trips.len()));

    let columns: Vec<(&str, ArrayRef)> = vec![
        ("record", Arc::new(StringArray::from(record))),
        ("strategy_id", Arc::new(StringArray::from(strategy_id))),
        ("instrument_id", Arc::new(StringArray::from(instrument_id))),
        ("side", Arc::new(StringArray::from(side))),
        ("quantity", float(quantity)),
        ("price", float(price)),
        ("ts", timestamps(ts)),
        ("ts_closed", timestamps(ts_closed)),
        ("price_close", float_opt(price_close)),
        ("commission", float(commission)),
        ("slippage", float(slippage)),
        ("pnl_gross", float_opt(pnl_gross)),
        ("pnl_net", float_opt(pnl_net)),
        ("open_price", float_opt(open_price)),
        ("segment", Arc::new(segment.collect::<StringArray>())),
        (
            "client_order_id",
            Arc::new(client_order_id.collect::<StringArray>()),
        ),
        (
            "venue_order_id",
            Arc::new(venue_order_id.collect::<StringArray>()),
        ),
        ("trade_id", Arc::new(trade_id.collect::<StringArray>())),
        ("flatten", Arc::new(flatten.collect::<BooleanArray>())),
    ];
    write(path, columns)
}

/// Writes `returns.parquet`: one row per trading day.
pub(crate) fn write_returns(path: &Path, outcome: &SimOutcome) -> anyhow::Result<()> {
    let daily = &outcome.daily;
    let columns: Vec<(&str, ArrayRef)> = vec![
        (
            "date",
            dates(&daily.iter().map(|d| d.date).collect::<Vec<_>>())?,
        ),
        ("ts", timestamps(daily.iter().map(|d| Some(d.ts)))),
        ("close", float(daily.iter().map(|d| d.close))),
        ("position", float(daily.iter().map(|d| d.position))),
        ("equity", float(daily.iter().map(|d| d.equity))),
        ("pnl", float(daily.iter().map(|d| d.pnl))),
        (
            "return",
            Arc::new(Float64Array::from(
                daily.iter().map(|d| d.return_).collect::<Vec<_>>(),
            )),
        ),
        ("borrow", float(daily.iter().map(|d| d.borrow))),
    ];
    write(path, columns)
}

fn write(path: &Path, columns: Vec<(&str, ArrayRef)>) -> anyhow::Result<()> {
    // Every column is nullable, so the schema does not depend on the values
    let fields: Vec<Field> = columns
        .iter()
        .map(|(name, array)| Field::new(*name, array.data_type().clone(), true))
        .collect();
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        columns.into_iter().map(|(_, array)| array).collect(),
    )?;
    let properties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .build();
    let file =
        File::create(path).with_context(|| format!("failed to create '{}'", path.display()))?;
    let mut writer = ArrowWriter::try_new(file, schema, Some(properties))?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}

fn nones<T>(n: usize) -> impl Iterator<Item = Option<T>> {
    std::iter::repeat_with(|| None).take(n)
}

fn utf8<const N: usize>(values: [&str; N]) -> ArrayRef {
    Arc::new(StringArray::from(values.to_vec()))
}

fn uint<const N: usize>(values: [u64; N]) -> ArrayRef {
    Arc::new(UInt64Array::from(values.to_vec()))
}

fn float(values: impl IntoIterator<Item = Decimal>) -> ArrayRef {
    Arc::new(
        values
            .into_iter()
            .map(|d| d.to_f64())
            .collect::<Float64Array>(),
    )
}

fn float_opt(values: impl IntoIterator<Item = Option<Decimal>>) -> ArrayRef {
    Arc::new(
        values
            .into_iter()
            .map(|d| d.and_then(|d| d.to_f64()))
            .collect::<Float64Array>(),
    )
}

fn dates(values: &[Date]) -> anyhow::Result<ArrayRef> {
    let days = values
        .iter()
        .map(|d| date_to_days(*d))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Arc::new(Date32Array::from(days)))
}

fn timestamps(values: impl IntoIterator<Item = Option<UnixNanos>>) -> ArrayRef {
    let array = values
        .into_iter()
        .map(|ts| ts.map(|ts| i64::try_from(ts.as_u64()).expect("timestamp fits in i64")))
        .collect::<TimestampNanosecondArray>()
        .with_timezone("UTC");
    Arc::new(array)
}

const fn order_side(side: OrderSide) -> &'static str {
    match side {
        OrderSide::Sell => "SELL",
        _ => "BUY",
    }
}

const fn position_side(side: PositionSide) -> &'static str {
    match side {
        PositionSide::Short => "SHORT",
        _ => "LONG",
    }
}
