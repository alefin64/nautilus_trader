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

//! Converts raw 5-minute bar files into one Nautilus Parquet catalog per data split.
//!
//! ```text
//! nautilus-ingest --raw out/SYNTH --sessions sessions.parquet \
//!     --profile rth --segments regular --instrument-id SYNTH.XNYS \
//!     --split train=2016-10-03..2022-12-31 --split validation=2023-01-01..2024-12-31 \
//!     --split holdout=2025-01-01..2026-09-30 --embargo-days 7 --output data/rth
//! ```
//!
//! Every raw bar must lie inside a segment of the sessions file, or the ingest fails. Bars in
//! the profile's segments and inside a split are written to `<output>/<split>/catalog`, stamped
//! at their close (`ts_event = ts_init = start + interval`), with the instrument definition. The
//! sessions file is copied to `<output>/<split>/sessions.parquet`, and
//! `<output>/ingest-manifest.json` records the inputs, parameters, row counts, and hashes.

mod raw;
mod splits;

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use clap::Parser;
use nautilus_rt::{
    anyhow::{self, Context},
    blake3, catalog,
    core::UnixNanos,
    digest,
    jiff::civil::Date,
    model::{
        data::{Bar, BarType},
        identifiers::InstrumentId,
        types::{Currency, Price, Quantity},
    },
    serde_json::{self, Value, json},
    sessions::{Segment, SessionSchedule},
};

use crate::{
    raw::RawBar,
    splits::{Split, SplitSpec, apply_embargo},
};

/// Format identifier of `ingest-manifest.json`.
const MANIFEST_FORMAT: &str = "nautilus-ingest-manifest/1";
const MANIFEST_FILE: &str = "ingest-manifest.json";
const MINUTE_NS: u64 = 60_000_000_000;
/// Bars outside session segments listed in an error.
const MAX_REPORTED: usize = 5;

#[derive(Debug, Parser)]
#[command(about = "Convert raw bar files into per-split Nautilus Parquet catalogs")]
struct Cli {
    /// Raw bar Parquet file, or a directory searched recursively for `*.parquet` files.
    #[arg(long)]
    raw: PathBuf,
    /// Sessions file (`sessions.parquet`) covering every raw bar.
    #[arg(long)]
    sessions: PathBuf,
    /// Session profile name, recorded in the manifest.
    #[arg(long)]
    profile: String,
    /// Exchange calendar the sessions file was built from, recorded in the manifest.
    #[arg(long)]
    calendar: String,
    /// Segments of the profile, comma separated: pre_market, regular, after_hours.
    #[arg(long, value_delimiter = ',', required = true)]
    segments: Vec<Segment>,
    /// Instrument ID of the bars, such as `SYNTH.XNYS`. The instrument is a whole-share equity.
    #[arg(long)]
    instrument_id: InstrumentId,
    /// Quote currency of the instrument.
    #[arg(long)]
    currency: Currency,
    /// Price precision of the instrument and bars. Raw prices must have no more decimals.
    #[arg(long)]
    price_precision: u8,
    /// Bar length in minutes.
    #[arg(long)]
    bar_minutes: u64,
    /// A split as `name=START..END` (trading days, inclusive), in chronological order.
    #[arg(long = "split", required = true)]
    splits: Vec<SplitSpec>,
    /// Calendar days removed from the end of a split before the next split starts.
    #[arg(long)]
    embargo_days: u32,
    /// Output directory. Must not exist or be empty.
    #[arg(long)]
    output: PathBuf,
}

/// A raw bar with its session.
struct SessionRawBar {
    bar: RawBar,
    date: Date,
    segment: Segment,
}

fn main() -> anyhow::Result<()> {
    nautilus_rt::ensure_host_build(nautilus_rt::BUILD_FINGERPRINT)?;
    let cli = Cli::parse();
    let summary = ingest(&cli)?;
    println!("{summary}");
    Ok(())
}

fn ingest(cli: &Cli) -> anyhow::Result<String> {
    check_output_dir(&cli.output)?;
    let mut segments = cli.segments.clone();
    segments.sort_unstable();
    segments.dedup();
    let splits = apply_embargo(&cli.splits, cli.embargo_days)?;
    anyhow::ensure!(cli.bar_minutes > 0, "--bar-minutes must be positive");
    let interval = cli.bar_minutes * MINUTE_NS;
    let bar_type = BarType::from(
        format!(
            "{}-{}-MINUTE-LAST-EXTERNAL",
            cli.instrument_id, cli.bar_minutes
        )
        .as_str(),
    );

    // Read inputs
    let schedule = SessionSchedule::read_parquet(&cli.sessions)?;
    let files = raw::list_files(&cli.raw)?;
    let mut raw_bars = Vec::new();
    let mut raw_inputs = Vec::new();
    for file in &files {
        let bars = raw::read_file(file)?;
        raw_inputs.push(json!({
            "path": relative_path(&cli.raw, file),
            "blake3": digest::file(file)?,
            "rows": bars.len(),
        }));
        raw_bars.extend(bars);
    }
    raw_bars.sort_by_key(|bar| bar.ts_start);
    if let Some(pair) = raw_bars
        .windows(2)
        .find(|pair| pair[0].ts_start == pair[1].ts_start)
    {
        anyhow::bail!(
            "raw data has more than one bar starting at {}",
            UnixNanos::from(pair[0].ts_start).to_rfc3339()
        );
    }

    let bars = assign_sessions(&raw_bars, &schedule, interval)?;
    let mut raw_by_segment: BTreeMap<Segment, usize> = BTreeMap::new();
    for bar in &bars {
        *raw_by_segment.entry(bar.segment).or_default() += 1;
    }
    for segment in &segments {
        anyhow::ensure!(
            raw_by_segment.contains_key(segment),
            "profile segment {segment} has no bars in the raw data"
        );
    }

    // Write one catalog per split
    fs::create_dir_all(&cli.output)?;
    let mut split_entries = Vec::new();
    let mut in_splits = 0;
    for split in &splits {
        let selected: Vec<&SessionRawBar> = bars
            .iter()
            .filter(|b| split.contains(b.date) && segments.contains(&b.segment))
            .collect();
        anyhow::ensure!(
            !selected.is_empty(),
            "split '{}' has no bars in the profile's segments",
            split.name
        );
        in_splits += selected.len();
        let entry = write_split(cli, split, &selected, bar_type, interval)?;
        split_entries.push(entry);
    }

    // Manifest
    let segment_names: Vec<&str> = segments.iter().map(|s| s.as_str()).collect();
    let identity = json!({
        "inputs": {
            "raw_files": raw_inputs,
            "sessions_blake3": digest::file(&cli.sessions)?,
        },
        "instrument": {
            "instrument_id": cli.instrument_id.to_string(),
            "currency": cli.currency.code.as_str(),
            "price_precision": cli.price_precision,
            "bar_type": bar_type.to_string(),
        },
        "profile": {
            "name": cli.profile,
            "calendar": cli.calendar,
            "segments": segment_names,
        },
        "embargo_days": cli.embargo_days,
        "splits": splits.iter().map(|s| json!({
            "name": s.name,
            "start": s.start.to_string(),
            "end": s.end.to_string(),
            "effective_end": s.effective_end.to_string(),
        })).collect::<Vec<_>>(),
    });
    let data_profile_hash = blake3::hash(serde_json::to_string(&identity)?.as_bytes())
        .to_hex()
        .to_string();
    let row_counts = json!({
        "raw": raw_bars.len(),
        "raw_by_segment": raw_by_segment
            .iter()
            .map(|(segment, count)| (segment.as_str().to_string(), json!(count)))
            .collect::<serde_json::Map<_, _>>(),
        "in_splits": in_splits,
    });
    let mut manifest = json!({
        "format": MANIFEST_FORMAT,
        "engine": {
            "version": env!("CARGO_PKG_VERSION"),
            "build_fingerprint": nautilus_rt::BUILD_FINGERPRINT,
        },
    });
    let object = manifest.as_object_mut().expect("manifest is an object");
    if let Value::Object(identity) = identity {
        object.extend(identity);
    }
    object.insert("row_counts".to_string(), row_counts);
    object.insert("split_outputs".to_string(), Value::Array(split_entries));
    object.insert("data_profile_hash".to_string(), json!(data_profile_hash));
    let mut text = serde_json::to_string_pretty(&manifest)?;
    text.push('\n');
    fs::write(cli.output.join(MANIFEST_FILE), text)?;

    Ok(format!(
        "Ingested {} raw bars into {} splits ({in_splits} bars in profile '{}'), data_profile_hash {data_profile_hash}",
        raw_bars.len(),
        splits.len(),
        cli.profile
    ))
}

fn check_output_dir(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        let empty = fs::read_dir(path)
            .with_context(|| format!("output '{}' is not a directory", path.display()))?
            .next()
            .is_none();
        anyhow::ensure!(empty, "output directory '{}' is not empty", path.display());
    }
    Ok(())
}

// Finds the session segment of every bar; fails if any bar is outside all segments
fn assign_sessions(
    bars: &[RawBar],
    schedule: &SessionSchedule,
    interval: u64,
) -> anyhow::Result<Vec<SessionRawBar>> {
    let mut outside = Vec::new();
    let mut assigned = Vec::with_capacity(bars.len());
    for bar in bars {
        let start = UnixNanos::from(bar.ts_start);
        let end = UnixNanos::from(bar.ts_start + interval);
        match schedule.locate(start, end) {
            Some(window) => assigned.push(SessionRawBar {
                bar: *bar,
                date: window.date,
                segment: window.segment,
            }),
            None => outside.push(start),
        }
    }
    if !outside.is_empty() {
        let examples: Vec<String> = outside
            .iter()
            .take(MAX_REPORTED)
            .map(UnixNanos::to_rfc3339)
            .collect();
        anyhow::bail!(
            "{} raw bars lie outside every session segment, starting at: {}",
            outside.len(),
            examples.join(", ")
        );
    }
    Ok(assigned)
}

fn write_split(
    cli: &Cli,
    split: &Split,
    selected: &[&SessionRawBar],
    bar_type: BarType,
    interval: u64,
) -> anyhow::Result<Value> {
    let precision = cli.price_precision;
    let bars = selected
        .iter()
        .map(|b| {
            let raw = b.bar;
            let ts = UnixNanos::from(raw.ts_start + interval);
            let price = |value: f64| -> anyhow::Result<Price> {
                let price = Price::new_checked(value, precision)?;
                anyhow::ensure!(
                    (price.as_f64() - value).abs() <= 1e-9 * value.abs().max(1.0),
                    "price {value} of the bar starting at {} has more than {precision} decimals",
                    UnixNanos::from(raw.ts_start).to_rfc3339()
                );
                Ok(price)
            };
            Bar::new_checked(
                bar_type,
                price(raw.open)?,
                price(raw.high)?,
                price(raw.low)?,
                price(raw.close)?,
                Quantity::new_checked(raw.volume as f64, 0)?,
                ts,
                ts,
            )
            .with_context(|| {
                format!(
                    "invalid bar starting at {}",
                    UnixNanos::from(raw.ts_start).to_rfc3339()
                )
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    // Defined as of the split's first bar
    let instrument = catalog::equity(cli.instrument_id, cli.currency, precision, bars[0].ts_init)?;
    let split_dir = cli.output.join(&split.name);
    let catalog_dir = split_dir.join("catalog");
    catalog::write_bars(&catalog_dir, &instrument, &bars)?;
    fs::copy(&cli.sessions, split_dir.join("sessions.parquet"))
        .with_context(|| format!("failed to copy sessions file to '{}'", split_dir.display()))?;

    let mut by_segment: BTreeMap<&str, usize> = BTreeMap::new();
    let mut sessions: Vec<Date> = Vec::new();
    for bar in selected {
        *by_segment.entry(bar.segment.as_str()).or_default() += 1;
        if sessions.last() != Some(&bar.date) {
            sessions.push(bar.date);
        }
    }
    Ok(json!({
        "name": split.name,
        "catalog": format!("{}/catalog", split.name),
        "sessions_file": format!("{}/sessions.parquet", split.name),
        "first_session": sessions.first().map(ToString::to_string),
        "last_session": sessions.last().map(ToString::to_string),
        "sessions": sessions.len(),
        "bars": bars.len(),
        "bars_by_segment": by_segment,
        "first_bar_close": bars.first().map(|b| b.ts_init.to_rfc3339()),
        "last_bar_close": bars.last().map(|b| b.ts_init.to_rfc3339()),
        "catalog_blake3": digest::tree(&catalog_dir)?,
    }))
}

// Path of `file` relative to `root`, with `/` separators
fn relative_path(root: &Path, file: &Path) -> String {
    let relative = file.strip_prefix(root).unwrap_or(file);
    let parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.is_empty() {
        file.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        parts.join("/")
    }
}
