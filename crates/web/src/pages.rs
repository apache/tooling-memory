//! The pages and the front-end libraries, served from the binary so the page works with
//! no internet: the harness has none, and the browser should not need any either.

/// The session list and the form that starts one.
pub const INDEX: &str = include_str!("../static/index.html");
/// A session: the terminal and the reading pane.
pub const SESSION: &str = include_str!("../static/session.html");
/// The browser reader: a bundle searched client-side with the server's own ranking.
pub const READER: &str = include_str!("../static/reader.html");

/// The reader's core, built from `crates/reader` by `make reader`: the JavaScript glue
/// and the WebAssembly it loads.
pub fn reader_asset(name: &str) -> Option<(&'static str, &'static [u8])> {
    Some(match name {
        "dm_reader.js" => (
            "text/javascript",
            include_bytes!("../static/reader/dm_reader.js"),
        ),
        "dm_reader_bg.wasm" => (
            "application/wasm",
            include_bytes!("../static/reader/dm_reader_bg.wasm"),
        ),
        _ => return None,
    })
}

/// A vendored library: xterm.js, its fit addon, marked and mermaid, MIT, and SQLite with
/// FTS5 for the browser, Apache-2.0 over public-domain SQLite; all pinned.
pub fn vendor(name: &str) -> Option<(&'static str, &'static [u8])> {
    Some(match name {
        "xterm.min.js" => (
            "text/javascript",
            include_bytes!("../static/vendor/xterm.min.js"),
        ),
        "xterm.min.css" => ("text/css", include_bytes!("../static/vendor/xterm.min.css")),
        "addon-fit.min.js" => (
            "text/javascript",
            include_bytes!("../static/vendor/addon-fit.min.js"),
        ),
        "marked.min.js" => (
            "text/javascript",
            include_bytes!("../static/vendor/marked.min.js"),
        ),
        "mermaid.min.js" => (
            "text/javascript",
            include_bytes!("../static/vendor/mermaid.min.js"),
        ),
        "sqlite3.mjs" => (
            "text/javascript",
            include_bytes!("../static/vendor/sqlite3.mjs"),
        ),
        "sqlite3.wasm" => (
            "application/wasm",
            include_bytes!("../static/vendor/sqlite3.wasm"),
        ),
        _ => return None,
    })
}
