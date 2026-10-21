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

//! Checks the build configuration and computes the build fingerprint shared by the runtime
//! library and its plug-ins.
//!
//! Plug-ins exchange Rust types with the runtime across a Rust-ABI boundary, which is only
//! sound when both sides were compiled from identical sources with an identical toolchain and
//! configuration. The fingerprint hashes everything that can change type layout or symbol
//! meaning: compiler version, target, profile settings, rustflags, lockfile, manifests, and the
//! sources of the engine crates and this runtime.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

const BUILD_ENV_KEYS: &[&str] = &[
    "TARGET",
    "PROFILE",
    "OPT_LEVEL",
    "DEBUG",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_CFG_PANIC",
    "CARGO_CFG_TARGET_FEATURE",
];

fn main() {
    check_build_config();

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let rt_dir = manifest_dir
        .parent()
        .expect("runtime lives at <repo>/rt/runtime");
    let repo_dir = rt_dir.parent().expect("runtime lives at <repo>/rt/runtime");

    let files = [
        rt_dir.join("Cargo.lock"),
        rt_dir.join("Cargo.toml"),
        repo_dir.join("Cargo.toml"),
    ];
    let source_dirs = [repo_dir.join("crates"), manifest_dir.clone()];

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RUSTC");
    for path in files.iter().chain(&source_dirs) {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let mut hasher = blake3::Hasher::new();

    for key in BUILD_ENV_KEYS {
        println!("cargo:rerun-if-env-changed={key}");
        hash_field(
            &mut hasher,
            key,
            env::var(key).unwrap_or_default().as_bytes(),
        );
    }
    hash_field(&mut hasher, "rustc", rustc_version().as_bytes());

    let mut sources = files.to_vec();
    for dir in &source_dirs {
        collect_sources(dir, &mut sources);
    }
    sources.sort();
    sources.dedup();

    for path in sources {
        let relative = path
            .strip_prefix(repo_dir)
            .expect("source is under the repository")
            .to_string_lossy()
            .replace('\\', "/");
        hash_field(
            &mut hasher,
            &relative,
            &fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display())),
        );
    }

    let fingerprint = hasher.finalize().to_hex();
    println!("cargo:rustc-env=NAUTILUS_RT_FINGERPRINT={fingerprint}");
}

// Fails the build when the runtime would not share `std` with the host and plug-ins
fn check_build_config() {
    let rustflags = env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    let flags: Vec<&str> = rustflags.split('\x1f').collect();
    let prefer_dynamic = flags.contains(&"-Cprefer-dynamic")
        || flags
            .windows(2)
            .any(|pair| pair == ["-C", "prefer-dynamic"]);
    assert!(
        prefer_dynamic,
        "nautilus-rt must be built with `-C prefer-dynamic`; run cargo from the `rt` directory so it reads `rt/.cargo/config.toml`"
    );

    let panic = env::var("CARGO_CFG_PANIC").unwrap_or_default();
    assert!(
        panic == "unwind",
        "nautilus-rt requires `panic = \"unwind\"` to link the shared `std` library, found `{panic}`"
    );
}

fn hash_field(hasher: &mut blake3::Hasher, name: &str, value: &[u8]) {
    hasher.update(&(name.len() as u64).to_le_bytes());
    hasher.update(name.as_bytes());
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).expect("read source directory");
    for entry in entries {
        let path = entry.expect("read directory entry").path();
        if path.is_dir() {
            collect_sources(&path, out);
        } else if path
            .extension()
            .is_some_and(|ext| ext == "rs" || ext == "toml")
        {
            out.push(path);
        }
    }
}

fn rustc_version() -> String {
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let output = Command::new(rustc)
        .arg("-vV")
        .output()
        .expect("run rustc -vV");
    assert!(output.status.success(), "rustc -vV failed");
    String::from_utf8(output.stdout).expect("rustc -vV output is UTF-8")
}
