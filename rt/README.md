# Strategy plug-in workspace

Compiled strategies loaded into the engine at startup, backtested on 5-minute bars from a
Nautilus Parquet data catalog. See `BULLPEN_HANDOVER.md` at the repository root for the full
guide.

| Path                         | Package                             | Output                                            |
| ---------------------------- | ----------------------------------- | ------------------------------------------------- |
| `runtime/`                   | `nautilus-rt`                       | `libnautilus_rt.so`: the shared engine and SDK    |
| `backtest_runner/`           | `nautilus-backtest-runner`          | `nautilus-backtest`: bar backtest runner          |
| `ingest/`                    | `nautilus-ingest`                   | `nautilus-ingest`: raw bars to per-split catalogs |
| `plugins/strategy/`          | `strategy-plugin`                   | `libstrategy_plugin.so`: the strategy slot        |
| `plugins/strategy-template/` | (not a package)                     | Pristine copy of the slot's `src/`                |
| `plugins/ema_cross/`         | `nautilus-example-ema-cross-plugin` | `libema_cross_plugin.so`: quote-based example     |
| `scripts/bundle.bash`        |                                     | Self-contained release bundle                     |

The engine is built once, as the Rust `dylib` `libnautilus_rt.so`. The binaries and every
plug-in link it dynamically, so the process has one copy of the engine's process-wide state:
message bus, actor registry, interned strings, logger.

## Build and run

Run cargo from this directory so it reads `.cargo/config.toml`:

```bash
cd rt
cargo build --release

# Raw bars -> one catalog per split (sessions.parquet comes from the calendar tooling)
cargo run --release --bin nautilus-ingest -- \
  --raw ../../bullpen-synth-data/out/SYNTH --sessions sessions.parquet \
  --profile rth --calendar XNYS --segments regular \
  --instrument-id SYNTH.XNYS --currency USD --price-precision 4 --bar-minutes 5 \
  --split train=2016-10-03..2022-12-31 --split validation=2023-01-01..2024-12-31 \
  --split holdout=2025-01-01..2026-09-30 --embargo-days 7 --output data/rth

# Backtest the strategy slot on one split
cargo run --release --bin nautilus-backtest -- --config train.toml --output out/train
```

`scripts/bundle.bash` builds in release and copies `nautilus-backtest`, `nautilus-ingest`,
`libnautilus_rt.so`, and the toolchain's `libstd-*.so` into `target/bundle/`. Everything carries
`RUNPATH $ORIGIN`, so the bundle runs from any directory without `LD_LIBRARY_PATH`. Binaries run
straight from `target/` still need the toolchain's `libstd` on the library path; `cargo run`
sets it.

## Writing a strategy

Edit only `plugins/strategy/src/`. Its `Cargo.toml` is frozen: a `dylib` named
`strategy_plugin` that depends on `nautilus-rt` alone (plus `rstest` for tests). `rt/Cargo.toml`
and `rt/Cargo.lock` are part of the build fingerprint, so a new dependency anywhere makes workers
refuse the plug-in.

1. Reach engine and third-party crates through `nautilus-rt`'s re-exports
   (`nautilus_rt::model`, `nautilus_rt::indicators`, `nautilus_rt::serde_json`, ...). If something
   is missing, add the re-export to `nautilus-rt`; never depend on a crate from the slot.
2. Serde derives need `#[serde(crate = "nautilus_rt::serde")]`.
3. Export the strategy as type `"Strategy"`:

   ```rust
   nautilus_rt::strategy_plugin! {
       name: "strategy",
       version: env!("CARGO_PKG_VERSION"),
       strategies: {
           "Strategy" => MyStrategy::from_json,
       },
   }
   ```

4. Test in-process with `nautilus_rt::sim::BarSimulation` on
   `nautilus_rt::synthetic::EquityScenario`, as `plugins/strategy/src/tests.rs` does.

To start over from the template: `rm -rf plugins/strategy/src && cp -r
plugins/strategy-template/src plugins/strategy/src`.

## Simulation model

`nautilus_rt::sim::BarSimulation` runs the backtest:

- **Fills.** Every order fills at the open of the next bar after it is sent, plus slippage in
  basis points of that bar's session segment, never on its own bar. Resting limit and stop
  orders are matched against bar opens only. An order sent on a session's last bar fills at the
  next session's first bar, gap included.
- **Costs.** Commission per share with a minimum per order. Borrow on short positions at each
  session close, for the calendar days to the next trading day.
- **Rules.** Orders are accepted only at a bar close. Orders that would short when shorting is
  off, or push the position past `min(max_leverage, max_position_pct_equity / 100)` times equity
  (valued at the last close, counting working orders), are denied.
- **Sessions.** Every bar must lie in a window of the sessions file. With `overnight = "flatten"`,
  working orders are cancelled and positions closed at each session's second-to-last bar close,
  so the exit fills on the last bar, and strategy orders are denied until the next session.

## Outputs

`nautilus-backtest` writes four files. The same inputs give byte-identical files.

| File              | Contents                                                                |
| ----------------- | ----------------------------------------------------------------------- |
| `results.parquet` | One row: PnL (net, gross), costs, return, Sharpe, drawdown, trades, ... |
| `trades.parquet`  | One row per fill (`record = "fill"`), then per position (`"position"`)  |
| `returns.parquet` | One row per trading day: close, position, equity, PnL, return, borrow   |
| `run.json`        | Config echo, build fingerprint, compiler, input and output digests      |

`nautilus-backtest --build-info` prints the engine version, build fingerprint, and compiler.

## Build identity

The plug-in boundary passes Rust types directly, which is only sound when the binaries, runtime,
and plug-ins come from one build. Build and ship them together from this workspace.

The runtime fingerprints its build: compiler version, target, profile settings, rustflags,
lockfiles and manifests, and the sources of the engine crates and runtime. At startup:

- A binary refuses to run if it was compiled against a different runtime.
- The loader refuses a plug-in whose `nautilus-plugin` manifest is invalid, or whose fingerprint
  differs from the loaded runtime's. It checks before reading any Rust-ABI symbol.

## Build configuration

- `-C prefer-dynamic` (from `.cargo/config.toml`): links `std` dynamically into every artifact.
  The runtime build fails without it.
- `-Wl,-rpath,$ORIGIN` on Linux (from `.cargo/config.toml`): binaries and libraries find their
  shared libraries in their own directory.
- Release builds use `lto = false` and `panic = "unwind"`. Fat LTO does not support `dylib`
  outputs, and the shared `std` library is built to unwind.
- `rt/Cargo.toml` mirrors the root `[patch.crates-io]` entries for `arrow` and `parquet`.
- Plug-ins are `dylib`s by convention. Inside this workspace a `cdylib` would also link the shared
  runtime and behave the same; what matters is building here and depending only on
  `nautilus-rt`.
- Loaded plug-ins are never unloaded.

## Limits

- One instrument per backtest, `NETTING` only.
- Only tested on Linux.
