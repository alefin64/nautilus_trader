# Bullpen handover: compiled strategy plug-in backtests on bars

Branch `rust-plugin-backtest`, based on release tag `v2.0.0rc7` (Rust crates 0.66.0).

## Status

Strategies compiled as Rust plug-ins (`.so` files) are backtested on 5-minute bars read from a
Nautilus Parquet data catalog. There is no Python anywhere in this path.

- **End to end.** `nautilus-ingest` turns raw bar files and a session schedule into one catalog
  per data split. `nautilus-backtest` runs the strategy slot plug-in on a split and writes
  `results.parquet`, `trades.parquet`, `returns.parquet`, and `run.json`. Verified on the
  10-year synthetic dataset from `bullpen-synth-data`: train and validation splits of the `rth`
  profile (see [Checks done by hand](#checks-done-by-hand)).
- **Fill model.** Every order fills at the open of the next bar after it is sent, plus
  per-segment slippage, never on its own bar. Commission per share with a per-order minimum,
  borrow on shorts, and position limits come from the config, as do the session rules
  (`overnight = "hold"` or `"flatten"`). Nothing is hard-coded.
- **Determinism.** The same inputs give byte-identical output files, covered by a test.
- **Safety.** A mismatched or stale plug-in or binary is refused at startup with a clear error,
  rather than running with undefined behavior.
- **Packaging.** `rt/scripts/bundle.bash` produces a self-contained directory (runner, ingest
  tool, `libnautilus_rt.so`, `libstd-*.so`) that runs without `LD_LIBRARY_PATH`.
- **Speed.** The template strategy runs 6.25 years of regular-hours bars (121,914 bars) in
  0.5 s in release. A strategy-only change rebuilds in seconds.

## Where everything is

All plug-in work lives in its own cargo workspace, `rt/`. The root workspace is untouched, so
merges from upstream stay conflict-free.

| Path                                | What it is                                                                         |
| ----------------------------------- | ---------------------------------------------------------------------------------- |
| `rt/Cargo.toml`                     | Workspace manifest: members, shared dependencies, profiles, lints, patches         |
| `rt/.cargo/config.toml`             | Adds `-C prefer-dynamic` and `-Wl,-rpath,$ORIGIN`; see [Rules](#rules-and-gotchas) |
| `rt/Cargo.lock`                     | Lockfile, seeded from the root `Cargo.lock` (same dependency versions)             |
| `rt/runtime/`                       | `nautilus-rt`: the engine as one shared library, plus the plug-in SDK              |
| `rt/runtime/src/lib.rs`             | Re-exports, build fingerprint, compiler version                                    |
| `rt/runtime/src/sdk.rs`             | Plug-in side: the `strategy_plugin!` macro and `StrategyRegistrar`                 |
| `rt/runtime/src/loader.rs`          | Host side: `PluginLoader` and `StrategyPluginConfig`                               |
| `rt/runtime/src/sim/`               | `BarSimulation`: fill model, costs, account and session rules, reports             |
| `rt/runtime/src/sessions.rs`        | `SessionSchedule`: reads `sessions.parquet`                                        |
| `rt/runtime/src/catalog.rs`         | Reads and writes instruments and bars in a Parquet catalog                         |
| `rt/runtime/src/digest.rs`          | BLAKE3 digests of files and directory trees                                        |
| `rt/runtime/src/synthetic.rs`       | Test scenarios: synthetic FX quotes, and the `SYNTH.XNYS` equity bar scenario      |
| `rt/runtime/build.rs`               | Build-config guards and fingerprint computation                                    |
| `rt/backtest_runner/`               | `nautilus-backtest`: reads a TOML config, loads plug-ins, writes the outputs       |
| `rt/ingest/`                        | `nautilus-ingest`: raw bars and sessions to per-split catalogs                     |
| `rt/plugins/strategy/`              | The strategy slot (frozen `Cargo.toml`) with the template strategy                 |
| `rt/plugins/strategy-template/src/` | Pristine copy of the slot's `src/`, for seeding new strategies                     |
| `rt/plugins/ema_cross/`             | Quote-based example plug-in, kept for parity with upstream                         |
| `rt/scripts/bundle.bash`            | Builds in release and assembles the self-contained bundle                          |
| `rt/README.md`                      | Short reference for the workspace                                                  |

Commits on the branch, oldest first:

| Commit       | Summary                                                                  |
| ------------ | ------------------------------------------------------------------------ |
| `c9cb100444` | Add shared runtime dylib and strategy plug-in prototype                  |
| `b857de7ce3` | Move strategy plug-in prototype into its own rt workspace                |
| `488500baf5` | Make simulated venue OMS type configurable in backtest runner            |
| `c8a6d84548` | Add tests and shared synthetic scenario to rt workspace                  |
| `87cd6f2c5b` | Add handover guide for compiled strategy plug-in backtests               |
| `7df67f786a` | Add bar simulation, sessions and Parquet catalog to rt runtime           |
| `380084cca9` | Add frozen strategy slot crate with bar strategy template                |
| `8cfac11c55` | Replace synthetic scenario in backtest runner with catalog bar backtests |
| `a17940e599` | Add nautilus-ingest tool for per-split Parquet catalogs                  |
| `ef9fecb945` | Package rt binaries with an $ORIGIN rpath and add build info flag        |
| (this one)   | Update handover and rt README for bar backtests                          |

## Setup

You need:

- The Rust toolchain pinned by `rust-toolchain.toml` (1.99.0), installed with rustup. Rustup
  picks it up automatically inside the repository.
- `clang` and `lld`. The repository `.cargo/config.toml` links with `-fuse-ld=lld`.
- `cargo +nightly-2026-08-21 fmt` for formatting, the nightly pinned in `tools.toml`.

On Debian or Ubuntu:

```bash
sudo apt-get install build-essential clang lld pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
rustup toolchain install nightly-2026-08-21 --profile minimal --component rustfmt
rustup component add clippy   # run inside the repo so it targets 1.99.0
```

A full build compiles the Parquet and DataFusion stack that `nautilus-persistence` needs: about
8 minutes in debug on the 4-core development VM, and the `target/` directory grows to several
gigabytes per profile.

## End to end

**Always run cargo from `rt/`.** Cargo reads `rt/.cargo/config.toml` based on the current
directory, and the runtime refuses to build without it.

### 1. Sessions file

The engine has no calendar logic. A `sessions.parquet` file lists, for every trading day and
segment, the segment's UTC window:

| Column      | Type                 | Meaning                                   |
| ----------- | -------------------- | ----------------------------------------- |
| `date`      | `date32`             | Trading day (exchange-local date)         |
| `segment`   | `string`             | `pre_market`, `regular`, or `after_hours` |
| `open_utc`  | `timestamp[ns, UTC]` | Segment start                             |
| `close_utc` | `timestamp[ns, UTC]` | Segment end                               |

The orchestrator's Python tooling produces it. Holidays, early closes, and daylight saving time
are resolved there. For the checks below it was generated with `exchange_calendars` (XNYS) from
a script outside the repository: pre-market 04:00 to the open, regular open to close, after-hours
close to close + 4 h, all America/New_York converted to UTC per day (the same segments as
`bullpen-synth-data`).

### 2. Ingest

```bash
cd rt
cargo build --release
cargo run --release --bin nautilus-ingest -- \
  --raw ../../bullpen-synth-data/out/SYNTH \
  --sessions /path/to/sessions.parquet \
  --profile rth --calendar XNYS --segments regular \
  --instrument-id SYNTH.XNYS --currency USD --price-precision 4 --bar-minutes 5 \
  --split train=2016-10-03..2022-12-31 \
  --split validation=2023-01-01..2024-12-31 \
  --split holdout=2025-01-01..2026-09-30 \
  --embargo-days 7 \
  --output /path/to/data/rth
```

| Flag                | Meaning                                                                       |
| ------------------- | ----------------------------------------------------------------------------- |
| `--raw`             | A raw Parquet file, or a directory searched recursively for `*.parquet`       |
| `--sessions`        | The sessions file. It must cover every raw bar                                |
| `--profile`         | Profile name, recorded                                                        |
| `--calendar`        | Calendar the sessions file came from, recorded                                |
| `--segments`        | The profile's segments, comma separated                                       |
| `--instrument-id`   | Instrument of the bars. Written as a whole-share `Equity`                     |
| `--currency`        | Quote currency                                                                |
| `--price-precision` | Price decimals of the instrument and bars. Raw prices must not have more      |
| `--bar-minutes`     | Bar length. The bar type is `<instrument>-<minutes>-MINUTE-LAST-EXTERNAL`     |
| `--split`           | `name=START..END` in trading days, inclusive, repeated in chronological order |
| `--embargo-days`    | Calendar days removed from the end of a split before the next one starts      |
| `--output`          | Output directory. It must not exist or be empty                               |

Raw files hold one row per bar: `ts_start` (`timestamp[ns, UTC]`, bar start), `open`, `high`,
`low`, `close` (`float64`), and `volume` (`int64`). Other columns, such as `trade_count` and the
vendor's `segment`, are ignored: segments are recomputed from the sessions file.

What it does:

1. Reads every raw bar and finds its session segment. It **fails** if any bar lies outside
   every segment, if two bars start at the same time, if a profile segment has no raw bars, or
   if a price has more decimals than `--price-precision`.
2. Applies the embargo: the `embargo-days` calendar days before each split's start are removed
   from the end of the previous split (the convention of a time-series split "gap"). With the
   splits above, train ends 2022-12-24 and validation 2024-12-24. A split is assigned by trading
   day, so after-hours bars of 31 December stay with 31 December.
3. Writes, per split, `<output>/<split>/catalog/` (the instrument and the bars in the profile's
   segments, stamped at their close: `ts_event = ts_init = ts_start + bar length`) and copies
   the sessions file to `<output>/<split>/sessions.parquet`.
4. Writes `<output>/ingest-manifest.json`: raw file digests and row counts, the sessions file
   digest, instrument, profile, embargo, splits (with effective ends), row counts per segment,
   per-split session and bar counts, first and last bar, a digest of each catalog, and
   `data_profile_hash`. That hash covers the inputs and every parameter, so it identifies the
   derived dataset. All digests are BLAKE3.

The outputs are deterministic.

### 3. Backtest

```bash
cd rt
cargo run --release --bin nautilus-backtest -- --config train.toml --output out/train
```

The run prints one summary line. `--log-level info` shows the engine's full log and
performance report (default `warn`). `nautilus-backtest --build-info` prints the engine version,
build fingerprint, and compiler as JSON.

### Running the binaries outside cargo

Use the bundle:

```bash
rt/scripts/bundle.bash            # writes rt/target/bundle/
rt/target/bundle/nautilus-backtest --config train.toml --output out/train
```

Every binary and library is linked with `RUNPATH $ORIGIN`, so the bundle finds
`libnautilus_rt.so` and `libstd-*.so` in its own directory. Binaries run straight from
`target/release/` find the runtime but not `libstd`; set
`LD_LIBRARY_PATH="$(rustc --print target-libdir)"` from `rt/` if you do that.

Plug-ins must come from the same build as the bundle: `rt/target/release/libstrategy_plugin.so`
after `bundle.sh`, or a later `cargo build --release` with nothing changed in between.

## Backtest config reference

The runner config is TOML, parsed into `RunnerConfig` in `rt/backtest_runner/src/config.rs`.
Every key is required and unknown keys are errors. Relative paths resolve from the process
working directory, not the config file.

```toml
seed = 0                                      # recorded; passed to the engine's fill model

[data]
catalog = "data/rth/train/catalog"            # Nautilus Parquet catalog
sessions = "data/rth/train/sessions.parquet"  # session schedule
instrument_id = "SYNTH.XNYS"
bar_type = "SYNTH.XNYS-5-MINUTE-LAST-EXTERNAL"
start = 2016-10-03                            # first trading day, inclusive
end = 2022-12-31                              # last trading day, inclusive

[session]
profile = "rth"                               # recorded
overnight = "hold"                            # "hold" or "flatten"

[venue]
oms_type = "NETTING"                          # only NETTING
account_type = "MARGIN"                       # MARGIN or CASH
base_currency = "USD"                         # must be the instrument's quote currency
starting_capital = 100000
max_leverage = 1.0                            # gross exposure / equity
max_position_pct_equity = 100
allow_short = true
short_borrow_bps_per_year = 50

[fill_model]
type = "next_bar_open"                        # the only fill model

[fill_model.slippage_bps]                     # by the fill bar's session segment
regular = 2
pre_market = 10
after_hours = 10

[fill_model.commission]
per_share = 0.005
min_per_order = 1.0

[[strategies]]                                # one entry per strategy instance
path = "target/release/libstrategy_plugin.so"
type_name = "Strategy"

[strategies.config]                           # passed to the plug-in's factory as JSON
instrument_id = "SYNTH.XNYS"
bar_type = "SYNTH.XNYS-5-MINUTE-LAST-EXTERNAL"
fast_period = 20
slow_period = 80
trade_size = "500"
go_short = true
```

The `[venue]`, `[fill_model]`, and `[session]` values map one to one onto
`bullpen-platform/config/backtest.yaml` (`account`, `fill_model`) and `sessions.yaml`
(`overnight`). Two extra keys have no counterpart there: `oms_type` and `account_type`. Dates
may also be strings (`"2016-10-03"`). Decimal values are exact: write `0.005` or `"0.005"`.

Validation rejects: anything but `NETTING`; account types other than `MARGIN` and `CASH`;
non-positive capital, leverage, or position percentage; negative borrow, slippage, or
commission; slippage of 10,000 bps or more; and a `CASH` account with `allow_short = true` or
`max_leverage > 1`.

Each `[[strategies]]` entry is a `StrategyPluginConfig` (`rt/runtime/src/loader.rs`):

| Key         | Type   | Meaning                                                                         |
| ----------- | ------ | ------------------------------------------------------------------------------- |
| `path`      | string | Plug-in library path                                                            |
| `type_name` | string | Strategy type exported by the plug-in. The slot exports `"Strategy"`            |
| `config`    | table  | Instance config, passed to the factory as JSON. Its shape is up to the strategy |

Several instances can run in one backtest, each with a unique `strategy_id` and `order_id_tag`
in `[strategies.config.base]`. Account rules apply to the net position of all of them.

## Simulation model

`BarSimulation` (`rt/runtime/src/sim/mod.rs`) wraps the engine's `BacktestEngine` with one
simulated venue for the instrument's venue (`XNYS`). The strategy and its tests use exactly the
same code as the runner.

### Fills at the next bar's open

The venue never sees bars (`bar_execution = false`). Instead, one nanosecond after each bar
closes, the data feed carries a **fill quote** built from the *next* bar's open: the ask is the
open plus the next bar's segment slippage, rounded up to the price increment; the bid is the
open minus slippage, rounded down. Quote sizes are large enough never to limit a fill.

Orders reach the venue with a latency of one nanosecond (`StaticLatencyModel`: insert and
modify 1 ns, cancel 0 ns). So an order sent while handling the bar that closed at `t` arrives at
`t + 1 ns`, after the fill quote for the next bar's open, and fills against it: buys at the
open plus slippage, sells at the open minus slippage. It can never fill on its own bar, whose
prices are gone by then. Cancels arrive at once, before the next open.

Consequences:

- Resting limit and stop orders are matched only against bar opens (each fill quote), not
  intrabar highs and lows. A stop that would have triggered intrabar fills at the next open.
- The next bar is the next one in the catalog. Because a derived catalog holds only bars in the
  profile's segments, an order sent on a session's last bar fills at the next session's first
  bar, overnight gap included, with no special case.
- A strategy that subscribes to quotes sees the fill quotes. That reveals the next open one
  nanosecond after a bar closes, but an order sent then fills at that same open, so it gives no
  edge.
- Price precision matters: slippage is rounded against the trader to the increment. The ingest
  default of 4 decimals keeps 2 bps on a $40 stock from being rounded up to a whole cent.

### Costs

- **Commission** (`sim/fees.rs`, `PerShareFeeModel`): `max(per_share * shares, min_per_order)`
  per order, rounded half away from zero to cents. Each fill is charged the increase in the
  order's total, so partial fills cost the same as one fill.
- **Slippage**: built into the fill quote, as above. Reported as the fill price's distance from
  the open times the quantity.
- **Borrow** (`sim/ledger.rs`, `BorrowModule`, an engine simulation module): at each session's
  last bar close, `short shares * close * bps / 10000 * days / 365`, where `days` is the calendar
  days to the next trading day in the sessions file (3 over a weekend). Applied to the venue
  account and recorded in the ledger. Positions closed intraday pay nothing.

### Account and session rules

A gate (`sim/gate.rs`) replaces the engine's `RiskEngine.queue_execute` endpoint, which strategy
orders go through, and the order emulator's endpoint. It checks each command after the strategy
callback returns, then forwards it to the risk engine or denies it with an `OrderDenied` event
(`on_order_denied` in the strategy; modifications get `OrderModifyRejected`). It denies:

- orders sent other than at a bar close (or one nanosecond after, in fill callbacks). An order
  from a timer between bars would otherwise fill at an open that is already in the past.
- orders on the final bar of the run, which has no next bar to fill at.
- with `overnight = "flatten"`, orders sent at either of a session's last two bar closes.
- emulated orders.
- orders that would take the position short when `allow_short = false`, or whose worst case
  (position plus all working orders on that side plus the new order) is worth more than
  `min(max_leverage, max_position_pct_equity / 100) * equity`. Positions and equity are valued at
  the last bar close. Reduce-only orders and contingent child orders are not limited.

The engine's own margin checks are inert: the instrument has zero margin rates, so the gate is
the single source of position limits. `CASH` accounts allow a slightly negative balance, so a
fill above the last close does not abort the run.

With `overnight = "flatten"`, an actor (`sim/flatten.rs`) fires at each session's second-to-last
bar close, after the strategies have handled that bar. It cancels every working order and
submits a reduce-only market order per open position, on behalf of its strategy, with client
order ID `FLATTEN-<strategy>-<n>`. The orders fill at the open of the session's last bar. The
gate then blocks strategy orders until the next session.

### Ledger and reports

The ledger (`sim/ledger.rs`) records every fill from the engine's order events, plus borrow
charges, in exact decimals. Equity is cash plus the net position marked at a price, the same for
margin and cash accounts. The reports (`sim/report.rs`) replay the ledger against the bars:

- **Round trips**: a position per strategy from flat to flat. A reversing fill closes one and
  opens the next; its commission and slippage are split pro rata.
- **Daily records**: equity at each session's last bar close, with PnL and return against the
  previous close (or the starting capital).
- **Metrics**: see below.

A simulation test checks that the ledger's final equity equals the engine's account balance to the cent.

## Outputs

All files are deterministic: no wall-clock times, random IDs, or paths beyond those in the
config. Parquet files are zstd-compressed, every column nullable.

### `results.parquet` (one row)

| Column                                      | Meaning                                                                                                    |
| ------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| `instrument_id`, `bar_type`                 | From the config                                                                                            |
| `session_profile`, `overnight`, `seed`      | From the config                                                                                            |
| `build_fingerprint`                         | Engine build                                                                                               |
| `start_date`, `end_date`                    | First and last trading day with bars                                                                       |
| `sessions`, `bars`                          | Trading days and bars simulated                                                                            |
| `starting_capital`, `ending_equity`         | Equity at the start and at the last bar close                                                              |
| `net_pnl`                                   | `ending_equity - starting_capital`, after all costs                                                        |
| `gross_pnl`                                 | `net_pnl + total_costs`                                                                                    |
| `total_costs`                               | `commissions + slippage + borrow`                                                                          |
| `return_pct`                                | `net_pnl / starting_capital * 100`                                                                         |
| `sharpe`                                    | Mean / sample standard deviation of daily returns * sqrt(252); null with fewer than 2 days or no variation |
| `max_drawdown_pct`                          | Largest peak-to-trough fall of equity marked at every bar close, in % of the peak                          |
| `trade_count`, `winning_trades`, `win_rate` | Closed round trips, those with positive net PnL, and their share                                           |
| `fill_count`, `order_count`                 | Fills, and orders with at least one fill                                                                   |
| `denied_orders`                             | Orders denied by the simulation's rules                                                                    |
| `traded_notional`, `turnover`               | Total fill value, and that divided by the average daily equity                                             |
| `exposure_pct`                              | Share of bar closes with an open position, in %                                                            |
| `avg_gross_exposure`                        | Average position value / equity at bar closes                                                              |
| `final_position`                            | Net shares at the end                                                                                      |

Money is in the account currency, stored as `float64` from exact decimals.

### `trades.parquet`

One row per fill (`record = "fill"`) in the order they happened, then one row per round trip
(`record = "position"`) in the order they opened.

| Column                                          | Fill                                  | Position                                              |
| ----------------------------------------------- | ------------------------------------- | ----------------------------------------------------- |
| `strategy_id`, `instrument_id`                  | Owner and instrument                  | Same                                                  |
| `side`                                          | `BUY` or `SELL`                       | `LONG` or `SHORT`                                     |
| `quantity`                                      | Shares filled                         | Peak shares                                           |
| `price`                                         | Fill price                            | Average entry price                                   |
| `ts`                                            | Fill time                             | Opening fill time                                     |
| `ts_closed`, `price_close`                      | null                                  | Closing time and average exit price (null while open) |
| `commission`, `slippage`                        | Of the fill                           | Of the round trip                                     |
| `pnl_gross`, `pnl_net`                          | null                                  | Realized PnL before and after commission              |
| `open_price`, `segment`                         | Open and segment of the fill bar      | null                                                  |
| `client_order_id`, `venue_order_id`, `trade_id` | Order and trade IDs                   | null                                                  |
| `flatten`                                       | Whether the overnight flatten sent it | null                                                  |

### `returns.parquet` (one row per trading day)

`date`, `ts` (last bar close), `close`, `position` (net shares), `equity`, `pnl`, `return`
(fraction of the previous equity), `borrow` (charged at that close). This is the series for the
deflated Sharpe ratio and novelty correlation.

### `run.json`

`format` (`nautilus-backtest-run/1`), `engine` (version, build fingerprint, compiler), `config`
(the parsed config, echoed), `inputs` (BLAKE3 of the config text, the catalog tree, the sessions
file, and each plug-in with its name, version, and strategy types), `data` (first and last
session, counts), `engine_totals`, `denied_orders` (count per reason), and `outputs` (BLAKE3 of
the three Parquet files).

## Writing a strategy plug-in

### The strategy slot

`rt/plugins/strategy/` is the only crate a strategy author edits, and only its `src/`. Its
`Cargo.toml` is **frozen**:

```toml
[package]
name = "strategy-plugin"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish = false

[lints]
workspace = true

[lib]
name = "strategy_plugin"        # target/<profile>/libstrategy_plugin.so
crate-type = ["dylib"]

[dependencies]
nautilus-rt = { workspace = true }

[dev-dependencies]
rstest = { workspace = true }
```

`rt/Cargo.toml` and `rt/Cargo.lock` are hashed into the build fingerprint, so adding a crate or a
dependency anywhere in the workspace changes the build and workers refuse the plug-in. Anything
a strategy needs beyond `nautilus-rt`'s re-exports gets added to `nautilus-rt` here, in the
engine repository.

The plug-in exports one strategy type, `"Strategy"`, so every run config uses
`type_name = "Strategy"`. The template in `src/lib.rs` is a moving-average crossover on bar
closes (`SmaCross`), configured with `instrument_id`, `bar_type`, `fast_period`, `slow_period`,
`trade_size`, and `go_short`, with tests in `src/tests.rs`. A pristine copy lives in
`rt/plugins/strategy-template/src/`:

```bash
cd rt
rm -rf plugins/strategy/src && cp -r plugins/strategy-template/src plugins/strategy/src
```

### Writing the strategy

A plug-in is an ordinary Rust strategy, written exactly as in the engine (`StrategyCore`,
`nautilus_strategy!`, `DataActor`), compiled as a `dylib` that depends only on `nautilus-rt`.
The skeleton:

```rust
use nautilus_rt::{
    anyhow,
    common::actor::DataActor,
    model::{data::{Bar, BarType}, identifiers::InstrumentId},
    serde::Deserialize,
    serde_json,
    trading::{nautilus_strategy, strategy::{Strategy, StrategyConfig, StrategyCore}},
};

nautilus_rt::strategy_plugin! {
    name: "strategy",
    version: env!("CARGO_PKG_VERSION"),
    strategies: {
        "Strategy" => MyStrategy::from_json,
    },
}

#[derive(Debug, Deserialize)]
#[serde(crate = "nautilus_rt::serde", deny_unknown_fields)] // required: serde comes via nautilus_rt
pub struct MyConfig {
    pub base: StrategyConfig,
    pub instrument_id: InstrumentId,
    pub bar_type: BarType,
}

pub struct MyStrategy { core: StrategyCore, bar_type: BarType /* ... */ }

impl MyStrategy {
    pub fn from_json(config: serde_json::Value) -> anyhow::Result<Self> {
        let config: MyConfig = serde_json::from_value(config)?;
        Ok(Self { core: StrategyCore::new(config.base), bar_type: config.bar_type })
    }
}

nautilus_strategy!(MyStrategy);

impl std::fmt::Debug for MyStrategy { /* ... */ }

impl DataActor for MyStrategy {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        // Orders sent here fill at the next bar's open
        Ok(())
    }
}
```

To react to denials or fills, pass extra trait items to the macro:
`nautilus_strategy!(MyStrategy, { fn on_order_denied(&mut self, event: OrderDenied) { ... } });`.

### Plug-in authoring checklist

- Use `crate-type = ["dylib"]`, the convention. Inside `rt/`, a `cdylib` also links the shared
  runtime and behaves identically: the earlier claim here that a `cdylib` embeds its own copy of
  the engine was wrong for this workspace. What keeps a plug-in sound is building it in `rt/`
  and depending only on `nautilus-rt`, not the crate type.
- Depend on `nautilus-rt` only. Use everything through its re-exports: `nautilus_rt::model`,
  `common`, `trading`, `indicators`, `execution`, `portfolio`, `analysis`, `backtest`,
  `system`, `core`, `persistence`, `serialization`, and third-party `anyhow`, `serde`,
  `serde_json`, `log`, `ustr`, `indexmap`, `rust_decimal`, `jiff`, `arrow`, `parquet`, `blake3`
  (list in `rt/runtime/src/lib.rs`). Adding any of these directly links a second copy.
- Put `#[serde(crate = "nautilus_rt::serde")]` on every serde derive.
- Avoid derives and macros that hard-code a crate path, such as `bon::Builder`. They expect that
  crate as a direct dependency.
- `strategy_plugin!` goes once per plug-in crate, at module scope.
- Factory signature: `fn(serde_json::Value) -> anyhow::Result<YourStrategy>`.
- Send orders from `on_bar` (or fill callbacks), not from timers between bars.
- Testing-only dependencies such as `rstest` are fine under `[dev-dependencies]`.

### Testing a strategy in-process

The tests run the strategy straight in an engine, with no library loading, using the runner's
own `BarSimulation` on the synthetic equity scenario (`SYNTH.XNYS`, weekdays from 2025-01-06,
78 regular bars a day):

```rust
use nautilus_rt::{
    sdk::StrategyRegistrar,
    serde_json::json,
    sim::{BarSimulation, SimConfig},
    synthetic::EquityScenario,
};

let scenario = EquityScenario::new(5).unwrap();
let config: SimConfig = serde_json::from_value(json!({ /* venue, fill_model, overnight, seed */ })).unwrap();
let mut sim = BarSimulation::new(&config, scenario.instrument.clone(), scenario.bars.clone(), &scenario.schedule).unwrap();
StrategyRegistrar::backtest(sim.engine_mut()).add_strategy(MyStrategy::from_json(json!({ /* ... */ })).unwrap()).unwrap();
let outcome = sim.run().unwrap(); // fills, round trips, daily records, metrics
assert_eq!(outcome.metrics.trade_count, 7);
```

Use one engine per test. The engine keeps its state per thread, and by default the test harness
runs each test on its own thread.

## Testing

Run everything from `rt/`:

```bash
cd rt
cargo test                                         # all 137 tests
cargo test -p nautilus-rt                          # runtime unit tests (69)
cargo test -p strategy-plugin                      # template strategy tests (12)
cargo test -p nautilus-backtest-runner             # config tests (11) and end-to-end tests (14)
cargo test -p nautilus-ingest                      # split tests (13) and end-to-end tests (11)
cargo test -p nautilus-example-ema-cross-plugin    # quote example (7)
cargo +nightly-2026-08-21 fmt --check
cargo clippy --all-targets
```

What they cover:

- **Runtime** (`rt/runtime/src`): sessions file round trip and window lookup; catalog round
  trip; digests; config validation; the fee model; borrow charges; round-trip accounting and
  Sharpe; and simulation tests (`sim/tests.rs`) with a scripted strategy: fills at the next open
  with slippage for buys and sells, fills across the overnight gap, commission minimum and
  rounding, short-sale denial, exposure and position-percentage limits, the final-bar denial,
  flatten exits and blocking, borrow over a weekend, the ledger matching the engine's account,
  and metrics adding up.
- **Strategy slot** (`rt/plugins/strategy/src/tests.rs`): config parsing and rejection, and
  in-process backtests: pinned totals (8 orders, 7 round trips over 5 sessions), every fill at
  the next bar's open, long-only mode.
- **Runner** (`rt/backtest_runner`): config parsing, every value required, rejection of bad
  values; end-to-end runs of the real binary and plug-in on a generated catalog, with the same
  totals as the in-process test; **byte-identical outputs across two runs**
  (`outputs_are_deterministic`); flatten; date-range selection; `--build-info`; and each setup
  error path.
- **Ingest** (`rt/ingest`): split parsing and embargo; end-to-end runs on generated raw files
  with deliberately wrong vendor `segment` labels: catalogs per split equal to the source bars,
  embargo, extended profiles, profile hash, deterministic manifest, and failures for a bar
  outside every session, a missing segment, too many decimals, empty and overlapping splits, a
  bad segment name, and a non-empty output directory.

### Checks done by hand

Synthetic dataset: `bullpen-synth-data/out/SYNTH` (10 years, all segments), sessions generated
with `exchange_calendars` 4.13 for 2016-09-01 to 2026-10-30, profile `rth`, the splits and
embargo above.

- **Ingest**: 376,646 raw bars (105,414 pre-market, 195,180 regular, 76,052 after-hours), all
  inside a session segment. 194,556 regular bars in splits: train 1,569 sessions and 121,914
  bars (2016-10-03 to 2022-12-23), validation 498 and 38,664 (2023-01-03 to 2024-12-24, last
  bar at the 13:00 ET early close), holdout 437 and 33,978. 0.7 s in debug.
- **Backtests** of the template (`fast_period = 20`, `slow_period = 80`, `trade_size = 500`,
  shorting on, the `backtest.yaml` placeholder account and fill values), from the release bundle
  without `LD_LIBRARY_PATH`:
  - train: 1,852 round trips, net PnL -38,642.11 (gross -5,235.00, costs 33,407.11: commission
    9,262.50, slippage 23,630.20, borrow 514.41), Sharpe -0.42, max drawdown 54.8%.
  - validation: 615 round trips, net PnL -32,269.01, Sharpe -1.45.
  - validation with `overnight = "flatten"`: 497 flatten exits, no position at any session
    close, no borrow, 504 strategy orders denied on session-closing bars.

  A crossover with no edge loses to costs on data without a planted effect, as expected.
- **Spot checks of the files**: the first train fill sells 500 at 41.9916 against an open of
  42.00 (2 bps down, rounded down to the increment); its round trip's PnL and pro-rata
  commission; the first borrow charge (500 * 41.87 * 0.5% / 365 = 0.29).
- **Determinism on the real data**: two train runs give byte-identical files.
- **`cdylib` plug-in**: with the slot temporarily built as a `cdylib`, the library still links
  `libnautilus_rt.so`, exports only the three plug-in symbols (against about 1,200 for the
  `dylib`), and the runner's end-to-end tests pass with the same totals.
- **Feature stability**: after a full workspace build, `cargo build -p strategy-plugin` reuses the
  same `nautilus-rt`. The runner's and ingest tool's own dependencies (`clap`, `toml`) do not
  change the runtime's features.

Checks from the prototype, still valid:

- **Parity with the static build.** The upstream `engine-ema-cross` example and the plug-in
  produce identical logs, after removing wall-clock timestamps, UUIDs, PID, and banner lines.
  `ema_cross`'s in-process test keeps asserting the upstream totals.
- **Stale binary.** Edit any engine source, rebuild only the runtime
  (`cargo build -p nautilus-rt`), then run the old `nautilus-backtest`. Result:
  `host binary was built against runtime <a>, but the loaded runtime is <b>; rebuild the host and runtime together`.
- **Stale plug-in.** Same edit, rebuild only the runner, then load the old plug-in. Result:
  `plug-in 'ema-cross' was built against runtime <a>, but the loaded runtime is <b>; rebuild the plug-in with the runtime`.
- **Wrong directory.** Building with `cargo build --manifest-path rt/Cargo.toml` from the repo
  root fails with:
  `nautilus-rt must be built with -C prefer-dynamic; run cargo from the rt directory ...`.

## How it works

### One engine, shared

The engine is compiled once into `libnautilus_rt.so` (`crate-type = ["dylib"]` in
`rt/runtime/Cargo.toml`), now including the Parquet catalog stack (`nautilus-persistence`). The
binaries and every plug-in link it dynamically, together with Rust's shared `libstd`. The
engine's process-wide state is therefore a single copy: message bus, actor registry, clocks,
logger, and interned strings.

Every crate in the workspace depends only on `nautilus-rt` plus `clap` and `toml`, which the
runtime library does not link (`toml` is only a build dependency inside the engine), so building
one package (`-p strategy-plugin`) resolves the runtime with the same features as a full build. Keep it that way: a binary that
depends on an engine crate or a re-exported crate directly would change the runtime's features
for builds that include it.

### Loading sequence

1. **Host build check.** `main` in `rt/backtest_runner/src/main.rs` calls
   `nautilus_rt::ensure_host_build(nautilus_rt::BUILD_FINGERPRINT)`. The constant is compiled
   into the binary, and the runtime compares it with its own fingerprint.
2. **Config.** For each `[[strategies]]` entry, `PluginLoader::add_backtest_strategy` loads the
   library, or reuses it if already loaded.
3. **Verification.** `load_library` (`rt/runtime/src/loader.rs`) verifies in this order, each
   step trusting only what earlier steps established:
   1. The C-ABI `nautilus_plugin_init` manifest from upstream's `nautilus-plugin` crate: ABI
      version, plus model precision mode.
   2. The C-ABI `nautilus_rt_plugin_fingerprint`, which must equal the loaded runtime's
      fingerprint.
   3. Only then the Rust-ABI `NAUTILUS_RT_STRATEGY_PLUGIN` declaration.
4. **Registration.** The plug-in's generated `register` function deserializes the config with
   the factory. It then calls `StrategyRegistrar::add_strategy` (`rt/runtime/src/sdk.rs`), which
   calls `BacktestEngine::add_strategy` with the plug-in's concrete strategy type. No wrapper or
   dynamic dispatch sits on the hot path.
5. **Lifetime.** Libraries are never unloaded.

`strategy_plugin!` (`rt/runtime/src/sdk.rs`) generates all three exported symbols.

### Build fingerprint

The plug-in boundary passes Rust types directly. Rust has no stable ABI, so that is only sound
when the host, runtime, and plug-ins come from one build. `rt/runtime/build.rs` hashes (BLAKE3)
everything that defines "one build":

- `rustc -vV` output, target, profile, opt-level, debug, panic strategy, target features, and
  rustflags (`BUILD_ENV_KEYS` in `build.rs`).
- `rt/Cargo.lock`, `rt/Cargo.toml`, and the root `Cargo.toml` (feature and version specs of
  engine dependencies).
- Every `.rs` and `.toml` file under `crates/` (engine sources) and `rt/runtime/`.

The result is `nautilus_rt::BUILD_FINGERPRINT`. A constant is evaluated where it is used, so each
plug-in and binary records the fingerprint it was compiled against. `runtime_fingerprint()`
reports the fingerprint of the library actually loaded. `nautilus_rt::RUSTC_VERSION` is the
first line of `rustc -vV`.

**In practice:** build the runtime, binaries, and plug-ins together with one `cargo build` from
`rt/`, and ship them together.

### Why `-C prefer-dynamic` and the separate workspace

Cargo passes `-C prefer-dynamic` to a `dylib` only when building it as a dependency. When the
runtime itself is the package being built (`-p nautilus-rt`, or `--workspace`), cargo links
`std` statically into it. The binaries and plug-ins can then no longer link, and an already-built
binary could load it with a second copy of `std` without the fingerprint changing.
`rt/.cargo/config.toml` always passes the flag, and `check_build_config` in `build.rs` refuses
to build without it.

Keeping this in a separate workspace also lets `rt/` use the profiles a dylib needs (see
below) without touching upstream's.

## Performance

### Bar backtests

Release build on the 4-core development VM, template strategy, `rth` profile:

| Run        | Bars    | Sessions | Simulation | Whole process |
| ---------- | ------- | -------- | ---------- | ------------- |
| train      | 121,914 | 1,569    | 0.50 s     | 0.63 s        |
| validation | 38,664  | 498      | 0.16 s     | 0.23 s        |

Medians of three runs from the bundle; "whole process" includes loading the catalog and writing
the outputs. The engine processes two data points per bar (the bar and the fill quote).
Ingesting the whole dataset takes 0.35 s.

### Plug-in boundary (prototype measurements)

Measured on the same VM with the quote-based EMA-cross strategy on 960,025 synthetic quotes, a
`NETTING` venue, 7 interleaved runs per variant, median:

| Variant                                                    | Quotes/s | vs today's static |
| ---------------------------------------------------------- | -------- | ----------------- |
| Static, fat LTO, `panic = "abort"` (upstream release)      | 423,979  | baseline          |
| Static, fat LTO, `panic = "unwind"`                        | 404,815  | −4.5%             |
| Static, no LTO, unwind (same flags as plug-in build)       | 340,707  | −19.6%            |
| **Plug-in, stable** (what this branch builds)              | 340,895  | −19.6%            |
| Plug-in, nightly, no LTO (control for the row below)       | 328,298  | −22.6%            |
| Plug-in, nightly, `-Zdylib-lto` fat LTO inside the runtime | 362,710  | −14.5%            |

Rebuild after a strategy-only change (release): **2.5 s** for the plug-in, against
**1 min 47 s** for the static fat-LTO binary.

What this means:

- **The plug-in boundary itself costs 0–3%**: the plug-in row against the static row with the
  same flags.
- **The cost is in build settings that a `dylib` forces.** Losing fat LTO costs about 14%.
  `panic = "unwind"`, required because Rust's shared `std` is built to unwind, costs about 5%.
  `rt/Cargo.toml` therefore uses `lto = false` and `panic = "unwind"` in release.
- **Nightly `-Zdylib-lto` recovers about 10% of that**, but needs a nightly compiler for
  production builds. Not adopted. To reproduce it, build from `rt/` with
  `cargo +nightly-2026-08-21 build --release --target-dir <dir>`, using a `RUSTC_WRAPPER` script
  that appends `-C embed-bitcode=yes --cap-lints=warn` to every rustc call and, when
  `--crate-name nautilus_rt` is present, adds `-C lto=fat -Zdylib-lto`.
- **Benchmark with `NETTING`.** With `HEDGING`, per-quote portfolio work grows with the
  open-position count and long runs slow down quadratically.

## Rules and gotchas

- **Run cargo from `rt/`.** From anywhere else, `rt/.cargo/config.toml` is not read and the
  runtime build fails by design.
- **Rebuild and ship binaries, runtime, and plug-ins together.** Any mismatch is refused at
  startup. That is intended: it prevents undefined behavior. Changing `rt/.cargo/config.toml`
  changes rustflags, so it rebuilds everything and changes the fingerprint.
- **Plug-ins depend only on `nautilus-rt`** (see the checklist above), and the strategy slot's
  `Cargo.toml` stays frozen.
- **Relative paths resolve from the working directory**, not from the config file's location.
- **Libraries are loaded once at startup and never unloaded.** There is no mid-run swap, by
  design.
- **`sha256` is not supported** in `[[strategies]]`. Upstream's live `PluginConfig` has it;
  `StrategyPluginConfig` rejects it as an unknown field. `run.json` records each plug-in's
  BLAKE3 digest instead.
- **Only Linux was tested.** The `$ORIGIN` rpath is Linux-only; other platforms need their own
  equivalent.
- **The root repository's checks do not cover `rt/`.** `make pre-flight` and the root CI only
  see the root workspace. Run the commands under [Testing](#testing) yourself.
- **`rt/Cargo.lock` is separate from the root lockfile.** When pulling upstream, re-seed it so
  versions match the root again: `cp Cargo.lock rt/`, then run `cargo build` from `rt/` to prune
  it and add `rt`-only crates (currently just `libloading`). Then run the cooldown check:
  `bash scripts/check-cargo-cooldown.sh --lock rt/Cargo.lock --all` (passes for all 346 crates
  today; 35 of them are looked up on crates.io because the root's
  `.supply-chain/crate-dates.json` does not list them).
- **The root `[patch.crates-io]` entries for `arrow` and `parquet` are mirrored** in
  `rt/Cargo.toml`. They patch Arrow 57, which the `rt` graph does not use, so cargo warns that
  they are unused. Keep them in step with the root.

## Next steps

Roughly in priority order:

1. **Engine build image.** Build `rt/` once in CI at a fixed path (`bullpen-images`,
   `engine-build`), run `scripts/bundle.bash` for the worker image, and give agent images the
   full `rt/` workspace and `target/`.
2. **Holdout and Monte Carlo runs.** The runner already takes any split; Monte Carlo needs
   slippage and fee noise and parameter jitter as config, and trade-list post-processing on
   `trades.parquet`.
3. **Multiple symbols.** One instrument per run today. Running several symbols means one venue,
   several instruments, and position limits per symbol.
4. **Parameter sweeps in one process.** Several configs, one engine per thread, sharing the
   loaded data.
5. **Performance.** Decide whether the ~20% `dylib` gap matters for sweep workloads: accept it,
   adopt nightly `-Zdylib-lto`, or add a frozen mode that statically links a chosen strategy.
6. **Live trading.** Implement upstream's stubbed `LiveNode` plug-in hooks
   (`crates/live/src/node/mod.rs`) with the same loader.
7. **Remove Python.** The planned "soft rip": delete `python/` and `crates/pyo3`, keep the
   `cfg(feature = "python")` code dormant, and fail CI if `pyo3` appears in the dependency
   tree.
8. **CI for `rt/`.** A job running fmt, clippy, and tests from `rt/`.
9. **macOS and Windows** builds and tests.

## Machine setup changes made during this work

On the development VM:

- **Global git identity:** `alefin64 <lefinfamily@gmail.com>` in `~/.gitconfig`.
- **apt packages:** `build-essential clang lld pkg-config gpg jq`.
- **HashiCorp apt repository:** the existing source was missing its keyring
  (`/usr/share/keyrings/hashicorp-archive-keyring.gpg`). The current signing key was installed
  (`D55C 0D1A C78A 8D81 26CB 631C FC9C A96A CA02 6560`), and `terraform` 1.16.6 was installed.
  The binary matches the official release zip, whose `SHA256SUMS` is signed by HashiCorp's
  security key (`C874 011F 0AB4 0511 0D02 1055 3436 5D94 72D7 468F`).
- **rustup:** installed with toolchains `1.99.0` (default) and `nightly-2026-08-21` (rustfmt),
  plus the clippy component.
