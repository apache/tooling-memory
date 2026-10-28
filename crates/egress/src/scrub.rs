//! The injected credential kept out of what comes back. An upstream may echo a request's
//! headers, from a debug endpoint or in an error that quotes them, and what the egress
//! put on a request is the one thing the harness must never hold. So a response from a
//! destination the egress authenticates to has that value replaced, in its headers and
//! in its body as it streams through.

use aho_corasick::{AhoCorasick, MatchKind};
use axum::http::{HeaderMap, HeaderValue};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

/// What stands in for the credential.
pub const REDACTED: &str = "[credential removed by the egress]";

/// The shortest credential replaced on its own, apart from the scheme before it: a
/// shorter one matches by chance as often as by echo. The whole value is always
/// replaced, whatever its length, since it is exactly what was injected.
const SHORTEST: usize = 8;

/// What to replace for an injected header value: the value, and the credential in it
/// after a scheme such as `Bearer`, each also as JSON escapes a `/` in it and as a URL
/// percent-encodes it; longest first, so the whole value is replaced as one.
#[must_use]
pub fn needles(value: &str) -> Vec<Vec<u8>> {
    let mut out = vec![value.as_bytes().to_vec()];
    if let Some((_, credential)) = value.split_once(' ') {
        let credential = credential.trim().as_bytes();
        if credential.len() >= SHORTEST {
            out.push(credential.to_vec());
        }
    }
    let forms: Vec<Vec<u8>> = out.iter().flat_map(|n| encodings(n)).collect();
    out.extend(forms);
    out.retain(|n| !n.is_empty());
    out.sort_by_key(|n| std::cmp::Reverse(n.len()));
    out.dedup();
    out
}

/// `credential` with `/` escaped as JSON may write it, and percent-encoded with upper-
/// and lower-case hex.
fn encodings(credential: &[u8]) -> Vec<Vec<u8>> {
    let plain = |b: u8| b.is_ascii_alphanumeric() || b"._~-".contains(&b);
    let mut escaped = Vec::with_capacity(credential.len());
    for &b in credential {
        if b == b'/' {
            escaped.extend_from_slice(b"\\/");
        } else {
            escaped.push(b);
        }
    }
    let percent = |upper: bool| -> Vec<u8> {
        let mut out = Vec::with_capacity(credential.len());
        for &b in credential {
            if plain(b) {
                out.push(b);
            } else if upper {
                out.extend_from_slice(format!("%{b:02X}").as_bytes());
            } else {
                out.extend_from_slice(format!("%{b:02x}").as_bytes());
            }
        }
        out
    };
    vec![escaped, percent(true), percent(false)]
}

/// One automaton over every needle, leftmost-longest, so the whole value is replaced
/// before the credential within it. `None` for no needles. A set the automaton cannot
/// be built for is `Err`, and what is scanned with it is replaced whole.
fn matcher(needles: &[Vec<u8>]) -> Result<Option<AhoCorasick>, ()> {
    if needles.is_empty() {
        return Ok(None);
    }
    AhoCorasick::builder()
        .match_kind(MatchKind::LeftmostLongest)
        .build(needles)
        .map(Some)
        .map_err(|_| ())
}

fn replace_with(m: &Result<Option<AhoCorasick>, ()>, text: &[u8]) -> Vec<u8> {
    match m {
        Ok(None) => text.to_vec(),
        Ok(Some(ac)) => {
            let mut out = Vec::with_capacity(text.len());
            ac.replace_all_with_bytes(text, &mut out, |_, _, dst| {
                dst.extend_from_slice(REDACTED.as_bytes());
                true
            });
            out
        }
        Err(()) => REDACTED.as_bytes().to_vec(),
    }
}

/// `text` with every needle replaced.
#[must_use]
pub fn replace(text: &[u8], needles: &[Vec<u8>]) -> Vec<u8> {
    replace_with(&matcher(needles), text)
}

/// How many bytes at the end of `text` could be the start of a needle the next chunk
/// completes: those wait for it, and nothing else does, so a stream is not held back
/// where no credential could be forming.
fn pending(text: &[u8], needles: &[Vec<u8>]) -> usize {
    let longest = needles.iter().map(Vec::len).max().unwrap_or(0);
    (1..longest.min(text.len() + 1))
        .rev()
        .find(|&k| {
            let tail = &text[text.len() - k..];
            needles.iter().any(|n| n.starts_with(tail))
        })
        .unwrap_or(0)
}

/// Headers with every needle replaced; a value that cannot be rebuilt is dropped.
pub fn headers(headers: &mut HeaderMap, needles: &[Vec<u8>]) {
    if needles.is_empty() {
        return;
    }
    let names: Vec<_> = headers.keys().cloned().collect();
    for name in names {
        let values: Vec<HeaderValue> = headers.get_all(&name).iter().cloned().collect();
        if !values
            .iter()
            .any(|v| needles.iter().any(|n| contains(v.as_bytes(), n)))
        {
            continue;
        }
        headers.remove(&name);
        for v in values {
            if let Ok(v) = HeaderValue::from_bytes(&replace(v.as_bytes(), needles)) {
                headers.append(name.clone(), v);
            }
        }
    }
}

fn contains(text: &[u8], needle: &[u8]) -> bool {
    text.windows(needle.len()).any(|w| w == needle)
}

/// A body with every needle replaced as it streams: each chunk goes on at once, less
/// only a tail that could be a needle's start, which waits for the next chunk or the
/// end. With no needles the stream is as it came.
pub fn body<S>(
    stream: S,
    needles: Vec<Vec<u8>>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    struct State<S> {
        inner: std::pin::Pin<Box<S>>,
        matcher: Result<Option<AhoCorasick>, ()>,
        needles: Vec<Vec<u8>>,
        carry: Vec<u8>,
        done: bool,
    }
    let state = State {
        inner: Box::pin(stream),
        matcher: matcher(&needles),
        needles,
        carry: Vec::new(),
        done: false,
    };
    futures_util::stream::unfold(state, |mut s| async move {
        if s.done {
            return None;
        }
        loop {
            match s.inner.next().await {
                Some(Ok(chunk)) if s.needles.is_empty() => return Some((Ok(chunk), s)),
                // the usual chunk: nothing waiting, nothing in it, nothing starting at
                // its end; it goes on as it came, uncopied
                Some(Ok(chunk))
                    if s.carry.is_empty()
                        && matches!(&s.matcher, Ok(Some(ac)) if !ac.is_match(&chunk[..]))
                        && pending(&chunk, &s.needles) == 0 =>
                {
                    return Some((Ok(chunk), s));
                }
                Some(Ok(chunk)) => {
                    let mut text = std::mem::take(&mut s.carry);
                    text.extend_from_slice(&chunk);
                    let mut out = replace_with(&s.matcher, &text);
                    let keep = pending(&out, &s.needles);
                    s.carry = out.split_off(out.len() - keep);
                    if !out.is_empty() {
                        return Some((Ok(Bytes::from(out)), s));
                    }
                }
                Some(Err(e)) => {
                    s.done = true;
                    return Some((Err(e), s));
                }
                None => {
                    s.done = true;
                    let rest = std::mem::take(&mut s.carry);
                    return (!rest.is_empty()).then(|| (Ok(Bytes::from(rest)), s));
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn through(chunks: &[&str], value: &str) -> (String, usize) {
        let chunks: Vec<Result<Bytes, std::io::Error>> = chunks
            .iter()
            .map(|c| Ok(Bytes::copy_from_slice(c.as_bytes())))
            .collect();
        let out: Vec<_> = body(futures_util::stream::iter(chunks), needles(value))
            .collect()
            .await;
        let n = out.len();
        let text: Vec<u8> = out.into_iter().flat_map(|c| c.unwrap().to_vec()).collect();
        (String::from_utf8(text).unwrap(), n)
    }

    #[test]
    fn the_value_and_the_credential_after_its_scheme_are_needles() {
        let n = needles("Bearer ghs_abcdef123456");
        assert_eq!(n[0], b"Bearer%20ghs_abcdef123456".to_vec(), "longest first");
        assert!(n.contains(&b"Bearer ghs_abcdef123456".to_vec()));
        assert!(n.contains(&b"ghs_abcdef123456".to_vec()));
        assert_eq!(
            needles("sk-ant-api03-xyz"),
            vec![b"sk-ant-api03-xyz".to_vec()]
        );
        // too short to tell an echo from chance on its own
        assert!(needles("Bearer short").iter().all(|n| n != b"short"));
    }

    #[tokio::test]
    async fn an_echo_is_replaced_wherever_the_chunks_split_it() {
        let value = "Bearer ghs_abcdef123456";
        let body = "{\"headers\":{\"authorization\":\"Bearer ghs_abcdef123456\"},\"raw\":\"ghs_abcdef123456\"}";
        let want =
            format!("{{\"headers\":{{\"authorization\":\"{REDACTED}\"}},\"raw\":\"{REDACTED}\"}}");
        for at in 1..body.len() {
            let (got, _) = through(&[&body[..at], &body[at..]], value).await;
            assert_eq!(got, want, "split at {at}");
        }
        let bytes: Vec<String> = body.chars().map(String::from).collect();
        let bytes: Vec<&str> = bytes.iter().map(String::as_str).collect();
        assert_eq!(through(&bytes, value).await.0, want);
    }

    #[tokio::test]
    async fn a_stream_with_no_echo_goes_on_chunk_by_chunk() {
        let events = [
            "data: {\"a\":1}\n\n",
            "data: {\"b\":2}\n\n",
            "data: [DONE]\n\n",
        ];
        let (got, n) = through(&events, "Bearer ghs_abcdef123456").await;
        assert_eq!(got, events.concat());
        assert_eq!(n, 3, "nothing waited for a later chunk");
        // a tail that starts like the credential waits for the next chunk, and goes on
        // at the end when no chunk comes
        let value = "Bearer ghs_abcdef123456";
        assert_eq!(
            chunks(&["ends with Bear", "er x"], value).await,
            vec!["ends with ", "Bearer x"]
        );
        assert_eq!(
            chunks(&["ends with Bear"], value).await,
            vec!["ends with ", "Bear"]
        );
    }

    async fn chunks(chunks: &[&str], value: &str) -> Vec<String> {
        let chunks: Vec<Result<Bytes, std::io::Error>> = chunks
            .iter()
            .map(|c| Ok(Bytes::copy_from_slice(c.as_bytes())))
            .collect();
        body(futures_util::stream::iter(chunks), needles(value))
            .map(|c| String::from_utf8(c.unwrap().to_vec()).unwrap())
            .collect()
            .await
    }

    #[test]
    fn an_escaped_or_encoded_echo_is_replaced() {
        // a credential of base64's alphabet, as `openssl rand -base64` makes one
        let n = needles("Bearer abc/def+ghi=jk012");
        for echo in [
            r#"{"seen":"Bearer abc\/def+ghi=jk012"}"#,
            "seen=Bearer%20abc%2Fdef%2Bghi%3Djk012",
            "seen=Bearer%20abc%2fdef%2bghi%3djk012",
            r#"{"seen":"abc\/def+ghi=jk012"}"#,
        ] {
            let out = String::from_utf8(replace(echo.as_bytes(), &n)).unwrap();
            assert!(!out.contains("def"), "{echo} -> {out}");
            assert!(out.contains(REDACTED), "{echo} -> {out}");
        }
    }

    #[test]
    fn a_short_value_is_still_replaced_whole() {
        // too short to be a credential, but it is what was injected
        let n = needles("abc1234");
        assert_eq!(
            String::from_utf8(replace(b"key abc1234 echoed", &n)).unwrap(),
            format!("key {REDACTED} echoed")
        );
    }

    #[test]
    fn headers_are_rebuilt_without_the_value() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-echo",
            HeaderValue::from_static("got Bearer ghs_abcdef123456"),
        );
        h.insert("x-other", HeaderValue::from_static("plain"));
        headers(&mut h, &needles("Bearer ghs_abcdef123456"));
        assert_eq!(h["x-echo"], format!("got {REDACTED}").as_str());
        assert_eq!(h["x-other"], "plain");
    }
}
