//! The browser reader, built from its sources: `dm-reader` compiled for
//! `wasm32-unknown-unknown` by a cargo of its own, then bound to JavaScript by
//! wasm-bindgen's library. `build.rs` runs this into `OUT_DIR`, where `src/pages.rs`
//! embeds the two files; a test in `src/pages.rs` runs it again and compares, so an embed
//! that does not follow its sources fails a test.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The target the reader is compiled for.
pub const TARGET: &str = "wasm32-unknown-unknown";
/// The JavaScript glue, as wasm-bindgen names it for `dm_reader.wasm`.
pub const GLUE: &str = "dm_reader.js";
/// The WebAssembly the glue loads.
pub const WASM: &str = "dm_reader_bg.wasm";

/// Rust flags and wrappers the outer build hands down, which are for its own target and
/// not for the wasm: coverage instrumentation, which wasm32 cannot link, and `cargo
/// clippy`, which would lint the reader a second time.
const NOT_INHERITED: [&str; 8] = [
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
    "CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET",
    "CARGO_BUILD_TARGET_DIR",
];

/// Where one build reads and writes.
pub struct Build<'a> {
    /// The workspace root: its `Cargo.toml` and `Cargo.lock`.
    pub workspace: &'a Path,
    /// The cargo that runs the nested build.
    pub cargo: &'a Path,
    /// The rustc asked whether the target is installed.
    pub rustc: &'a Path,
    /// The nested build's own target directory. Not the outer one: cargo holds that one's
    /// lock for the whole outer build, so a nested build there waits for ever.
    pub target_dir: &'a Path,
    /// Where the glue and the wasm are written.
    pub out: &'a Path,
}

/// Compile the reader and bind it, writing [`GLUE`] and [`WASM`] into `out`.
///
/// # Errors
///
/// When the target is not installed, when `Cargo.lock` binds the reader with another
/// wasm-bindgen than the library here, when the nested build fails, or when the bindgen
/// step fails or writes nothing. The first line says what to do.
pub fn build(b: &Build<'_>) -> Result<(), String> {
    let lock = fs::read_to_string(b.workspace.join("Cargo.lock"))
        .map_err(|e| format!("cannot read Cargo.lock: {e}"))?;
    same_bindgen(&lock)?;
    target_installed(b.rustc)?;
    let wasm = compile(b)?;
    bind(&wasm, b.out)
}

/// The versions of `name` that a `Cargo.lock` holds.
fn locked_versions<'a>(lock: &'a str, name: &str) -> Vec<&'a str> {
    let quoted = |line: &'a str, key: &str| {
        line.strip_prefix(key)
            .and_then(|v| v.trim().strip_prefix('='))
            .map(|v| v.trim().trim_matches('"'))
    };
    lock.split("[[package]]")
        .filter_map(|block| {
            let mut lines = block.lines().map(str::trim);
            let found = lines.clone().find_map(|l| quoted(l, "name"))?;
            (found == name)
                .then(|| lines.find_map(|l| quoted(l, "version")))
                .flatten()
        })
        .collect()
}

/// The reader's wasm-bindgen and the library that binds it are one release: the custom
/// section the macro writes is read by the library of the same version, and a mismatch is
/// not always caught by wasm-bindgen's own schema check.
fn same_bindgen(lock: &str) -> Result<(), String> {
    let used = locked_versions(lock, "wasm-bindgen");
    let binder = locked_versions(lock, "wasm-bindgen-cli-support");
    match binder.as_slice() {
        [v] if !used.is_empty() && used.iter().all(|u| u == v) => Ok(()),
        _ => Err(format!(
            "Cargo.lock has wasm-bindgen {used:?} for the reader and wasm-bindgen-cli-support \
             {binder:?} to bind it; they must be one version: change the pins in \
             crates/reader/Cargo.toml and crates/web/Cargo.toml together"
        )),
    }
}

/// The wasm target's standard library is there, or one line that says how to get it.
fn target_installed(rustc: &Path) -> Result<(), String> {
    let out = Command::new(rustc)
        .args(["--print", "target-libdir", "--target", TARGET])
        .output()
        .map_err(|e| format!("cannot run {}: {e}", rustc.display()))?;
    let libdir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let has_core = fs::read_dir(&libdir).is_ok_and(|mut entries| {
        entries.any(|e| e.is_ok_and(|e| e.file_name().to_string_lossy().starts_with("libcore-")))
    });
    if out.status.success() && has_core {
        Ok(())
    } else {
        Err(format!(
            "the browser reader is built for {TARGET}, which is not installed; run: rustup target add {TARGET}"
        ))
    }
}

/// `cargo llvm-cov` makes itself the rustc wrapper, which instruments every workspace
/// crate by name, `dm_core` among them, whatever the target. It keeps the wrapper it
/// replaced, if any, in a variable of its own; the nested build gets that one back.
fn without_coverage_wrapper(cmd: &mut Command) {
    if std::env::var_os("__CARGO_LLVM_COV_RUSTC_WRAPPER").is_none() {
        return;
    }
    match std::env::var_os("__CARGO_LLVM_COV_RUSTC_WRAPPER_PRE_EXISTING") {
        Some(wrapper) => cmd.env("RUSTC_WRAPPER", wrapper),
        None => cmd.env_remove("RUSTC_WRAPPER"),
    };
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("__CARGO_LLVM_COV") {
            cmd.env_remove(name);
        }
    }
}

/// `dm-reader` for the browser, in release, as `make wasm-check` checks it.
fn compile(b: &Build<'_>) -> Result<PathBuf, String> {
    let mut cmd = Command::new(b.cargo);
    cmd.args(["build", "--release", "--locked", "--color", "never"])
        .args(["-p", "dm-reader", "--target", TARGET])
        .arg("--manifest-path")
        .arg(b.workspace.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(b.target_dir);
    for name in NOT_INHERITED {
        cmd.env_remove(name);
    }
    without_coverage_wrapper(&mut cmd);
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run {}: {e}", b.cargo.display()))?;
    if !out.status.success() {
        return Err(format!(
            "building dm-reader for {TARGET} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(b.target_dir
        .join(TARGET)
        .join("release")
        .join("dm_reader.wasm"))
}

/// The glue and the wasm for `<script type="module">`, as `wasm-bindgen --target web
/// --no-typescript` writes them. A file left from an earlier run is removed first, so a
/// failure cannot leave it to be embedded.
fn bind(wasm: &Path, out: &Path) -> Result<(), String> {
    for name in [GLUE, WASM] {
        match fs::remove_file(out.join(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("cannot remove the old {name}: {e}"));
            }
            _ => {}
        }
    }
    let mut bindgen = wasm_bindgen_cli_support::Bindgen::new();
    bindgen
        .input_path(wasm)
        .web(true)
        .map_err(|e| format!("wasm-bindgen: {e:#}"))?
        .typescript(false)
        .omit_default_module_path(false)
        .generate(out)
        .map_err(|e| format!("wasm-bindgen could not bind {}: {e:#}", wasm.display()))?;
    for name in [GLUE, WASM] {
        let len = fs::metadata(out.join(name)).map_or(0, |m| m.len());
        if len == 0 {
            return Err(format!(
                "wasm-bindgen wrote no {name} into {}",
                out.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"
[[package]]
name = "wasm-bindgen"
version = "0.2.129"

[[package]]
name = "wasm-bindgen-cli-support"
version = "0.2.129"

[[package]]
name = "wasm-bindgen-shared"
version = "0.2.130"
"#;

    #[test]
    fn one_bindgen_release_passes_and_two_fail_with_what_to_change() {
        assert_eq!(locked_versions(LOCK, "wasm-bindgen"), ["0.2.129"]);
        assert_eq!(same_bindgen(LOCK), Ok(()));
        let skewed = LOCK.replacen("0.2.129", "0.2.130", 1);
        let err = same_bindgen(&skewed).unwrap_err();
        assert!(
            err.contains(r#"["0.2.130"]"#) && err.contains(r#"["0.2.129"]"#),
            "{err}"
        );
        assert!(err.contains("crates/reader/Cargo.toml"), "{err}");
        let unbound = LOCK.replace("wasm-bindgen-cli-support", "something-else");
        assert!(same_bindgen(&unbound).is_err());
    }

    #[test]
    fn a_rustc_that_cannot_run_is_named() {
        let err = target_installed(Path::new("/nonexistent/rustc")).unwrap_err();
        assert!(err.contains("/nonexistent/rustc"), "{err}");
    }
}
