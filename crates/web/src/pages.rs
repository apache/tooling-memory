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
/// The design tokens every page links: docs/design-book.html and docs/poster.html carry
/// the same file whole.
pub const TOKENS: &str = include_str!("../static/tokens.css");

/// The reader's core, built from `crates/reader` by this crate's build script: the
/// JavaScript glue and the WebAssembly it loads.
pub fn reader_asset(name: &str) -> Option<(&'static str, &'static [u8])> {
    Some(match name {
        "dm_reader.js" => (
            "text/javascript",
            include_bytes!(concat!(env!("OUT_DIR"), "/reader/dm_reader.js")),
        ),
        "dm_reader_bg.wasm" => (
            "application/wasm",
            include_bytes!(concat!(env!("OUT_DIR"), "/reader/dm_reader_bg.wasm")),
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

    /// The reader the binary serves is what `crates/reader` and `crates/core` build to now:
    /// the build script's work done again, against the sources as they are when the test
    /// runs, and compared byte for byte. The nested build uses the build script's
    /// target directory, so when nothing changed it compiles nothing.
    #[test]
    fn the_embedded_reader_is_what_its_sources_build_to() {
        use crate::reader_build::{build, nested_target_dir, Build, GLUE, WASM};
        use std::path::Path;
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace = manifest.parent().and_then(Path::parent).unwrap();
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let fresh = tempfile::tempdir().unwrap();
        build(&Build {
            workspace,
            cargo: Path::new(env!("CARGO")),
            rustc: Path::new(&rustc),
            target_dir: &nested_target_dir(Path::new(env!("OUT_DIR"))),
            out: fresh.path(),
        })
        .unwrap();
        for name in [GLUE, WASM] {
            let built = std::fs::read(fresh.path().join(name)).unwrap();
            let (_, served) = reader_asset(name).unwrap();
            assert!(
                served == built.as_slice(),
                "the embedded {name} is not what the reader's sources build to now: a source \
                 the reader compiles is missing from SOURCES in crates/web/build.rs"
            );
        }
    }

    #[test]
    fn the_book_inlines_the_tokens_and_components_whole() {
        let book = include_str!("../../../docs/design-book.html");
        assert!(
            book.contains(TOKENS),
            "docs/design-book.html must carry crates/web/static/tokens.css whole, as it is"
        );
    }

    #[test]
    fn the_poster_inlines_the_tokens_and_components_whole() {
        // docs/poster.html draws the paper's design in the tokens' colours and type, and
        // is opened from anywhere: a copy with nothing beside it must still render in them
        let poster = include_str!("../../../docs/poster.html");
        assert_eq!(
            poster.matches(TOKENS).count(),
            1,
            "docs/poster.html must carry crates/web/static/tokens.css whole, once, as it is"
        );
        let style = poster.find("<style>").expect("a style block");
        let copy = poster.find(TOKENS).unwrap();
        assert!(
            style < copy && copy < poster.find("</style>").unwrap(),
            "the tokens must be inside the poster's style block, not quoted elsewhere"
        );
        let before = &poster[..style];
        assert!(
            before.matches("<!--").count() == before.matches("-->").count(),
            "the poster's style block sits inside a comment"
        );
        // beyond that copy, the page names no colour of its own, defines no token of its
        // own, and fetches nothing
        let rest = poster.replacen(TOKENS, "", 1);
        assert!(!hex_colour(&rest), "the poster has a colour of its own");
        assert_eq!(
            named_colour_in_styles(&rest),
            None,
            "the poster names a colour"
        );
        assert!(
            !defines_a_token(&rest),
            "the poster defines a token of its own"
        );
        assert!(
            !rest.contains("<template"),
            "the poster hides markup in a template, where its styles would not apply"
        );
        assert_eq!(
            fetch_or_script(&rest),
            None,
            "the poster fetches or runs something"
        );
        assert_eq!(
            colour_in_an_attribute(&rest),
            None,
            "a presentation attribute names a colour"
        );
    }

    /// The first named colour in `page`'s own style blocks (with or without attributes
    /// on the tag) and style attributes, double or single quoted.
    fn named_colour_in_styles(page: &str) -> Option<&'static str> {
        let styles = page
            .split("<style")
            .skip(1)
            .filter_map(|s| s.split("</style>").next());
        let attributes = page
            .split("style=\"")
            .skip(1)
            .filter_map(|s| s.split('"').next());
        let quoted = page
            .split("style='")
            .skip(1)
            .filter_map(|s| s.split('\'').next());
        styles
            .chain(attributes)
            .chain(quoted)
            .find_map(named_colour)
    }

    /// The first thing in `page` that fetches, runs, or points off the page: a tag or
    /// attribute that loads a resource, a `url()` that is not a fragment, or a glyph
    /// reference to another document.
    fn fetch_or_script(page: &str) -> Option<String> {
        let lower = page.to_ascii_lowercase();
        let banned = [
            "<link", "<script", "<object", "<iframe", "<embed", "<image", "srcset", "src=",
            "@import",
        ];
        if let Some(b) = banned.iter().find(|b| lower.contains(*b)) {
            return Some((*b).to_string());
        }
        if let Some(u) = lower.split("url(").skip(1).find(|u| !u.starts_with('#')) {
            return Some(format!("url({}", &u[..u.len().min(40)]));
        }
        page.split("<use href=\"")
            .skip(1)
            .find(|u| !u.starts_with('#'))
            .map(|u| format!("<use href=\"{}", &u[..u.len().min(40)]))
    }

    /// The first `fill` or `stroke` presentation attribute whose value is a colour of its
    /// own rather than `none` or `currentColor`.
    fn colour_in_an_attribute(page: &str) -> Option<String> {
        ["fill=\"", "stroke=\""].iter().find_map(|attr| {
            page.split(attr)
                .skip(1)
                .filter_map(|s| s.split('"').next())
                .find(|v| *v != "none" && *v != "currentColor")
                .map(|v| format!("{attr}{v}"))
        })
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

    /// Whether `s` holds a hex colour: `#` and 3, 4, 6 or 8 hex digits, ending there,
    /// and not an anchor in an address.
    fn hex_colour(s: &str) -> bool {
        let b = s.as_bytes();
        (0..b.len()).any(|i| {
            if b[i] != b'#' || s[..i].ends_with("href=\"") || s[..i].ends_with("href='") {
                return false;
            }
            let n = b[i + 1..]
                .iter()
                .take_while(|c| c.is_ascii_alphanumeric())
                .count();
            matches!(n, 3 | 4 | 6 | 8) && b[i + 1..=i + n].iter().all(u8::is_ascii_hexdigit)
        })
    }

    /// A colour function or a colour by name, in CSS: a page's styles, or a style attribute.
    /// Whether `text` declares a custom property of the token family (`--am-x:`),
    /// with any spacing before the colon.
    fn defines_a_token(text: &str) -> bool {
        text.split("--am-").skip(1).any(|after| {
            let name_end = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .unwrap_or(after.len());
            after[name_end..].trim_start().starts_with(':')
        })
    }

    fn named_colour(css: &str) -> Option<&'static str> {
        const FUNCTIONS: [&str; 10] = [
            "rgb(",
            "rgba(",
            "hsl(",
            "hsla(",
            "hwb(",
            "lab(",
            "lch(",
            "oklab(",
            "oklch(",
            "color-mix(",
        ];
        const NAMES: [&str; 7] = ["white", "black", "gray", "grey", "red", "blue", "green"];
        // a name is a whole word of the CSS, not part of `border` or `required`
        FUNCTIONS.into_iter().find(|f| css.contains(f)).or_else(|| {
            css.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .find_map(|w| NAMES.into_iter().find(|n| *n == w))
        })
    }

    #[test]
    fn the_colour_check_sees_colours_and_not_anchors() {
        for colour in ["#abc;", "#abcd ", "#a0b1c2;", "#aabbccdd)", "color: #FFF;"] {
            assert!(hex_colour(colour), "{colour}");
        }
        for not in [
            "href=\"#bad\"",
            "#abcde;",
            "#ghi;",
            "issue #12",
            "#main-nav",
        ] {
            assert!(!hex_colour(not), "{not}");
        }
        for f in [
            "rgba(0,0,0,.5)",
            "hsla(1,2%,3%,.1)",
            "oklch(50% 0.1 20)",
            "color: red",
        ] {
            assert!(named_colour(f).is_some(), "{f}");
        }
        assert_eq!(named_colour("color: var(--am-ink)"), None);
        assert_eq!(
            named_colour("border: 1px solid var(--am-rule); required"),
            None
        );
    }

    #[test]
    fn the_pages_take_every_colour_from_the_tokens() {
        for (name, page) in PAGES {
            assert!(!hex_colour(page), "{name} has a colour of its own");
            // the page's own styles and every style attribute; the terminal's ANSI names
            // in its script are keys of the theme read from the tokens, not colours
            let styles = page
                .split("<style>")
                .skip(1)
                .filter_map(|s| s.split("</style>").next());
            let attributes = page
                .split("style=\"")
                .skip(1)
                .filter_map(|s| s.split('"').next());
            for css in styles.chain(attributes) {
                if let Some(f) = named_colour(css) {
                    panic!("{name} names a colour: {f}");
                }
            }
            for f in ["rgb(", "rgba(", "hsl(", "hsla(", "oklch(", "color-mix("] {
                assert!(
                    !page.contains(f),
                    "{name} builds a colour in its script: {f}"
                );
            }
        }
    }
}
