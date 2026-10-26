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

//! End-to-end tests: run the `nautilus-ingest` binary on raw files generated from the
//! synthetic equity scenario.

use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::Arc,
};

use nautilus_rt::{
    arrow::{
        array::{Float64Array, Int64Array, RecordBatch, StringArray, TimestampNanosecondArray},
        datatypes::{DataType, Field, Schema, TimeUnit},
    },
    catalog,
    model::{data::BarType, identifiers::InstrumentId, instruments::Instrument},
    parquet::arrow::ArrowWriter,
    serde_json::{self, Value},
    synthetic::{BARS_PER_SESSION, EquityScenario},
};
use rstest::rstest;
use tempfile::TempDir;

const INGEST: &str = env!("CARGO_BIN_EXE_nautilus-ingest");
const BAR_NS: i64 = 5 * 60 * 1_000_000_000;
const HOUR_NS: i64 = 3_600_000_000_000;
/// Pre-market bars per day in the raw fixture.
const PRE_MARKET_BARS: usize = 2;

struct RawRow {
    ts_start: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: i64,
}

struct Fixture {
    dir: TempDir,
    scenario: EquityScenario,
}

impl Fixture {
    /// Raw files with the scenario's regular bars plus a few pre-market bars per day, one file
    /// per week, and a matching sessions file.
    fn new(sessions: usize, extra: &[RawRow]) -> Self {
        let dir = TempDir::new().unwrap();
        let scenario = EquityScenario::new(sessions).unwrap();

        let mut rows: Vec<RawRow> = Vec::new();
        for (index, bar) in scenario.bars.iter().enumerate() {
            let ts_start = bar.ts_init.as_u64() as i64 - BAR_NS;
            if index % BARS_PER_SESSION == 0 {
                // Pre-market bars at 13:00 and 13:05 UTC, inside 09:00-14:30
                for n in 0..PRE_MARKET_BARS {
                    rows.push(RawRow {
                        ts_start: ts_start - 90 * 60 * 1_000_000_000 + n as i64 * BAR_NS,
                        open: 49.5,
                        high: 49.75,
                        low: 49.25,
                        close: 49.5,
                        volume: 100,
                    });
                }
            }
            rows.push(RawRow {
                ts_start,
                open: bar.open.as_f64(),
                high: bar.high.as_f64(),
                low: bar.low.as_f64(),
                close: bar.close.as_f64(),
                volume: bar.volume.as_f64() as i64,
            });
        }
        rows.extend(extra.iter().map(|r| RawRow { ..*r }));
        rows.sort_by_key(|r| r.ts_start);

        let raw = dir.path().join("raw");
        let week_ns = 7 * 24 * HOUR_NS;
        let first = rows[0].ts_start;
        let mut weeks: Vec<Vec<&RawRow>> = Vec::new();
        for row in &rows {
            let week = ((row.ts_start - first) / week_ns) as usize;
            weeks.resize_with(weeks.len().max(week + 1), Vec::new);
            weeks[week].push(row);
        }
        for (week, rows) in weeks.iter().enumerate() {
            let path = raw.join(format!("week={week}")).join("bars.parquet");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_raw(&path, rows);
        }
        fs::write(raw.join("manifest.json"), "{}").unwrap();
        scenario
            .schedule
            .write_parquet(&dir.path().join("sessions.parquet"))
            .unwrap();
        Self { dir, scenario }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn run(&self, output: &str, args: &[&str]) -> (Output, String) {
        let mut command = Command::new(INGEST);
        command
            .arg("--raw")
            .arg(self.path("raw"))
            .arg("--sessions")
            .arg(self.path("sessions.parquet"))
            .arg("--output")
            .arg(self.path(output));
        let defaults = [
            ("--profile", "rth"),
            ("--calendar", "XNYS"),
            ("--segments", "regular"),
            ("--instrument-id", "SYNTH.XNYS"),
            ("--currency", "USD"),
            ("--price-precision", "4"),
            ("--bar-minutes", "5"),
            ("--embargo-days", "2"),
        ];
        for (flag, value) in defaults {
            if !args.contains(&flag) {
                command.args([flag, value]);
            }
        }
        if !args.contains(&"--split") {
            command.args([
                "--split",
                "train=2025-01-06..2025-01-10",
                "--split",
                "validation=2025-01-11..2025-01-17",
            ]);
        }
        let output = command.args(args).output().expect("run nautilus-ingest");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (output, text)
    }

    fn run_ok(&self, output: &str, args: &[&str]) -> PathBuf {
        let (result, text) = self.run(output, args);
        assert!(result.status.success(), "{text}");
        self.path(output)
    }
}

fn write_raw(path: &Path, rows: &[&RawRow]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new(
            "ts_start",
            DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
            false,
        ),
        Field::new("open", DataType::Float64, false),
        Field::new("high", DataType::Float64, false),
        Field::new("low", DataType::Float64, false),
        Field::new("close", DataType::Float64, false),
        Field::new("volume", DataType::Int64, false),
        Field::new("trade_count", DataType::Int64, false),
        Field::new("segment", DataType::Utf8, false),
    ]));
    let floats =
        |f: fn(&RawRow) -> f64| Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| f(r))));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(
                TimestampNanosecondArray::from_iter_values(rows.iter().map(|r| r.ts_start))
                    .with_timezone("UTC"),
            ),
            floats(|r| r.open),
            floats(|r| r.high),
            floats(|r| r.low),
            floats(|r| r.close),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.volume))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|_| 1))),
            // Deliberately wrong: ingest must not trust this column
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|_| "after_hours"),
            )),
        ],
    )
    .unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

fn manifest(out: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(out.join("ingest-manifest.json")).unwrap()).unwrap()
}

#[rstest]
fn writes_catalog_per_split_with_embargo() {
    let fixture = Fixture::new(10, &[]);

    let out = fixture.run_ok("out", &[]);

    // Train loses 2025-01-09 and 2025-01-10 to the 2-day embargo before 2025-01-11
    let bar_type = BarType::from("SYNTH.XNYS-5-MINUTE-LAST-EXTERNAL");
    let train = catalog::read_bars(&out.join("train/catalog"), bar_type, None, None).unwrap();
    let validation =
        catalog::read_bars(&out.join("validation/catalog"), bar_type, None, None).unwrap();
    assert_eq!(train.len(), 3 * BARS_PER_SESSION);
    assert_eq!(validation.len(), 5 * BARS_PER_SESSION);
    // Same bars as the scenario, stamped at their close
    assert_eq!(train, fixture.scenario.bars[..3 * BARS_PER_SESSION]);
    assert_eq!(validation, fixture.scenario.bars[5 * BARS_PER_SESSION..]);

    let instrument =
        catalog::read_instrument(&out.join("train/catalog"), InstrumentId::from("SYNTH.XNYS"))
            .unwrap();
    assert_eq!(instrument.price_precision(), 4);
    assert_eq!(
        fs::read(out.join("train/sessions.parquet")).unwrap(),
        fs::read(fixture.path("sessions.parquet")).unwrap()
    );

    let manifest = manifest(&out);
    assert_eq!(manifest["format"], "nautilus-ingest-manifest/1");
    assert_eq!(
        manifest["profile"]["segments"],
        serde_json::json!(["regular"])
    );
    assert_eq!(manifest["splits"][0]["effective_end"], "2025-01-08");
    assert_eq!(manifest["splits"][1]["effective_end"], "2025-01-17");
    assert_eq!(manifest["split_outputs"][0]["sessions"], 3);
    assert_eq!(manifest["split_outputs"][1]["bars"], 5 * BARS_PER_SESSION);
    assert_eq!(
        manifest["row_counts"]["raw"],
        10 * (BARS_PER_SESSION + PRE_MARKET_BARS)
    );
    assert_eq!(
        manifest["row_counts"]["raw_by_segment"]["pre_market"],
        10 * PRE_MARKET_BARS
    );
    assert_eq!(
        manifest["inputs"]["raw_files"][0]["path"],
        "week=0/bars.parquet"
    );
    assert_eq!(manifest["data_profile_hash"].as_str().unwrap().len(), 64);
}

#[rstest]
fn extended_profile_keeps_pre_market_bars() {
    let fixture = Fixture::new(3, &[]);

    let out = fixture.run_ok(
        "out",
        &[
            "--profile",
            "extended",
            "--segments",
            "pre_market,regular",
            "--split",
            "all=2025-01-06..2025-01-08",
        ],
    );

    let manifest = manifest(&out);
    assert_eq!(
        manifest["split_outputs"][0]["bars_by_segment"]["pre_market"],
        3 * PRE_MARKET_BARS
    );
    assert_eq!(
        manifest["split_outputs"][0]["bars"],
        3 * (BARS_PER_SESSION + PRE_MARKET_BARS)
    );
}

#[rstest]
fn profile_hash_depends_on_segments() {
    let fixture = Fixture::new(3, &[]);
    let split = ["--split", "all=2025-01-06..2025-01-08"];

    let rth = fixture.run_ok("rth", &split);
    let extended = fixture.run_ok(
        "extended",
        &["--segments", "pre_market,regular", split[0], split[1]],
    );

    assert_ne!(
        manifest(&rth)["data_profile_hash"],
        manifest(&extended)["data_profile_hash"]
    );
}

#[rstest]
fn outputs_are_deterministic() {
    let fixture = Fixture::new(10, &[]);

    let first = fixture.run_ok("first", &[]);
    let second = fixture.run_ok("second", &[]);

    assert_eq!(
        fs::read(first.join("ingest-manifest.json")).unwrap(),
        fs::read(second.join("ingest-manifest.json")).unwrap()
    );
}

#[rstest]
fn fails_when_a_bar_is_outside_every_session() {
    // Saturday 2025-01-11 15:00 UTC
    let saturday = RawRow {
        ts_start: 1_736_607_600_000_000_000,
        open: 50.0,
        high: 50.0,
        low: 50.0,
        close: 50.0,
        volume: 1,
    };
    let fixture = Fixture::new(10, &[saturday]);

    let (output, text) = fixture.run("out", &[]);

    assert!(!output.status.success());
    assert!(
        text.contains(
            "1 raw bars lie outside every session segment, starting at: 2025-01-11T15:00:00"
        ),
        "{text}"
    );
}

#[rstest]
#[case::missing_segment(&["--segments", "regular,after_hours"], "profile segment after_hours has no bars")]
#[case::too_precise(&["--price-precision", "1"], "has more than 1 decimals")]
#[case::empty_split(
    &["--split", "train=2025-01-06..2025-01-10", "--split", "later=2025-02-01..2025-02-28"],
    "split 'later' has no bars"
)]
#[case::overlapping_splits(
    &["--split", "a=2025-01-06..2025-01-10", "--split", "b=2025-01-08..2025-01-17"],
    "must end before split 'b' starts"
)]
#[case::bad_segment(&["--segments", "lunch"], "unknown session segment 'lunch'")]
fn rejects_invalid_input(#[case] args: &[&str], #[case] message: &str) {
    let fixture = Fixture::new(10, &[]);

    let (output, text) = fixture.run("out", args);

    assert!(!output.status.success(), "{text}");
    assert!(text.contains(message), "{text}");
}

#[rstest]
fn refuses_non_empty_output() {
    let fixture = Fixture::new(10, &[]);
    fs::create_dir_all(fixture.path("out")).unwrap();
    fs::write(fixture.path("out/keep.txt"), "x").unwrap();

    let (output, text) = fixture.run("out", &[]);

    assert!(!output.status.success());
    assert!(text.contains("is not empty"), "{text}");
}
