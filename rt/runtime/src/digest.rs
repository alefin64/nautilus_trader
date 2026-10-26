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

//! BLAKE3 digests of files and directory trees, for run and ingest manifests.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context;

/// Returns the BLAKE3 hex digest of the file at `path`.
///
/// # Errors
///
/// Returns an error if the file cannot be read.
pub fn file(path: &Path) -> anyhow::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to open '{}'", path.display()))?;
    std::io::copy(&mut file, &mut hasher)
        .with_context(|| format!("failed to hash '{}'", path.display()))?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Returns the BLAKE3 hex digest of every file under `dir`: their paths relative to `dir`
/// (with `/` separators) and contents, in sorted path order.
///
/// # Errors
///
/// Returns an error if the tree cannot be read.
pub fn tree(dir: &Path) -> anyhow::Result<String> {
    let mut files = Vec::new();
    collect(dir, &mut files).with_context(|| format!("failed to list '{}'", dir.display()))?;
    let mut entries = files
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(dir)
                .expect("file is under the directory")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            (relative, path)
        })
        .collect::<Vec<_>>();
    entries.sort();

    let mut hasher = blake3::Hasher::new();
    for (relative, path) in entries {
        let content =
            fs::read(&path).with_context(|| format!("failed to read '{}'", path.display()))?;
        for field in [relative.as_bytes(), &content] {
            hasher.update(&(field.len() as u64).to_le_bytes());
            hasher.update(field);
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tempfile::TempDir;

    use super::*;

    #[rstest]
    fn file_digest_matches_blake3() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, b"hello").unwrap();

        assert_eq!(
            file(&path).unwrap(),
            blake3::hash(b"hello").to_hex().to_string()
        );
    }

    #[rstest]
    fn tree_digest_covers_names_and_contents() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/a.txt"), b"a").unwrap();
        fs::write(dir.path().join("b.txt"), b"b").unwrap();
        let before = tree(dir.path()).unwrap();

        fs::rename(dir.path().join("b.txt"), dir.path().join("c.txt")).unwrap();
        let renamed = tree(dir.path()).unwrap();
        fs::write(dir.path().join("c.txt"), b"c").unwrap();
        let edited = tree(dir.path()).unwrap();

        assert_ne!(before, renamed);
        assert_ne!(renamed, edited);
        assert_eq!(edited, tree(dir.path()).unwrap());
    }
}
