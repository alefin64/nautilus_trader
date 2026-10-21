# Bullpen handover: compiled strategy plug-ins for backtesting

Branch `rust-plugin-backtest`, based on release tag `v2.0.0rc7` (Rust crates 0.66.0).

## Status

Backtests can load strategies from compiled Rust plug-ins (`.so` files) at startup. There is
no Python anywhere in this path.

- **Correctness.** A strategy loaded as a plug-in produces the same trading log line for line
  as the same strategy statically linked into the upstream `engine-ema-cross` example: same
  orders, fills, positions, and statistics.
- **Safety.** A mismatched or stale plug-in or host is refused at startup with a clear error,
  rather than running with undefined behavior.
- **Speed.** Today it runs about 20% slower than the upstream fat-LTO static build. Almost all
  of that comes from build settings a shared library forces, not from the plug-in boundary.
  See [Performance](#performance).
- **Iteration.** A strategy-only change rebuilds in about 2.5 s in release, against about
  1 min 47 s for a statically linked fat-LTO binary.
- **Scope.** The backtest runner is a prototype. It has one simulated FX venue (`SIM`) and
  synthetic AUD/USD quotes. Historical data loading is the next step.

## Where everything is

All plug-in work lives in its own cargo workspace, `rt/`. The root workspace is untouched, so
merges from upstream stay conflict-free.

| Path                                    | What it is                                                                          |
| --------------------------------------- | ----------------------------------------------------------------------------------- |
| `rt/Cargo.toml`                         | Workspace manifest: members, shared dependencies, profiles, lints                   |
| `rt/.cargo/config.toml`                 | Adds `-C prefer-dynamic`; required, see [Rules](#rules-and-gotchas)                 |
| `rt/Cargo.lock`                         | Lockfile, seeded from the root `Cargo.lock` (same dependency versions)              |
| `rt/runtime/`                           | `nautilus-rt`: the engine as one shared library, plus the plug-in SDK and loader    |
| `rt/runtime/src/lib.rs`                 | Re-exports and the build fingerprint                                                |
| `rt/runtime/src/sdk.rs`                 | Plug-in side: the `strategy_plugin!` macro and `StrategyRegistrar`                  |
| `rt/runtime/src/loader.rs`              | Host side: `PluginLoader` and `StrategyPluginConfig`                                |
| `rt/runtime/src/synthetic.rs`           | Synthetic FX scenario: venue, instrument, quote series                              |
| `rt/runtime/build.rs`                   | Build-config guards and fingerprint computation                                     |
| `rt/backtest_runner/`                   | `nautilus-backtest` binary: reads a TOML config, loads plug-ins, runs the backtest  |
| `rt/backtest_runner/tests/cli.rs`       | End-to-end tests of the binary and plug-in                                          |
| `rt/plugins/ema_cross/`                 | Example plug-in: dual-EMA crossover                                                 |
| `rt/plugins/ema_cross/src/tests.rs`     | Example strategy tests, including an in-process backtest                            |
| `rt/plugins/ema_cross/backtest.toml`    | Example runner config                                                               |
| `rt/README.md`                          | Short reference for the workspace                                                   |

Commits on the branch, oldest first:

| Commit       | Summary                                                    |
| ------------ | ---------------------------------------------------------- |
| `2e3d32a2ee` | Add shared runtime dylib and strategy plug-in prototype    |
| `4679615249` | Move strategy plug-in prototype into its own rt workspace  |
| `130a6b8947` | Make simulated venue OMS type configurable in backtest runner |
| `0187f16047` | Add tests and shared synthetic scenario to rt workspace    |

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

## Running a backtest

**Always run cargo from `rt/`.** Cargo reads `rt/.cargo/config.toml` based on the current
directory, and the runtime refuses to build without it.

```bash
cd rt
cargo build                       # runtime, runner, and all plug-ins (debug)
cargo run --bin nautilus-backtest -- --config plugins/ema_cross/backtest.toml
```

For speed, use release. Rebuild the runtime, runner, and plug-ins together, then point the
config at `target/release/...`:

```bash
cd rt
cargo build --release
sed 's#target/debug/#target/release/#' plugins/ema_cross/backtest.toml > /tmp/backtest-release.toml
cargo run --release --bin nautilus-backtest -- --config /tmp/backtest-release.toml
```

The run logs the usual engine output to stdout, ending with the performance report and one
throughput line:

```text
[INFO] TRADER-001.nautilus_rt::loader: Loaded plug-in ema-cross 0.66.0 from .../libema_cross_plugin.so with strategies ["EmaCross"]
...
[INFO] TRADER-001.nautilus_backtest::engine: Total orders: 12
[INFO] TRADER-001.nautilus_backtest::engine: Total positions: 12
...
Processed 745 quotes in 4.230ms (176121 quotes/s)
```

### Running the binary outside cargo

`cargo run` puts the runtime library and Rust's shared `std` on the library path. To run the
binary directly, set `LD_LIBRARY_PATH` yourself. Run this from `rt/`, so `rustc` resolves to the
pinned toolchain whose `libstd` the binary was linked against:

```bash
cd rt
export LD_LIBRARY_PATH="$PWD/target/release:$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/lib"
target/release/nautilus-backtest --config /tmp/backtest-release.toml
```

No rpath is set yet. Packaging a self-contained bundle is listed under [Next steps](#next-steps).

### Config reference

The runner config is TOML, parsed into `RunnerConfig` in `rt/backtest_runner/src/main.rs`.
Unknown fields are errors.

| Key                  | Type            | Default     | Meaning                                                                          |
| -------------------- | --------------- | ----------- | -------------------------------------------------------------------------------- |
| `synthetic_cycles`   | integer         | `6`         | Number of synthetic up/down cycles. Quote count is `25 + 120 * cycles`.          |
| `oms_type`           | string          | `"HEDGING"` | Venue order management: `"HEDGING"` or `"NETTING"`.                              |
| `[[strategies]]`     | array of tables | required    | One entry per strategy instance (see below).                                     |

Each `[[strategies]]` entry is a `StrategyPluginConfig` (`rt/runtime/src/loader.rs:49`):

| Key         | Type   | Meaning                                                                                |
| ----------- | ------ | -------------------------------------------------------------------------------------- |
| `path`      | string | Plug-in library path. Relative paths resolve from the **process working directory**.   |
| `type_name` | string | Strategy type exported by the plug-in, for example `"EmaCross"`.                       |
| `config`    | table  | Instance config, passed to the plug-in's factory as JSON. Its shape is up to the strategy. |

Several strategies, from one plug-in or many, can run in one backtest. Each library is loaded
once, however many instances use it. Each instance needs a unique `strategy_id` and
`order_id_tag`:

```toml
synthetic_cycles = 6
oms_type = "NETTING"

[[strategies]]
path = "target/release/libema_cross_plugin.so"
type_name = "EmaCross"

[strategies.config]
instrument_id = "AUD/USD.SIM"
trade_size = "100000"
fast_period = 10
slow_period = 20

[[strategies]]
path = "target/release/libema_cross_plugin.so"
type_name = "EmaCross"

[strategies.config]
instrument_id = "AUD/USD.SIM"
trade_size = "50000"
fast_period = 5
slow_period = 40

[strategies.config.base]          # engine StrategyConfig fields
strategy_id = "EMA_CROSS-002"
order_id_tag = "002"
```

`EmaCross` defaults `base` to `strategy_id = "EMA_CROSS-001"` and `order_id_tag = "001"`. That
is why the second instance above sets its own `base`.

## Writing a strategy plug-in

A plug-in is an ordinary Rust strategy, written exactly as in the engine (`StrategyCore`,
`nautilus_strategy!`, `DataActor`), compiled as a `dylib` that depends only on `nautilus-rt`.
The complete example below builds and runs together with `EmaCross` in one backtest.

### 1. Create the crate

`rt/plugins/buy_and_hold/Cargo.toml`:

```toml
[package]
name = "buy-and-hold-plugin"
version = "0.1.0"
edition.workspace = true
publish = false

[lints]
workspace = true

[lib]
name = "buy_and_hold_plugin"      # library file: target/<profile>/libbuy_and_hold_plugin.so
crate-type = ["dylib"]            # a Rust dylib, not a cdylib

[dependencies]
nautilus-rt = { workspace = true } # the only dependency
```

Add it to `members` in `rt/Cargo.toml`:

```toml
members = [
  "backtest_runner",
  "plugins/buy_and_hold",
  "plugins/ema_cross",
  "runtime",
]
```

### 2. Write the strategy

`rt/plugins/buy_and_hold/src/lib.rs`:

```rust
//! Buys once on the first quote and holds.

use std::fmt::Debug;

use nautilus_rt::{
    anyhow,
    common::actor::DataActor,
    model::{
        data::QuoteTick,
        enums::OrderSide,
        identifiers::InstrumentId,
        types::Quantity,
    },
    serde::Deserialize,
    serde_json,
    trading::{
        nautilus_strategy,
        strategy::{Strategy, StrategyConfig, StrategyCore},
    },
};

// Exports the plug-in. Each entry maps a `type_name` to a factory taking the JSON config.
nautilus_rt::strategy_plugin! {
    name: "buy-and-hold",
    version: env!("CARGO_PKG_VERSION"),
    strategies: {
        "BuyAndHold" => BuyAndHold::from_json,
    },
}

/// Configuration for [`BuyAndHold`].
#[derive(Debug, Deserialize)]
#[serde(crate = "nautilus_rt::serde", deny_unknown_fields)] // required: serde comes via nautilus_rt
pub struct BuyAndHoldConfig {
    /// Base strategy configuration (`strategy_id`, `order_id_tag`, ...).
    pub base: StrategyConfig,
    /// Instrument to buy.
    pub instrument_id: InstrumentId,
    /// Quantity to buy.
    pub trade_size: Quantity,
}

/// Buys `trade_size` on the first quote, then holds.
pub struct BuyAndHold {
    core: StrategyCore,
    instrument_id: InstrumentId,
    trade_size: Quantity,
    bought: bool,
}

impl BuyAndHold {
    /// Creates the strategy from its JSON config.
    ///
    /// # Errors
    ///
    /// Returns an error if `config` does not deserialize into [`BuyAndHoldConfig`].
    pub fn from_json(config: serde_json::Value) -> anyhow::Result<Self> {
        let config: BuyAndHoldConfig = serde_json::from_value(config)?;
        Ok(Self {
            core: StrategyCore::new(config.base),
            instrument_id: config.instrument_id,
            trade_size: config.trade_size,
            bought: false,
        })
    }
}

nautilus_strategy!(BuyAndHold);

impl Debug for BuyAndHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuyAndHold")
            .field("instrument_id", &self.instrument_id)
            .field("bought", &self.bought)
            .finish()
    }
}

impl DataActor for BuyAndHold {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_quotes(self.instrument_id, None, None);
        Ok(())
    }

    fn on_quote(&mut self, _quote: &QuoteTick) -> anyhow::Result<()> {
        if self.bought {
            return Ok(());
        }
        let order = self.order().market(
            self.instrument_id,
            OrderSide::Buy,
            self.trade_size,
            None, // time_in_force
            None, // reduce_only
            None, // quote_quantity
            None, // exec_algorithm_id
            None, // exec_algorithm_params
            None, // tags
            None, // client_order_id
        );
        self.submit_order(order, None, None, None)?;
        self.bought = true;
        Ok(())
    }
}
```

### 3. Configure and run

```toml
[[strategies]]
path = "target/debug/libbuy_and_hold_plugin.so"
type_name = "BuyAndHold"

[strategies.config]
instrument_id = "AUD/USD.SIM"
trade_size = "200000"

[strategies.config.base]
strategy_id = "BUY_AND_HOLD-002"
order_id_tag = "002"
```

```bash
cd rt
cargo build
cargo run --bin nautilus-backtest -- --config my_backtest.toml
```

Run alongside the `EmaCross` example config, this reports `Total orders: 13`: 12 from
`EmaCross` plus one from `BuyAndHold`.

### Plug-in authoring checklist

- Use `crate-type = ["dylib"]`, not `cdylib`. A `cdylib` embeds its own copy of the engine, so
  its subscriptions and orders would never reach the real one.
- Depend on `nautilus-rt` only. Use everything through its re-exports: `nautilus_rt::model`,
  `common`, `trading`, `indicators`, `execution`, `portfolio`, `analysis`, `backtest`,
  `system`, `core`, and third-party `anyhow`, `serde`, `serde_json`, `log`, `ustr`,
  `indexmap`, `rust_decimal` (list in `rt/runtime/src/lib.rs`). Adding any of these directly
  links a second copy. If you need a crate that isn't re-exported, add the re-export to
  `nautilus-rt` rather than depending on it from the plug-in.
- Put `#[serde(crate = "nautilus_rt::serde")]` on every serde derive.
- Avoid derives and macros that hard-code a crate path, such as `bon::Builder`. They expect that
  crate as a direct dependency.
- `strategy_plugin!` goes once per plug-in crate, at module scope. A plug-in may export several
  strategy types.
- Factory signature: `fn(serde_json::Value) -> anyhow::Result<YourStrategy>`.
- Testing-only dependencies such as `rstest` are fine under `[dev-dependencies]`.

## Testing

Run everything from `rt/`:

```bash
cd rt
cargo test                                         # all 27 tests
cargo test -p nautilus-rt                          # runtime unit tests (13)
cargo test -p nautilus-example-ema-cross-plugin    # strategy tests (7)
cargo test -p nautilus-backtest-runner             # end-to-end tests (7)
cargo +nightly-2026-08-21 fmt --check
cargo clippy --all-targets
```

### Runtime unit tests (`rt/runtime/src`)

- `lib.rs`, `mod tests`: the fingerprint format, plus `ensure_host_build` accepting a matching
  fingerprint and rejecting another.
- `loader.rs`, `mod tests`: `StrategyPluginConfig` parsing (TOML, null default, unknown-field
  rejection), and loading a missing file or a non-library.
- `synthetic.rs`, `mod tests`: quote counts per cycle, one-second spacing, and the instrument
  definition.

### Strategy tests (`rt/plugins/ema_cross/src/tests.rs`)

This file is the template for testing your own strategies. They run **in-process**: the test
adds the strategy straight to an engine, with no library loading.

- `from_json_*`: config parsing, defaults, overrides, and rejection of bad input.
- `backtest_on_synthetic_quotes` (line 91): a full backtest on the shared synthetic scenario.
  It asserts the same totals as the upstream `engine-ema-cross` example: 745 iterations,
  24 events, 12 orders, 12 positions.

The pattern for your own strategy:

```rust
use nautilus_rt::{
    backtest::{config::BacktestEngineConfig, engine::BacktestEngine},
    model::enums::OmsType,
    sdk::StrategyRegistrar,
    serde_json::json,
    synthetic,
};
use rstest::rstest;

use super::*;

#[rstest]
fn backtest_on_synthetic_quotes() {
    let mut engine = BacktestEngine::new(BacktestEngineConfig::default()).unwrap();
    let instrument_id =
        synthetic::add_venue_and_instrument(&mut engine, OmsType::Hedging).unwrap();
    let config = json!({ "instrument_id": "AUD/USD.SIM", "trade_size": "100000" /* ... */ });
    StrategyRegistrar::backtest(&mut engine)
        .add_strategy(MyStrategy::from_json(config).unwrap())
        .unwrap();
    engine
        .add_data(synthetic::generate_quotes(instrument_id, 6), None, true, true)
        .unwrap();

    engine.run(None, None, None, false).unwrap();

    let result = engine.get_result(); // BacktestResult: iterations, totals, stats
    assert_eq!(result.total_orders, 12);
}
```

Use one backtest engine per test. The engine keeps its state per thread, and by default the test
harness runs each test on its own thread. Running several engines one after another on one
thread (for example with `--test-threads=1`) has not been tested.

### End-to-end tests (`rt/backtest_runner/tests/cli.rs`)

These run the real `nautilus-backtest` binary with the real plug-in library.

- `plugin_path()` (line 33) runs `cargo build -p nautilus-example-ema-cross-plugin` with the
  same profile as the binary under test. The plug-in is only loadable by a runtime from the
  same build, so it must match.
- `runs_plugin_strategy`: loads the plug-in and asserts the `engine-ema-cross` totals.
- `netting_venue_with_more_cycles`: the `oms_type` and `synthetic_cycles` options.
- `rejects_unknown_strategy_type`, `rejects_library_without_plugin_symbols`,
  `rejects_missing_plugin`, `rejects_invalid_strategy_config`, `rejects_unknown_runner_field`:
  each error path and its message.

### Checks done by hand (not automated)

These were verified manually while building the prototype:

- **Parity with the static build.** The upstream example
  (`cargo run -p nautilus-backtest --features examples --example engine-ema-cross` from the
  repo root) and the plug-in runner produce identical logs, after removing wall-clock
  timestamps, UUIDs, PID, and dependency-version banner lines.
- **Stale host binary.** Edit any engine source, rebuild only the runtime
  (`cargo build -p nautilus-rt`), then run the old `nautilus-backtest`. Result:
  `host binary was built against runtime <a>, but the loaded runtime is <b>; rebuild the host and runtime together`.
- **Stale plug-in.** Same edit, rebuild only the runner, then load the old plug-in. Result:
  `plug-in 'ema-cross' was built against runtime <a>, but the loaded runtime is <b>; rebuild the plug-in with the runtime`.
  The stale library loaded without error at the OS level, because symbol names didn't change.
  The fingerprint check is the only thing that caught it.
- **Wrong directory.** Building with `cargo build --manifest-path rt/Cargo.toml` from the repo
  root fails with:
  `nautilus-rt must be built with -C prefer-dynamic; run cargo from the rt directory ...`.

## How it works

### One engine, shared

The engine is compiled once into `libnautilus_rt.so` (`crate-type = ["dylib"]` in
`rt/runtime/Cargo.toml`). The host binary and every plug-in link it dynamically, together with
Rust's shared `libstd`. The engine's process-wide state is therefore a single copy: message bus,
actor registry, clocks, logger, and interned strings.

A plug-in is small: the example is 480 KB in release, against a 22 MB runtime. It contains
only its own code and the generic engine code it instantiates.

### Loading sequence

1. **Host build check.** `rt/backtest_runner/src/main.rs:66` calls
   `nautilus_rt::ensure_host_build(nautilus_rt::BUILD_FINGERPRINT)`. The constant is compiled
   into the host, and the runtime compares it with its own fingerprint.
2. **Config.** For each `[[strategies]]` entry, `PluginLoader::add_backtest_strategy`
   (`rt/runtime/src/loader.rs:131`) loads the library, or reuses it if already loaded.
3. **Verification.** `load_library` (`rt/runtime/src/loader.rs:156`) verifies in this order,
   each step trusting only what earlier steps established:
   1. The C-ABI `nautilus_plugin_init` manifest from upstream's `nautilus-plugin` crate: ABI
      version, plus model precision mode (`loader.rs:178`).
   2. The C-ABI `nautilus_rt_plugin_fingerprint`, which must equal the loaded runtime's
      fingerprint (`loader.rs:195`).
   3. Only then the Rust-ABI `NAUTILUS_RT_STRATEGY_PLUGIN` declaration (`loader.rs:205`).
4. **Registration.** The plug-in's generated `register` function deserializes the config with
   the factory. It then calls `StrategyRegistrar::add_strategy` (`rt/runtime/src/sdk.rs:80`),
   which calls `BacktestEngine::add_strategy` with the plug-in's concrete strategy type. No
   wrapper or dynamic dispatch sits on the hot path.
5. **Lifetime.** Libraries are never unloaded (`loader.rs:161`).

`strategy_plugin!` (`rt/runtime/src/sdk.rs:103`) generates all three exported symbols.

### Build fingerprint

The plug-in boundary passes Rust types directly. Rust has no stable ABI, so that is only sound
when the host, runtime, and plug-ins come from one build. `rt/runtime/build.rs` hashes (BLAKE3)
everything that defines "one build":

- `rustc -vV` output, target, profile, opt-level, debug, panic strategy, target features, and
  rustflags (`BUILD_ENV_KEYS`, `build.rs:31`).
- `rt/Cargo.lock`, `rt/Cargo.toml`, and the root `Cargo.toml` (feature and version specs of
  engine dependencies).
- Every `.rs` and `.toml` file under `crates/` (engine sources) and `rt/runtime/`.

The result is `nautilus_rt::BUILD_FINGERPRINT` (`rt/runtime/src/lib.rs:60`). A constant is
evaluated where it is used, so each plug-in and host records the fingerprint it was compiled
against. `runtime_fingerprint()` (`lib.rs:65`) reports the fingerprint of the library actually
loaded.

**In practice:** build the runtime, runner, and plug-ins together with one `cargo build` from
`rt/`, and ship them together.

### Why `-C prefer-dynamic` and the separate workspace

Cargo passes `-C prefer-dynamic` to a `dylib` only when building it as a dependency. When the
runtime itself is the package being built (`-p nautilus-rt`, or `--workspace`), cargo links
`std` statically into it. The host and plug-ins can then no longer link, and an already-built
host could load it with a second copy of `std` without the fingerprint changing.
`rt/.cargo/config.toml` always passes the flag, and `check_build_config` (`build.rs:100`)
refuses to build without it.

Keeping this in a separate workspace also lets `rt/` use the profiles a dylib needs (see
below) without touching upstream's.

## Performance

Measured on the 4-core development VM. Workload: the same EMA-cross strategy on 960,025
synthetic quotes, with a `NETTING` venue. Each variant ran 7 times, interleaved, and the table
shows the median.

| Variant                                                     | Quotes/s | vs today's static |
| ----------------------------------------------------------- | -------- | ----------------- |
| Static, fat LTO, `panic = "abort"` (upstream release)       | 423,979  | baseline          |
| Static, fat LTO, `panic = "unwind"`                         | 404,815  | −4.5%             |
| Static, no LTO, unwind (same flags as plug-in build)        | 340,707  | −19.6%            |
| **Plug-in, stable** (what this branch builds)               | 340,895  | −19.6%            |
| Plug-in, nightly, no LTO (control for the row below)        | 328,298  | −22.6%            |
| Plug-in, nightly, `-Zdylib-lto` fat LTO inside the runtime  | 362,710  | −14.5%            |

Rebuild after a strategy-only change (release): **2.5 s** for the plug-in, against
**1 min 47 s** for the static fat-LTO binary.

What this means:

- **The plug-in boundary itself costs 0–3%**: the plug-in row against the static row with the
  same flags. An earlier 7-round run measured 2.8%.
- **The cost is in build settings that a `dylib` forces.** Losing fat LTO costs about 14%.
  `panic = "unwind"`, required because Rust's shared `std` is built to unwind, costs about 5%.
  `rt/Cargo.toml` therefore uses `lto = false` and `panic = "unwind"` in release.
- **Nightly `-Zdylib-lto` recovers about 10% of that** (against its nightly control), but needs
  a nightly compiler for production builds. It also cannot inline across the shared `std`,
  which is part of what static fat LTO gains. Not adopted. To reproduce it, build from `rt/`
  with `cargo +nightly-2026-08-21 build --release --target-dir <dir>`, using a `RUSTC_WRAPPER`
  script that does two things. It appends `-C embed-bitcode=yes --cap-lints=warn` to every
  rustc call. When `--crate-name nautilus_rt` is present, it also adds
  `-C lto=fat -Zdylib-lto`. Note that nightly cargo places `libnautilus_rt.so` in
  `<dir>/release`, not `<dir>/release/deps`.
- **Benchmark with `NETTING`.** With `HEDGING`, every crossover opens a position that never
  nets off, and per-quote portfolio work grows with the open-position count. Long runs then
  slow down quadratically: 960k quotes ran at about 3,900 quotes/s.

The static comparison binaries came from a throwaway crate outside the repository. It was a
copy of the runner that links `nautilus_trading::examples::strategies::EmaCross` statically,
built with the root release profile and with the `rt` release profile.

## Rules and gotchas

- **Run cargo from `rt/`.** From anywhere else, `rt/.cargo/config.toml` is not read and the
  runtime build fails by design.
- **Rebuild and ship host, runtime, and plug-ins together.** Any mismatch is refused at
  startup. That is intended: it prevents undefined behavior.
- **Plug-ins depend only on `nautilus-rt`** (see the checklist above).
- **Relative plug-in paths resolve from the working directory**, not from the config file's
  location.
- **Libraries are loaded once at startup and never unloaded.** There is no mid-run swap, by
  design.
- **`sha256` is not supported.** Upstream's live `PluginConfig` has a `sha256` field, but
  `StrategyPluginConfig` does not, and rejects it as an unknown field.
- **Only Linux was tested.** The code uses portable APIs (`libloading`,
  `std::env::consts::DLL_PREFIX/SUFFIX`), but macOS and Windows have not been built.
- **The root repository's checks do not cover `rt/`.** `make pre-flight` and the root CI only
  see the root workspace. Run the commands under [Testing](#testing) yourself.
- **`rt/Cargo.lock` is separate from the root lockfile.** When pulling upstream, re-seed it so
  versions match the root again: `cp Cargo.lock rt/`, then run `cargo build` from `rt/` to prune
  it and add `rt`-only crates (currently just `libloading`). Then run the cooldown check.
  `scripts/check-cargo-cooldown.sh` (3-day crate age policy) checks every tracked lockfile,
  including `rt/Cargo.lock`. `bash scripts/check-cargo-cooldown.sh --lock rt/Cargo.lock --all`
  currently passes for all 190 crates. `libloading 0.8.9` is recorded in
  `.supply-chain/crate-dates.json`.
- **Upstream `[patch.crates-io]` entries are not mirrored yet.** The root patches `arrow` and
  `parquet`, which the current dependency graph doesn't use. Copy them into `rt/Cargo.toml`
  when adding the Parquet data catalog.

## Next steps

Roughly in priority order:

1. **Historical data.** Replace the synthetic scenario with the Parquet data catalog
   (`nautilus-persistence`): instruments and data from config. This needs the `arrow`/`parquet`
   patches above.
2. **Venue and engine config from TOML.** Multiple venues and instruments, fee and fill models,
   and `BacktestEngineConfig` options, instead of the fixed `SIM` venue.
3. **Parameter sweeps.** Run many configs in parallel, one engine per thread, writing results
   (`BacktestResult`) to disk. This is where the 2.5 s rebuild pays off.
4. **Packaging.** A release bundle (host, `libnautilus_rt.so`, `libstd-*.so`, plug-ins) with
   `-Wl,-rpath,$ORIGIN`, so no `LD_LIBRARY_PATH` is needed.
5. **Performance.** Decide whether the ~20% gap matters for your workloads. Options:
   - Accept it.
   - Adopt nightly `-Zdylib-lto`.
   - Add a "frozen" mode that statically links a chosen set of strategies with fat LTO, for
     long production sweeps.
6. **Live trading.** Implement upstream's stubbed `LiveNode` plug-in hooks
   (`crates/live/src/node/mod.rs`, `load_configured_plugins`/`add_plugin`) with the same loader.
7. **Remove Python.** The planned "soft rip": delete `python/` and `crates/pyo3`, keep the
   `cfg(feature = "python")` code dormant, and fail CI if `pyo3` appears in the dependency
   tree. A "hard rip" only if upstream merges become too painful.
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
