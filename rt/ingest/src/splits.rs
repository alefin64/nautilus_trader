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

//! Chronological data splits with an embargo between neighbors.

use std::str::FromStr;

use nautilus_rt::{
    anyhow::{self, Context},
    jiff::{Span, civil::Date},
};

/// A named, inclusive range of trading days, as given on the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SplitSpec {
    pub(crate) name: String,
    pub(crate) start: Date,
    pub(crate) end: Date,
}

impl FromStr for SplitSpec {
    type Err = anyhow::Error;

    // `name=YYYY-MM-DD..YYYY-MM-DD`
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parse = || -> anyhow::Result<Self> {
            let (name, range) = s.split_once('=').context("expected name=START..END")?;
            let (start, end) = range.split_once("..").context("expected START..END")?;
            anyhow::ensure!(
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "split name must be non-empty and use only letters, digits, '_' and '-'"
            );
            let split = Self {
                name: name.to_string(),
                start: start.parse()?,
                end: end.parse()?,
            };
            anyhow::ensure!(split.start <= split.end, "start is after end");
            Ok(split)
        };
        parse().with_context(|| format!("invalid split '{s}'"))
    }
}

/// A split after applying the embargo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Split {
    pub(crate) name: String,
    pub(crate) start: Date,
    pub(crate) end: Date,
    /// Last trading day kept: `end`, or earlier when the embargo before the next split cuts in.
    pub(crate) effective_end: Date,
}

impl Split {
    /// Returns whether trading day `date` belongs to the split.
    pub(crate) fn contains(&self, date: Date) -> bool {
        date >= self.start && date <= self.effective_end
    }
}

/// Checks the order of the splits and applies the embargo.
///
/// Splits must be in chronological order and must not overlap. The `embargo_days` calendar
/// days before each split's start are removed from the end of the split before it, so at least
/// that many days separate neighboring splits (the convention of a time-series split "gap").
pub(crate) fn apply_embargo(specs: &[SplitSpec], embargo_days: u32) -> anyhow::Result<Vec<Split>> {
    anyhow::ensure!(!specs.is_empty(), "no splits given");
    for (index, spec) in specs.iter().enumerate() {
        anyhow::ensure!(
            specs[..index].iter().all(|other| other.name != spec.name),
            "split '{}' is given twice",
            spec.name
        );
    }
    let embargo = Span::new().try_days(i64::from(embargo_days) + 1)?;

    specs
        .iter()
        .enumerate()
        .map(|(index, spec)| {
            let effective_end = match specs.get(index + 1) {
                Some(next) => {
                    anyhow::ensure!(
                        spec.end < next.start,
                        "split '{}' must end before split '{}' starts",
                        spec.name,
                        next.name
                    );
                    spec.end.min(next.start.checked_sub(embargo)?)
                }
                None => spec.end,
            };
            anyhow::ensure!(
                effective_end >= spec.start,
                "the embargo before split '{}' leaves no days in split '{}'",
                specs[index + 1].name,
                spec.name
            );
            Ok(Split {
                name: spec.name.clone(),
                start: spec.start,
                end: spec.end,
                effective_end,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn spec(s: &str) -> SplitSpec {
        s.parse().unwrap()
    }

    #[rstest]
    fn parses_split() {
        assert_eq!(
            spec("train=2016-10-03..2022-12-31"),
            SplitSpec {
                name: "train".to_string(),
                start: Date::constant(2016, 10, 3),
                end: Date::constant(2022, 12, 31),
            }
        );
    }

    #[rstest]
    #[case("train")]
    #[case("train=2016-10-03")]
    #[case("=2016-10-03..2016-12-31")]
    #[case("a b=2016-10-03..2016-12-31")]
    #[case("train=2016-12-31..2016-10-03")]
    #[case("train=2016-13-01..2016-12-31")]
    fn rejects_bad_split(#[case] text: &str) {
        assert!(text.parse::<SplitSpec>().is_err());
    }

    #[rstest]
    fn embargo_trims_end_of_earlier_split() {
        let splits = apply_embargo(
            &[
                spec("train=2016-10-03..2022-12-31"),
                spec("validation=2023-01-01..2024-12-31"),
                spec("holdout=2025-01-01..2026-09-30"),
            ],
            7,
        )
        .unwrap();

        let ends: Vec<_> = splits.iter().map(|s| s.effective_end.to_string()).collect();
        assert_eq!(ends, ["2022-12-24", "2024-12-24", "2026-09-30"]);
        assert!(splits[0].contains(Date::constant(2022, 12, 24)));
        assert!(!splits[0].contains(Date::constant(2022, 12, 27)));
    }

    #[rstest]
    fn existing_gap_counts_toward_embargo() {
        let splits = apply_embargo(
            &[
                spec("a=2020-01-01..2020-06-30"),
                spec("b=2020-08-01..2020-12-31"),
            ],
            7,
        )
        .unwrap();

        assert_eq!(splits[0].effective_end, Date::constant(2020, 6, 30));
    }

    #[rstest]
    #[case::overlap(&["a=2020-01-01..2020-06-30", "b=2020-06-01..2020-12-31"], 0, "must end before")]
    #[case::order(&["b=2020-08-01..2020-12-31", "a=2020-01-01..2020-06-30"], 0, "must end before")]
    #[case::duplicate(&["a=2020-01-01..2020-06-30", "a=2020-08-01..2020-12-31"], 0, "twice")]
    #[case::swallowed(&["a=2020-01-01..2020-01-05", "b=2020-01-06..2020-12-31"], 7, "leaves no days")]
    fn rejects_bad_splits(#[case] specs: &[&str], #[case] embargo: u32, #[case] message: &str) {
        let specs: Vec<_> = specs.iter().map(|s| spec(s)).collect();

        let err = apply_embargo(&specs, embargo).unwrap_err();

        assert!(err.to_string().contains(message), "{err}");
    }
}
