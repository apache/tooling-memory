//! The pages and the front-end libraries, served from the binary so the page works with
//! no internet: the harness has none, and the browser should not need any either.

/// The session list and the form that starts one.
pub const INDEX: &str = include_str!("../static/index.html");
/// A session: the terminal and the reading pane.
pub const SESSION: &str = include_str!("../static/session.html");
/// The browser reader: a bundle searched client-side with the server's own ranking.
pub const READER: &str = include_str!("../static/reader.html");
/// The pages' key to the server: pairing, and the bearer on every call.
pub const AMEM_JS: &str = include_str!("../static/amem.js");
/// The board: tasks by status, each under the task it was filed from.
pub const BOARD: &str = include_str!("../static/board.html");
/// The design tokens every page links: docs/design-book.html holds the same values.
pub const TOKENS: &str = include_str!("../static/tokens.css");

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

/// A vendored library: xterm.js, its fit addon, marked and mermaid, MIT, DOMPurify,
/// Apache-2.0 or MPL-2.0, and SQLite with FTS5 for the browser, Apache-2.0 over
/// public-domain SQLite; all pinned.
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
        "purify.min.js" => (
            "text/javascript",
            include_bytes!("../static/vendor/purify.min.js"),
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

#[cfg(test)]
mod tests {
    use super::*;

    const PAGES: [(&str, &str); 4] = [
        ("index.html", INDEX),
        ("session.html", SESSION),
        ("board.html", BOARD),
        ("reader.html", READER),
    ];

    #[test]
    fn the_book_inlines_the_tokens_and_components_whole() {
        let book = include_str!("../../../docs/design-book.html");
        assert!(
            book.contains(TOKENS),
            "docs/design-book.html must carry crates/web/static/tokens.css whole, as it is"
        );
    }

    #[test]
    fn every_path_a_page_names_is_relative() {
        // the same files are served by amem and as a hosted copy on another origin, so a
        // leading slash would reach the host and not the page's own directory
        for (name, page) in PAGES {
            for attr in ["href=\"/", "src=\"/", "href='/", "src='/"] {
                assert!(
                    !page.contains(attr),
                    "{name} names a path from the root: {attr}"
                );
            }
        }
    }

    #[test]
    fn the_pages_take_every_colour_from_the_tokens() {
        let hex = |s: &str| {
            s.as_bytes().windows(5).any(|w| {
                w[0] == b'#'
                    && w[1..4].iter().all(u8::is_ascii_hexdigit)
                    && !w[4].is_ascii_alphanumeric()
            }) || s.as_bytes().windows(8).any(|w| {
                w[0] == b'#'
                    && w[1..7].iter().all(u8::is_ascii_hexdigit)
                    && !w[7].is_ascii_alphanumeric()
            })
        };
        for (name, page) in PAGES {
            assert!(!hex(page), "{name} has a colour of its own");
            // a colour by name, in the page's own styles; the terminal's ANSI names are
            // keys of the theme read from the tokens, not colours
            let styles = page
                .split("<style>")
                .skip(1)
                .filter_map(|s| s.split("</style>").next());
            for css in styles {
                for f in ["rgb(", "hsl(", "white", "black", "gray", "grey"] {
                    assert!(!css.contains(f), "{name} names a colour: {f}");
                }
            }
        }
    }
}
