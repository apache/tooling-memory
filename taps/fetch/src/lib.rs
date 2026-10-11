//! The fetch tool tap: one web page, fetched for an agent and handed back as text.
//!
//! An agent calls it through the memory server mid-turn. It sends a GET to the URL,
//! follows redirects itself, each hop a fresh request, reads the body up to a cap, and
//! turns HTML into plain text: the title apart, scripts, styles and the head dropped,
//! block elements on lines of their own, list items as `- `, entities decoded. Plain
//! text, JSON and XML come back as they are; anything else is refused as not text.
//!
//! The tap decides nothing about where it may go. The host decides every request
//! before it leaves, the redirects included: the scheme, the method, the host against
//! the tool's grant, the address it resolves to, and the label of what the calling
//! session has read. A refused hop is an error naming the URL. Any credential the grant
//! names is put on the request by the host; the tap never holds it. What the tap
//! returns is scanned and kept by the memory server before the agent sees it.
//!
//! Config, JSON, all optional: `{"max_bytes": 2000000, "max_redirects": 5}`.
//! Args, JSON: `{"url": "https://..."}`.
//! Answer, JSON: `{"url", "final_url", "status", "content_type", "title", "text",
//! "truncated", "redirects"}`, where `redirects` lists the URLs followed to, in order.

wit_bindgen::generate!({
    path: "../../crates/taps/wit",
    world: "tool",
});

use serde::Deserialize;
use wasi::http::outgoing_handler;
use wasi::http::types::{Fields, Method, OutgoingRequest, Scheme};
use wasi::io::streams::StreamError;

const ACCEPT: &str = "text/html, text/plain;q=0.9, application/json;q=0.8, */*;q=0.1";
const USER_AGENT: &str = "durable-memory-fetch/0.1";

#[derive(Deserialize)]
struct Config {
    #[serde(default = "max_bytes")]
    max_bytes: usize,
    #[serde(default = "max_redirects")]
    max_redirects: usize,
}

fn max_bytes() -> usize {
    2_000_000
}

fn max_redirects() -> usize {
    5
}

#[derive(Deserialize)]
struct Args {
    url: String,
}

/// `scheme`, `authority` and `path?query` of an http or https URL; anything else is
/// refused here, before the host is asked.
fn split(url: &str) -> Result<(Scheme, String, String), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("{url:?} is not an absolute URL"))?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        other => return Err(format!("{other}: is not fetched; http and https only")),
    };
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

/// A response: status, headers, and the body up to the cap.
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    truncated: bool,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// GET one URL, reading at most `cap` bytes of its body.
fn get(url: &str, cap: usize) -> Result<Answer, String> {
    let (scheme, authority, path) = split(url)?;
    let headers = Fields::from_list(&[
        ("accept".to_string(), ACCEPT.as_bytes().to_vec()),
        ("user-agent".to_string(), USER_AGENT.as_bytes().to_vec()),
    ])
    .map_err(|e| format!("headers: {e}"))?;
    let req = OutgoingRequest::new(headers);
    req.set_method(&Method::Get).map_err(|()| "method")?;
    req.set_scheme(Some(&scheme)).map_err(|()| "scheme")?;
    req.set_authority(Some(&authority))
        .map_err(|()| "authority")?;
    req.set_path_with_query(Some(&path)).map_err(|()| "path")?;
    let pending =
        outgoing_handler::handle(req, None).map_err(|e| format!("request to {url}: {e}"))?;
    pending.subscribe().block();
    let response = pending
        .get()
        .ok_or("no response")?
        .map_err(|()| "response taken twice")?
        .map_err(|e| format!("request to {url}: {e}"))?;
    let status = response.status();
    let headers = response
        .headers()
        .entries()
        .into_iter()
        .map(|(k, v)| {
            (
                k.to_ascii_lowercase(),
                String::from_utf8_lossy(&v).into_owned(),
            )
        })
        .collect();
    let body = response.consume().map_err(|()| "body")?;
    let stream = body.stream().map_err(|()| "stream")?;
    let mut bytes = Vec::new();
    let mut truncated = false;
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => {
                bytes.extend(chunk);
                if bytes.len() > cap {
                    bytes.truncate(cap);
                    truncated = true;
                    break;
                }
            }
            Err(StreamError::Closed) => break,
            Err(StreamError::LastOperationFailed(e)) => {
                return Err(format!("reading {url}: {}", e.to_debug_string()))
            }
        }
    }
    drop(stream);
    Ok(Answer {
        status,
        headers,
        body: bytes,
        truncated,
    })
}

/// Where a `Location` points, from the URL that answered with it: absolute,
/// scheme-relative, root-relative, or relative to the answering URL's directory.
fn resolve(base: &str, location: &str) -> String {
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

/// A page's title and its text: what a reader would see, line by line.
fn html_to_text(html: &str) -> (String, String) {
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
        let next = html[i + 1..].find('<').map_or(html.len(), |p| i + 1 + p);
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

/// One entity's text, without its `&` and `;`, if it is one this tap knows.
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
enum Kind {
    Html,
    Text,
}

fn kind_of(content_type: &str) -> Option<Kind> {
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

/// GET the URL, following redirects up to the limit. Returns the final answer, the
/// URL it came from, and the URLs followed to.
fn follow(url: &str, config: &Config) -> Result<(Answer, String, Vec<String>), String> {
    let mut at = url.to_string();
    let mut redirects = Vec::new();
    loop {
        let answer = get(&at, config.max_bytes)?;
        if !matches!(answer.status, 301 | 302 | 303 | 307 | 308) {
            return Ok((answer, at, redirects));
        }
        let location = answer
            .header("location")
            .ok_or_else(|| format!("HTTP {} from {at} without a Location", answer.status))?;
        if redirects.len() == config.max_redirects {
            return Err(format!(
                "more than {} redirects from {url}",
                config.max_redirects
            ));
        }
        at = resolve(&at, location);
        redirects.push(at.clone());
    }
}

struct Component;

impl Guest for Component {
    fn call(config: String, args: String) -> Result<String, String> {
        let config: Config = if config.trim().is_empty() {
            serde_json::from_str("{}")
        } else {
            serde_json::from_str(&config)
        }
        .map_err(|e| format!("config: {e}"))?;
        let args: Args = serde_json::from_str(&args).map_err(|e| format!("args: {e}"))?;
        split(&args.url)?;
        let (answer, final_url, redirects) = follow(&args.url, &config)?;
        if answer.status >= 400 {
            return Err(format!("HTTP {} from {final_url}", answer.status));
        }
        let content_type = answer.header("content-type").unwrap_or("").to_string();
        let body = String::from_utf8_lossy(&answer.body);
        let (title, text) = match kind_of(&content_type) {
            Some(Kind::Html) => html_to_text(&body),
            Some(Kind::Text) => (String::new(), body.into_owned()),
            None => return Err(format!("not text: {content_type} at {final_url}")),
        };
        Ok(serde_json::json!({
            "url": args.url,
            "final_url": final_url,
            "status": answer.status,
            "content_type": content_type,
            "title": title,
            "text": text,
            "truncated": answer.truncated,
            "redirects": redirects,
        })
        .to_string())
    }
}

export!(Component);

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn html_becomes_lines_of_text_with_the_title_apart() {
        let html = "<html><head><title>T &amp; U</title><style>p{}</style></head>\
            <body><script>evil()</script><h1>Head</h1><p>One &lt;two&gt;&#33;</p>\
            <ul><li>a</li><li>b&nbsp;c</li></ul><!-- gone --><p>x < y</p></body></html>";
        let (title, text) = html_to_text(html);
        assert_eq!(title, "T & U");
        assert_eq!(text, "Head\n\nOne <two>!\n\n- a\n- b c\n\nx < y");
    }

    #[test]
    fn only_http_and_https_are_split() {
        assert!(split("ftp://x/y").is_err());
        assert!(split("file:///etc/passwd").is_err());
        let (_, authority, path) = split("https://h.example?q=1").unwrap();
        assert_eq!((authority.as_str(), path.as_str()), ("h.example", "/?q=1"));
    }
}
