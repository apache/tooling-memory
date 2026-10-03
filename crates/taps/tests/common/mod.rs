//! Building a tap from the taps workspace for the host's tests.

// Each test binary uses what it needs of this module.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Build `package` from `taps/` for `wasm32-wasip2` and return the component's path.
pub fn build(package: &str) -> PathBuf {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let manifest = repo.join("taps/Cargo.toml");
    let out = std::process::Command::new(env!("CARGO"))
        .args([
            "build",
            "--manifest-path",
            manifest.to_str().unwrap(),
            "-p",
            package,
            "--target",
            "wasm32-wasip2",
            "--release",
        ])
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .expect("cargo runs");
    assert!(
        out.status.success(),
        "the tap did not build (is the wasm32-wasip2 target installed? rustup target add wasm32-wasip2):\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    repo.join(format!(
        "taps/target/wasm32-wasip2/release/{}.wasm",
        package.replace('-', "_")
    ))
}
