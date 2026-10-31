//! Builds the browser reader into `OUT_DIR`, where `src/pages.rs` embeds it, so the wasm
//! the binary serves is always the one its sources build to. `reader_build.rs` does the
//! work; this says when it runs again and reports a failure as cargo errors.

mod reader_build;

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// What the reader compiles, relative to the workspace root: its crate, every path
/// dependency it has (`dm-core`, which has none), and what fixes their versions and
/// profiles. A change to anything else leaves the wasm as it is.
const SOURCES: [&str; 6] = [
    "crates/reader/Cargo.toml",
    "crates/reader/src",
    "crates/core/Cargo.toml",
    "crates/core/src",
    "Cargo.toml",
    "Cargo.lock",
];

/// What `wasm_bindgen_cli_support::Bindgen::new` reads from the environment.
const BINDGEN_ENV: [&str; 3] = [
    "WASM_BINDGEN_ANYREF",
    "WASM_BINDGEN_EXTERNREF",
    "WASM_BINDGEN_MULTI_VALUE",
];

fn var(name: &str) -> Result<PathBuf, String> {
    env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("cargo did not set {name} for the build script"))
}

fn run() -> Result<(), String> {
    let manifest = var("CARGO_MANIFEST_DIR")?;
    let workspace = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or("crates/web is not two levels under the workspace root")?;
    for path in SOURCES {
        println!("cargo::rerun-if-changed={}", workspace.join(path).display());
    }
    for name in BINDGEN_ENV {
        println!("cargo::rerun-if-env-changed={name}");
    }
    let out = var("OUT_DIR")?;
    reader_build::build(&reader_build::Build {
        workspace,
        cargo: &var("CARGO")?,
        rustc: &var("RUSTC")?,
        target_dir: &out.join("reader-target"),
        out: &out.join("reader"),
    })
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // the first line is what to do; the rest, a nested build's output, is detail
            let (first, rest) = e.split_once('\n').unwrap_or((&e, ""));
            println!("cargo::error={first}");
            eprintln!("{rest}");
            ExitCode::FAILURE
        }
    }
}
