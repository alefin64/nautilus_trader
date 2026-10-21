# Strategy plug-in workspace

Compiled strategies loaded into the engine at startup.

| Path                  | Package                             | Output                                |
| --------------------- | ----------------------------------- | ------------------------------------- |
| `runtime/`            | `nautilus-rt`                       | `libnautilus_rt.so`, the shared engine |
| `backtest_runner/`    | `nautilus-backtest-runner`          | `nautilus-backtest` host binary        |
| `plugins/ema_cross/`  | `nautilus-example-ema-cross-plugin` | `libema_cross_plugin.so`               |

The engine is built once, as the Rust `dylib` `libnautilus_rt.so`. The host binary and every
plug-in link it dynamically, so the process has one copy of the engine's process-wide state:
message bus, actor registry, interned strings, logger.

## Build and run

Run cargo from this directory so it reads `.cargo/config.toml`:

```bash
cd rt
cargo build
cargo run --bin nautilus-backtest -- --config plugins/ema_cross/backtest.toml
```

## Writing a strategy plug-in

1. Add a crate under `plugins/` and list it in `members` in `Cargo.toml`.
2. Set `crate-type = ["dylib"]`.
3. Depend on `nautilus-rt` only. Use engine and third-party crates through its re-exports
   (`nautilus_rt::model`, `nautilus_rt::serde`, ...). A direct dependency on a re-exported crate
   links a second copy of it.
4. Implement the strategy exactly as in the engine (`StrategyCore`, `nautilus_strategy!`,
   `DataActor`), then export it:

   ```rust
   nautilus_rt::strategy_plugin! {
       name: "ema-cross",
       version: env!("CARGO_PKG_VERSION"),
       strategies: {
           "EmaCross" => EmaCross::from_json,
       },
   }
   ```

   Each factory takes the instance's JSON config and returns the strategy.
5. Serde derives need `#[serde(crate = "nautilus_rt::serde")]`.

See `plugins/ema_cross` for a complete example.

## Build identity

The plug-in boundary passes Rust types directly, which is only sound when the host, runtime, and
plug-ins come from one build. Build and ship them together from this workspace.

The runtime fingerprints its build: compiler version, target, profile settings, rustflags,
lockfiles and manifests, and the sources of the engine crates and runtime. At startup:

- The host refuses to run if it was compiled against a different runtime.
- The loader refuses a plug-in whose `nautilus-plugin` manifest is invalid, or whose fingerprint
  differs from the loaded runtime's. It checks before reading any Rust-ABI symbol.

## Build configuration

- `-C prefer-dynamic` (from `.cargo/config.toml`): links `std` dynamically into every artifact.
  The runtime build fails without it.
- Release builds use `lto = false` and `panic = "unwind"`. Fat LTO does not support `dylib`
  outputs, and the shared `std` library is built to unwind.
- Executables need `libnautilus_rt.so` and Rust's `libstd-*.so` on the library path. `cargo run`
  sets it.
- Loaded plug-ins are never unloaded.

## Prototype limits

- The backtest runner uses a fixed simulated FX venue with synthetic AUD/USD quotes.
- Only tested on Linux.
