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

//! Reads raw vendor-style bar files.
//!
//! Each file is Parquet with one row per bar:
//!
//! | Column                         | Type                  | Meaning                     |
//! | ------------------------------ | --------------------- | --------------------------- |
//! | `ts_start`                     | `timestamp[ns, UTC]`  | Bar start                   |
//! | `open`, `high`, `low`, `close` | `float64`             | Prices                      |
//! | `volume`                       | `int64`               | Shares traded               |
//!
//! Other columns (such as `trade_count` and `segment`) are ignored.

use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::Arc,
};

use nautilus_rt::{
    anyhow::{self, Context},
    arrow::{
        array::{Array, Float64Array, Int64Array, RecordBatch, TimestampNanosecondArray},
        compute::cast,
        datatypes::{DataType, TimeUnit},
    },
    parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder,
};

/// One raw bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RawBar {
    /// Bar start, UTC nanoseconds.
    pub(crate) ts_start: u64,
    pub(crate) open: f64,
    pub(crate) high: f64,
    pub(crate) low: f64,
    pub(crate) close: f64,
    pub(crate) volume: u64,
}

/// Lists the Parquet files under `path` (or `path` itself if it is a file), sorted by path.
pub(crate) fn list_files(path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    anyhow::ensure!(
        path.is_dir(),
        "raw data path '{}' not found",
        path.display()
    );
    let mut files = Vec::new();
    collect(path, &mut files).with_context(|| format!("failed to list '{}'", path.display()))?;
    files.sort();
    anyhow::ensure!(
        !files.is_empty(),
        "no Parquet files under '{}'",
        path.display()
    );
    Ok(files)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "parquet") {
            out.push(path);
        }
    }
    Ok(())
}

/// Reads all bars of one raw file.
pub(crate) fn read_file(path: &Path) -> anyhow::Result<Vec<RawBar>> {
    let read = || -> anyhow::Result<Vec<RawBar>> {
        let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?.build()?;
        let mut bars = Vec::new();
        for batch in reader {
            bars.extend(read_batch(&batch?)?);
        }
        Ok(bars)
    };
    read().with_context(|| format!("invalid raw bar file '{}'", path.display()))
}

fn read_batch(batch: &RecordBatch) -> anyhow::Result<Vec<RawBar>> {
    let column = |name: &str, data_type: &DataType| -> anyhow::Result<Arc<dyn Array>> {
        let array = batch
            .column_by_name(name)
            .with_context(|| format!("missing column '{name}'"))?;
        anyhow::ensure!(
            array.null_count() == 0,
            "column '{name}' contains null values"
        );
        cast(array, data_type).with_context(|| {
            format!(
                "column '{name}' has type {}, expected {data_type}",
                array.data_type()
            )
        })
    };
    let ts_type = batch
        .column_by_name("ts_start")
        .context("missing column 'ts_start'")?
        .data_type()
        .clone();
    anyhow::ensure!(
        matches!(ts_type, DataType::Timestamp(_, Some(_))),
        "column 'ts_start' has type {ts_type}, expected a timezone-aware timestamp"
    );

    let ts = column("ts_start", &DataType::Timestamp(TimeUnit::Nanosecond, None))?;
    let ts = ts
        .as_any()
        .downcast_ref::<TimestampNanosecondArray>()
        .context("ts_start column")?;
    let prices = ["open", "high", "low", "close"].map(|name| column(name, &DataType::Float64));
    let [open, high, low, close] = prices;
    let (open, high, low, close) = (open?, high?, low?, close?);
    let price = |array: &Arc<dyn Array>| -> anyhow::Result<Float64Array> {
        Ok(array
            .as_any()
            .downcast_ref::<Float64Array>()
            .context("price column")?
            .clone())
    };
    let (open, high, low, close) = (price(&open)?, price(&high)?, price(&low)?, price(&close)?);
    let volume = column("volume", &DataType::Int64)?;
    let volume = volume
        .as_any()
        .downcast_ref::<Int64Array>()
        .context("volume column")?;

    (0..batch.num_rows())
        .map(|row| {
            Ok(RawBar {
                ts_start: u64::try_from(ts.value(row))
                    .with_context(|| format!("ts_start {} is before 1970", ts.value(row)))?,
                open: open.value(row),
                high: high.value(row),
                low: low.value(row),
                close: close.value(row),
                volume: u64::try_from(volume.value(row))
                    .with_context(|| format!("negative volume {}", volume.value(row)))?,
            })
        })
        .collect()
}
