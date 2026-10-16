//! What the fetch tool does with a page, apart from fetching it.
//!
//! Fetch runs in two hosts (PLAN M19): as a WASI component a launcher or a tool server
//! runs, and natively in the tool server, where the project's own trusted code needs no
//! runtime around it. The two make the same requests and give the same answer, so what
//! is not the request lives here, once: the configuration and the arguments, the URL
//! split into what a request is made of, where a `Location` points, HTML turned into
//! the text a reader would see, and the answer shaped from what came back. Neither host
//! decides where a request may go; a gate does, before it leaves.
//!
//! Config, JSON, all optional: `{"max_bytes": 2000000, "max_redirects": 5}`.
//! Args, JSON: `{"url": "https://..."}`.
//! Answer, JSON: `{"url", "final_url", "status", "content_type", "title", "text",
//! "truncated", "redirects"}`, where `redirects` lists the URLs followed to, in order.

use serde::Deserialize;

/// What a request asks for: HTML first, then text, then JSON.
pub const ACCEPT: &str = "text/html, text/plain;q=0.9, application/json;q=0.8, */*;q=0.1";
/// Who a request says it is.
pub const USER_AGENT: &str = "durable-memory-fetch/0.1";

/// The owner's configuration for the tool.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// How much of a body is read; the rest is dropped and the answer says so.
    #[serde(default = "max_bytes")]
    pub max_bytes: usize,
    /// How many redirects are followed before the call fails.
    #[serde(default = "max_redirects")]
    pub max_redirects: usize,
}

fn max_bytes() -> usize {
    2_000_000
}

fn max_redirects() -> usize {
    5
}

impl Default for Config {
    fn default() -> Config {
        Config {
            max_bytes: max_bytes(),
            max_redirects: max_redirects(),
        }
    }
}

impl Config {
    /// The configuration as the grant carries it: JSON, or nothing for the defaults.
    ///
    /// # Errors
    ///
    /// Text that is not the configuration's JSON.
    pub fn parse(text: &str) -> Result<Config, String> {
        if text.trim().is_empty() {
            return Ok(Config::default());
        }
        serde_json::from_str(text).map_err(|e| format!("config: {e}"))
    }
}

/// The agent's arguments.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Args {
    /// The page.
    pub url: String,
    /// Whether the page is to be rendered first, opened in the owner's browser and read
    /// as its scripts built it. Only a fetch the browser runs (`browser = true`) can;
    /// the other hosts refuse it rather than fetch the bytes and call them rendered.
    #[serde(default)]
    pub render: bool,
}

impl Args {
    /// The arguments as the agent sent them, the URL checked to be one that is fetched.
    ///
    /// # Errors
    ///
    /// Text that is not the arguments' JSON, or a URL that is not http or https.
    pub fn parse(text: &str) -> Result<Args, String> {
        let args: Args = serde_json::from_str(text).map_err(|e| format!("args: {e}"))?;
        split(&args.url)?;
        Ok(args)
    }
}

/// `scheme`, `authority` and `path?query` of an http or https URL, the scheme in lower
/// case and the fragment dropped; anything else is refused here, before a gate is asked.
///
/// # Errors
///
/// No scheme, a scheme other than http or https, or no host.
pub fn split(url: &str) -> Result<(String, String, String), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("{url:?} is not an absolute URL"))?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return Err(format!("{scheme}: is not fetched; http and https only"));
    }
    let rest = rest.split('#').next().unwrap_or("");
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) if rest.as_bytes()[i] == b'/' => (rest[..i].to_string(), rest[i..].to_string()),
        Some(i) => (rest[..i].to_string(), format!("/{}", &rest[i..])),
        None => (rest.to_string(), "/".to_string()),
    };
    if authority.is_empty() {
        return Err(format!("{url:?} has no host"));
    }
    Ok((scheme, authority, path))
}

/// Whether a status is a redirect the tool follows.
#[must_use]
pub fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Where a `Location` points, from the URL that answered with it: absolute,
/// scheme-relative, root-relative, or relative to the answering URL's directory.
#[must_use]
pub fn resolve(base: &str, location: &str) -> String {
    let location = location.trim();
    if location.contains("://") {
        return location.to_string();
    }
    let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
    if let Some(stripped) = location.strip_prefix("//") {
        return format!("{scheme}://{stripped}");
    }
    let rest = rest.split(['#', '?']).next().unwrap_or("");
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if location.starts_with('/') {
        return format!("{scheme}://{authority}{location}");
    }
    let dir = &path[..=path.rfind('/').unwrap_or(0)];
    format!("{scheme}://{authority}{dir}{location}")
}

/// Elements whose content is never text for a reader.
const SKIPPED: &[&str] = &["script", "style", "noscript", "svg", "template", "head"];

/// Elements that start a line of their own.
const BLOCKS: &[&str] = &[
    "p",
    "div",
    "br",
    "tr",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "section",
    "article",
    "pre",
    "blockquote",
    "table",
    "ul",
    "ol",
    "hr",
    "header",
    "footer",
    "nav",
    "main",
    "aside",
    "dl",
    "dt",
    "dd",
    "figure",
    "figcaption",
];

/// The tag name in the text between `<` and `>`, lowercased already, and whether it
/// closes.
fn tag_name(tag: &str) -> (bool, &str) {
    let (closing, rest) = match tag.strip_prefix('/') {
        Some(r) => (true, r),
        None => (false, tag),
    };
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(rest.len());
    (closing, &rest[..end])
}

/// The text of a page's `<title>`, decoded and on one line.
fn title_of(html: &str, lower: &str) -> String {
    let Some(start) = lower.find("<title") else {
        return String::new();
    };
    let Some(open_end) = lower[start..].find('>').map(|e| start + e + 1) else {
        return String::new();
    };
    let close = lower[open_end..]
        .find("</title")
        .map_or(html.len(), |e| open_end + e);
    decode_entities(&html[open_end..close])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Where scanning resumes after a skipped element that opened at `end` (its `>`).
fn past_closing(lower: &str, end: usize, name: &str) -> usize {
    let close = format!("</{name}");
    lower[end..].find(&close).map_or(lower.len(), |p| {
        let q = end + p;
        lower[q..].find('>').map_or(lower.len(), |g| q + g + 1)
    })
}

/// Whether the text at `i`, which starts with `<`, opens markup rather than being a
/// literal less-than sign.
fn is_markup(html: &str, i: usize) -> bool {
    html[i + 1..]
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!' || c == '?')
}

/// A page's title and its text: what a reader would see, line by line. The title
/// apart, scripts, styles and the head are dropped, block elements go on lines of
/// their own, list items as `- `, and entities are decoded.
#[must_use]
pub fn html_to_text(html: &str) -> (String, String) {
    let lower = html.to_ascii_lowercase();
    let title = title_of(html, &lower);
    let mut out = String::new();
    let mut pre = false;
    let mut i = 0;
    while i < html.len() {
        if html[i..].starts_with("<!--") {
            i = lower[i..].find("-->").map_or(html.len(), |e| i + e + 3);
            continue;
        }
        if html[i..].starts_with('<') && is_markup(html, i) {
            let Some(end) = html[i..].find('>').map(|e| i + e) else {
                break;
            };
            let (closing, name) = tag_name(&lower[i + 1..end]);
            if !closing && SKIPPED.contains(&name) && !lower[i..end].ends_with('/') {
                i = past_closing(&lower, end, name);
                continue;
            }
            if name == "pre" {
                pre = !closing;
            }
            if name == "li" && !closing {
                out.push_str("\n- ");
            } else if BLOCKS.contains(&name) {
                out.push('\n');
            }
            i = end + 1;
            continue;
        }
        // a run starts with any character, `¶` or `—` among them: step past it whole
        let step = html[i..].chars().next().map_or(1, char::len_utf8);
        let next = html[i + step..]
            .find('<')
            .map_or(html.len(), |p| i + step + p);
        let run = &html[i..next];
        if pre {
            out.push_str(run);
        } else {
            out.push_str(&run.replace(['\n', '\r', '\t'], " "));
        }
        i = next;
    }
    (title, tidy(&decode_entities(&out)))
}

/// Spaces collapsed within lines, lines trimmed, at most one blank line in a row.
fn tidy(text: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank += 1;
            if blank == 1 && !out.is_empty() {
                out.push('\n');
            }
            continue;
        }
        blank = 0;
        out.push_str(&line);
        out.push('\n');
    }
    out.trim().to_string()
}

/// One entity's text, without its `&` and `;`, if it is one this tool knows.
fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse().ok()?,
            };
            char::from_u32(code).map(|c| if c == '\u{a0}' { ' ' } else { c })
        }
    }
}

/// Entities decoded: the named few and every numeric one. Anything else is left as
/// written.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let decoded = after
            .find(';')
            .filter(|&semi| semi > 0 && semi <= 10)
            .and_then(|semi| entity(&after[..semi]).map(|c| (c, semi)));
        match decoded {
            Some((c, semi)) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// What a content type's main part says to do with the body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// HTML: made text, the title apart.
    Html,
    /// Plain text, JSON or XML: as it is.
    Text,
}

/// What to do with a body of this content type; `None` is not text and not answered.
#[must_use]
pub fn kind_of(content_type: &str) -> Option<Kind> {
    let main = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match main.as_str() {
        "text/html" | "application/xhtml+xml" => Some(Kind::Html),
        "application/json" | "application/xml" | "" => Some(Kind::Text),
        t if t.starts_with("text/") || t.ends_with("+json") || t.ends_with("+xml") => {
            Some(Kind::Text)
        }
        _ => None,
    }
}

/// What came back for a URL, after the redirects: what the answer is shaped from.
#[derive(Clone, Debug)]
pub struct Fetched<'a> {
    /// The URL asked for.
    pub url: &'a str,
    /// The URL that answered, after the redirects.
    pub final_url: &'a str,
    /// Its status.
    pub status: u16,
    /// Its `Content-Type`, or empty.
    pub content_type: &'a str,
    /// Its body, up to the cap.
    pub body: &'a [u8],
    /// Whether the body was cut at the cap.
    pub truncated: bool,
    /// The URLs followed to, in order.
    pub redirects: &'a [String],
}

/// The tool's answer, JSON, for what came back.
///
/// # Errors
///
/// A status of 400 or above, or a body that is not text.
pub fn answer(f: &Fetched<'_>) -> Result<String, String> {
    if f.status >= 400 {
        return Err(format!("HTTP {} from {}", f.status, f.final_url));
    }
    let body = String::from_utf8_lossy(f.body);
    let (title, text) = match kind_of(f.content_type) {
        Some(Kind::Html) => html_to_text(&body),
        Some(Kind::Text) => (String::new(), body.into_owned()),
        None => return Err(format!("not text: {} at {}", f.content_type, f.final_url)),
    };
    Ok(serde_json::json!({
        "url": f.url,
        "final_url": f.final_url,
        "status": f.status,
        "content_type": f.content_type,
        "title": title,
        "text": text,
        "truncated": f.truncated,
        "redirects": f.redirects,
    })
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_of_text_may_start_with_any_character() {
        let (_, text) = html_to_text("<p>¶ Pilcrow</p><p>—dash<b>é</b></p>");
        assert_eq!(text, "¶ Pilcrow\n\n—dashé");
    }

    #[test]
    fn locations_resolve_against_the_url_that_answered() {
        let base = "https://a.example/docs/page.html?x=1";
        assert_eq!(resolve(base, "https://b.example/"), "https://b.example/");
        assert_eq!(resolve(base, "//c.example/x"), "https://c.example/x");
        assert_eq!(resolve(base, "/root"), "https://a.example/root");
        assert_eq!(
            resolve(base, "other.html"),
            "https://a.example/docs/other.html"
        );
        assert_eq!(resolve("https://a.example", "x"), "https://a.example/x");
        assert!(is_redirect(302) && is_redirect(308) && !is_redirect(200) && !is_redirect(404));
    }

    #[test]
    fn html_becomes_lines_of_text_with_the_title_apart() {
        let html = "<html><head><title>T &amp; U</title><style>p{}</style></head>\
            <body><script>evil()</script><h1>Head</h1><p>One &lt;two&gt;&#33;</p>\
            <ul><li>a</li><li>b&nbsp;c</li></ul><!-- gone --><p>x < y</p>\
            <pre>kept\n  as is</pre><p>&#x41;&unknown;</p></body></html>";
        let (title, text) = html_to_text(html);
        assert_eq!(title, "T & U");
        assert_eq!(
            text,
            "Head\n\nOne <two>!\n\n- a\n- b c\n\nx < y\n\nkept\nas is\n\nA&unknown;"
        );
        let (title, text) = html_to_text("<head><title>unclosed");
        assert_eq!((title.as_str(), text.as_str()), ("unclosed", ""));
    }

    #[test]
    fn only_http_and_https_are_split() {
        assert!(split("ftp://x/y").is_err());
        assert!(split("file:///etc/passwd").is_err());
        assert!(split("no-scheme").is_err());
        let (scheme, authority, path) = split("HTTPS://h.example?q=1#frag").unwrap();
        assert_eq!(
            (scheme.as_str(), authority.as_str(), path.as_str()),
            ("https", "h.example", "/?q=1")
        );
        let (_, authority, path) = split("http://h.example:8080").unwrap();
        assert_eq!((authority.as_str(), path.as_str()), ("h.example:8080", "/"));
    }

    #[test]
    fn config_and_args_are_json_with_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert_eq!(Config::parse("{}").unwrap().max_bytes, 2_000_000);
        assert_eq!(
            Config::parse(r#"{"max_bytes": 10}"#).unwrap(),
            Config {
                max_bytes: 10,
                max_redirects: 5
            }
        );
        assert!(Config::parse("nope").unwrap_err().starts_with("config:"));
        assert_eq!(
            Args::parse(r#"{"url": "https://a.example/"}"#).unwrap().url,
            "https://a.example/"
        );
        assert!(Args::parse(r#"{"url": "ftp://a.example/"}"#).is_err());
        assert!(Args::parse(r#"{"page": 1}"#)
            .unwrap_err()
            .starts_with("args:"));
    }

    #[test]
    fn the_answer_is_text_or_a_refusal() {
        let redirects = vec!["https://a.example/final".to_string()];
        let page = Fetched {
            url: "https://a.example/",
            final_url: "https://a.example/final",
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: b"<head><title>T</title></head><p>Body</p>",
            truncated: true,
            redirects: &redirects,
        };
        let v: serde_json::Value = serde_json::from_str(&answer(&page).unwrap()).unwrap();
        assert_eq!(v["title"], "T");
        assert_eq!(v["text"], "Body");
        assert_eq!(v["truncated"], true);
        assert_eq!(v["redirects"][0], "https://a.example/final");
        let json = Fetched {
            content_type: "application/json",
            body: br#"{"a": 1}"#,
            ..page.clone()
        };
        let v: serde_json::Value = serde_json::from_str(&answer(&json).unwrap()).unwrap();
        assert_eq!(v["text"], r#"{"a": 1}"#);
        assert_eq!(v["title"], "");
        let image = Fetched {
            content_type: "image/png",
            ..page.clone()
        };
        assert_eq!(
            answer(&image).unwrap_err(),
            "not text: image/png at https://a.example/final"
        );
        let missing = Fetched {
            status: 404,
            ..page
        };
        assert_eq!(
            answer(&missing).unwrap_err(),
            "HTTP 404 from https://a.example/final"
        );
        assert_eq!(kind_of("application/ld+json"), Some(Kind::Text));
        assert_eq!(kind_of("Text/Plain; charset=x"), Some(Kind::Text));
        assert_eq!(kind_of("application/octet-stream"), None);
    }
}
