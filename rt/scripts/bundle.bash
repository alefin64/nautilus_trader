#!/usr/bin/env bash
# Builds the rt workspace in release and assembles a self-contained bundle:
#
#   nautilus-backtest     backtest runner
#   nautilus-ingest       raw bars -> per-split Parquet catalogs
#   libnautilus_rt.so     the shared engine runtime
#   libstd-<hash>.so      the toolchain's shared Rust standard library
#   BUILD_INFO.json       engine version, build fingerprint, and compiler
#   SHA256SUMS            checksums of the files above
#
# Every binary and library carries RUNPATH $ORIGIN (see rt/.cargo/config.toml), so the bundle
# runs from any directory without LD_LIBRARY_PATH. Strategy plug-ins must come from the same
# build: build them with `cargo build --release` in this workspace before or after bundling,
# without changing anything in between.
#
# Usage, from anywhere: rt/scripts/bundle.bash [OUTPUT_DIR]   (default: rt/target/bundle)

set -euo pipefail

rt_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$rt_dir"
out="$(realpath -m "${1:-target/bundle}")"

cargo build --release --workspace

libdir="$(rustc --print target-libdir)"
shopt -s nullglob
libstd=("$libdir"/libstd-*.so)
shopt -u nullglob
if [[ ${#libstd[@]} -ne 1 ]]; then
  echo "error: expected one libstd-*.so in $libdir, found ${#libstd[@]}" >&2
  exit 1
fi

rm -rf "$out"
mkdir -p "$out"
cp target/release/nautilus-backtest target/release/nautilus-ingest target/release/libnautilus_rt.so \
  "${libstd[0]}" "$out/"

# Fails here if the bundle cannot run on its own
env -u LD_LIBRARY_PATH "$out/nautilus-backtest" --build-info > "$out/BUILD_INFO.json"
env -u LD_LIBRARY_PATH "$out/nautilus-ingest" --help > /dev/null

(cd "$out" && sha256sum -- * > SHA256SUMS)
echo "Bundle written to $out"
cat "$out/BUILD_INFO.json"
