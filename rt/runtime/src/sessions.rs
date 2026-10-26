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

//! Trading session schedule.
//!
//! A schedule lists, for every trading day, the UTC windows of its session segments. It is read
//! from a `sessions.parquet` file with one row per trading day and segment:
//!
//! | Column      | Type                  | Meaning                                          |
//! | ----------- | --------------------- | ------------------------------------------------ |
//! | `date`      | `date32`              | Trading day (exchange-local date)                |
//! | `segment`   | `string`              | `pre_market`, `regular`, or `after_hours`        |
//! | `open_utc`  | `timestamp[ns, UTC]`  | Segment start                                    |
//! | `close_utc` | `timestamp[ns, UTC]`  | Segment end                                      |
//!
//! The engine has no calendar logic of its own: holidays, early closes, and daylight saving time
//! are all resolved by whoever produced the file.

use std::{fmt::Display, fs::File, path::Path, str::FromStr, sync::Arc};

use anyhow::Context;
use arrow::{
    array::{Array, Date32Array, RecordBatch, StringArray, TimestampNanosecondArray},
    compute::cast,
    datatypes::{DataType, Field, Schema, TimeUnit},
};
use jiff::civil::Date;
use nautilus_core::UnixNanos;
use parquet::arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder};
use serde::{Deserialize, Serialize};

/// A session segment of a trading day.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Segment {
    /// Before the regular open.
    PreMarket,
    /// Regular trading hours.
    Regular,
    /// After the regular close.
    AfterHours,
}

impl Segment {
    /// All segments in time order.
    pub const ALL: [Self; 3] = [Self::PreMarket, Self::Regular, Self::AfterHours];

    /// Returns the segment's name as used in session files and configs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreMarket => "pre_market",
            Self::Regular => "regular",
            Self::AfterHours => "after_hours",
        }
    }
}

impl Display for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Segment {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|segment| segment.as_str() == s)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown session segment '{s}', expected pre_market, regular, or after_hours"
                )
            })
    }
}

/// One segment of one trading day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionWindow {
    /// Trading day the segment belongs to.
    pub date: Date,
    /// Which segment of the day.
    pub segment: Segment,
    /// Segment start (UTC).
    pub open: UnixNanos,
    /// Segment end (UTC).
    pub close: UnixNanos,
}

impl SessionWindow {
    /// Returns whether the interval `[start, end]` lies inside this window.
    #[must_use]
    pub fn contains(&self, start: UnixNanos, end: UnixNanos) -> bool {
        start >= self.open && end <= self.close
    }
}

/// Session windows for a range of trading days, sorted by time.
#[derive(Clone, Debug)]
pub struct SessionSchedule {
    windows: Vec<SessionWindow>,
}

impl SessionSchedule {
    /// Creates a schedule from `windows` in any order.
    ///
    /// # Errors
    ///
    /// Returns an error if a window is empty, windows overlap, or a trading day lists the same
    /// segment twice.
    pub fn new(mut windows: Vec<SessionWindow>) -> anyhow::Result<Self> {
        windows.sort_by_key(|w| (w.open, w.close));
        for window in &windows {
            anyhow::ensure!(
                window.open < window.close,
                "session window {} {} is empty or reversed",
                window.date,
                window.segment
            );
        }
        for pair in windows.windows(2) {
            anyhow::ensure!(
                pair[0].close <= pair[1].open,
                "session windows {} {} and {} {} overlap",
                pair[0].date,
                pair[0].segment,
                pair[1].date,
                pair[1].segment
            );
        }
        let mut keys: Vec<(Date, Segment)> = windows.iter().map(|w| (w.date, w.segment)).collect();
        keys.sort_unstable();
        if let Some(pair) = keys.windows(2).find(|pair| pair[0] == pair[1]) {
            anyhow::bail!("session {} lists segment {} twice", pair[0].0, pair[0].1);
        }
        Ok(Self { windows })
    }

    /// Reads a schedule from a `sessions.parquet` file (see the module docs for the schema).
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, a column is missing or has the wrong type,
    /// a value is null or invalid, or the windows are inconsistent.
    pub fn read_parquet(path: &Path) -> anyhow::Result<Self> {
        let read = || -> anyhow::Result<Self> {
            let file = File::open(path)?;
            let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
            let mut windows = Vec::new();
            for batch in reader {
                windows.extend(windows_from_batch(&batch?)?);
            }
            Self::new(windows)
        };
        read().with_context(|| format!("invalid sessions file '{}'", path.display()))
    }

    /// Writes the schedule to `path` in the `sessions.parquet` format.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn write_parquet(&self, path: &Path) -> anyhow::Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("date", DataType::Date32, false),
            Field::new("segment", DataType::Utf8, false),
            Field::new(
                "open_utc",
                DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
                false,
            ),
            Field::new(
                "close_utc",
                DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
                false,
            ),
        ]));
        let windows = &self.windows;
        let dates = windows
            .iter()
            .map(|w| date_to_days(w.date))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Date32Array::from(dates)),
                Arc::new(StringArray::from_iter_values(
                    windows.iter().map(|w| w.segment.as_str()),
                )),
                Arc::new(
                    TimestampNanosecondArray::from_iter_values(
                        windows.iter().map(|w| nanos_to_i64(w.open)),
                    )
                    .with_timezone("UTC"),
                ),
                Arc::new(
                    TimestampNanosecondArray::from_iter_values(
                        windows.iter().map(|w| nanos_to_i64(w.close)),
                    )
                    .with_timezone("UTC"),
                ),
            ],
        )?;
        let mut writer = ArrowWriter::try_new(File::create(path)?, schema, None)?;
        writer.write(&batch)?;
        writer.close()?;
        Ok(())
    }

    /// Returns all windows, sorted by time.
    #[must_use]
    pub fn windows(&self) -> &[SessionWindow] {
        &self.windows
    }

    /// Returns the window containing the interval `[start, end]`, if any.
    #[must_use]
    pub fn locate(&self, start: UnixNanos, end: UnixNanos) -> Option<&SessionWindow> {
        // Last window opening at or before `start`
        let index = self.windows.partition_point(|w| w.open <= start);
        let window = self.windows.get(index.checked_sub(1)?)?;
        window.contains(start, end).then_some(window)
    }

    /// Returns the first trading day after `date`, if the schedule has one.
    #[must_use]
    pub fn next_date_after(&self, date: Date) -> Option<Date> {
        let index = self.windows.partition_point(|w| w.date <= date);
        self.windows.get(index).map(|w| w.date)
    }
}

fn windows_from_batch(batch: &RecordBatch) -> anyhow::Result<Vec<SessionWindow>> {
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
    let check_timestamp = |name: &str| -> anyhow::Result<()> {
        let data_type = batch
            .column_by_name(name)
            .with_context(|| format!("missing column '{name}'"))?
            .data_type();
        anyhow::ensure!(
            matches!(data_type, DataType::Timestamp(_, Some(_))),
            "column '{name}' has type {data_type}, expected a timezone-aware timestamp"
        );
        Ok(())
    };
    check_timestamp("open_utc")?;
    check_timestamp("close_utc")?;

    let timestamp = DataType::Timestamp(TimeUnit::Nanosecond, None);
    let date = column("date", &DataType::Date32)?;
    let segment = column("segment", &DataType::Utf8)?;
    let open = column("open_utc", &timestamp)?;
    let close = column("close_utc", &timestamp)?;

    let date = date
        .as_any()
        .downcast_ref::<Date32Array>()
        .context("date column")?;
    let segment = segment
        .as_any()
        .downcast_ref::<StringArray>()
        .context("segment column")?;
    let open = open
        .as_any()
        .downcast_ref::<TimestampNanosecondArray>()
        .context("open_utc column")?;
    let close = close
        .as_any()
        .downcast_ref::<TimestampNanosecondArray>()
        .context("close_utc column")?;

    (0..batch.num_rows())
        .map(|row| {
            Ok(SessionWindow {
                date: days_to_date(date.value(row))?,
                segment: segment.value(row).parse()?,
                open: i64_to_nanos(open.value(row))?,
                close: i64_to_nanos(close.value(row))?,
            })
        })
        .collect()
}

const EPOCH: Date = Date::constant(1970, 1, 1);

/// Converts days since 1970-01-01 (Arrow `date32`) to a date.
///
/// # Errors
///
/// Returns an error if the date is out of range.
pub fn days_to_date(days: i32) -> anyhow::Result<Date> {
    Ok(EPOCH.checked_add(jiff::Span::new().try_days(days)?)?)
}

/// Converts a date to days since 1970-01-01 (Arrow `date32`).
///
/// # Errors
///
/// Returns an error if the date is out of range.
pub fn date_to_days(date: Date) -> anyhow::Result<i32> {
    Ok(EPOCH.until(date)?.get_days())
}

/// Returns midnight UTC at the start of `date`.
///
/// # Errors
///
/// Returns an error if the date is out of range.
pub fn date_to_utc_nanos(date: Date) -> anyhow::Result<UnixNanos> {
    let timestamp = date.to_zoned(jiff::tz::TimeZone::UTC)?.timestamp();
    i64_to_nanos(i64::try_from(timestamp.as_nanosecond())?)
}

fn i64_to_nanos(value: i64) -> anyhow::Result<UnixNanos> {
    Ok(UnixNanos::from(u64::try_from(value).with_context(
        || format!("timestamp {value} is before 1970"),
    )?))
}

fn nanos_to_i64(value: UnixNanos) -> i64 {
    i64::try_from(value.as_u64()).expect("timestamp fits in i64")
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tempfile::TempDir;

    use super::*;

    const HOUR: u64 = 3_600_000_000_000;

    fn window(day: i8, segment: Segment, open_hour: u64, close_hour: u64) -> SessionWindow {
        let date = Date::constant(2025, 1, day);
        let midnight = date_to_utc_nanos(date).unwrap().as_u64();
        SessionWindow {
            date,
            segment,
            open: (midnight + open_hour * HOUR).into(),
            close: (midnight + close_hour * HOUR).into(),
        }
    }

    fn schedule() -> SessionSchedule {
        SessionSchedule::new(vec![
            window(7, Segment::Regular, 14, 21),
            window(6, Segment::Regular, 14, 21),
            window(6, Segment::PreMarket, 9, 14),
        ])
        .unwrap()
    }

    #[rstest]
    #[case("pre_market", Segment::PreMarket)]
    #[case("regular", Segment::Regular)]
    #[case("after_hours", Segment::AfterHours)]
    fn segment_round_trips(#[case] name: &str, #[case] segment: Segment) {
        assert_eq!(name.parse::<Segment>().unwrap(), segment);
        assert_eq!(segment.to_string(), name);
    }

    #[rstest]
    fn segment_rejects_unknown_name() {
        assert!("lunch".parse::<Segment>().is_err());
    }

    #[rstest]
    fn new_sorts_windows() {
        let schedule = schedule();

        let dates: Vec<_> = schedule
            .windows()
            .iter()
            .map(|w| (w.date.day(), w.segment))
            .collect();
        assert_eq!(
            dates,
            vec![
                (6, Segment::PreMarket),
                (6, Segment::Regular),
                (7, Segment::Regular)
            ]
        );
    }

    #[rstest]
    fn new_rejects_overlap() {
        let result = SessionSchedule::new(vec![
            window(6, Segment::PreMarket, 9, 15),
            window(6, Segment::Regular, 14, 21),
        ]);

        assert!(result.unwrap_err().to_string().contains("overlap"));
    }

    #[rstest]
    fn new_rejects_duplicate_segment() {
        let result = SessionSchedule::new(vec![
            window(6, Segment::Regular, 9, 10),
            window(6, Segment::Regular, 14, 21),
        ]);

        assert!(result.unwrap_err().to_string().contains("twice"));
    }

    #[rstest]
    fn locate_finds_containing_window() {
        let schedule = schedule();
        let regular = schedule.windows()[1];
        let bar = nautilus_core::DurationNanos::new(5 * 60 * 1_000_000_000);

        assert_eq!(
            schedule.locate(regular.open, regular.open + bar),
            Some(&regular)
        );
        let last_start = UnixNanos::from(regular.close.as_u64() - bar.as_u64());
        assert_eq!(schedule.locate(last_start, regular.close), Some(&regular));
        assert_eq!(schedule.locate(regular.close, regular.close + bar), None);
        assert_eq!(
            schedule.locate(UnixNanos::from(1), UnixNanos::from(2)),
            None
        );
    }

    #[rstest]
    fn next_date_after_skips_to_next_day() {
        let schedule = schedule();

        assert_eq!(
            schedule.next_date_after(Date::constant(2025, 1, 6)),
            Some(Date::constant(2025, 1, 7))
        );
        assert_eq!(schedule.next_date_after(Date::constant(2025, 1, 7)), None);
    }

    #[rstest]
    fn parquet_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sessions.parquet");
        let schedule = schedule();

        schedule.write_parquet(&path).unwrap();
        let read = SessionSchedule::read_parquet(&path).unwrap();

        assert_eq!(read.windows(), schedule.windows());
    }

    #[rstest]
    fn read_parquet_reports_missing_file() {
        let err = SessionSchedule::read_parquet(Path::new("missing.parquet")).unwrap_err();

        assert!(err.to_string().contains("invalid sessions file"));
    }

    #[rstest]
    fn days_round_trip() {
        let date = Date::constant(2016, 10, 3);

        assert_eq!(days_to_date(date_to_days(date).unwrap()).unwrap(), date);
        assert_eq!(date_to_days(EPOCH).unwrap(), 0);
    }
}
