//! The `amem` binary's main thread gets eight megabytes of stack on Windows, as it has on
//! Linux and macOS. Windows gives a main thread one by default, and what `main` does
//! before the commands move to a thread of their own (src/main.rs) overflowed that in a
//! debug build: `amem init` and `amem doctor` both died with "thread 'main' has
//! overflowed its stack" on the Windows runner.

fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" {
        if env == "msvc" {
            println!("cargo:rustc-link-arg-bins=/STACK:8388608");
        } else {
            println!("cargo:rustc-link-arg-bins=-Wl,--stack,8388608");
        }
    }
}
