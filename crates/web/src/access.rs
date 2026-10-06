//! Who may talk to the server. It listens on loopback, which keeps other hosts out but
//! not other pages or other users of the machine: any site the browser has open can aim
//! a request or a websocket at `127.0.0.1`, a name that rebinds to loopback arrives with a
//! foreign `Host`, and anything another user runs can listen on a port nearby.
//!
//! So every request names a loopback host, a request that carries an `Origin` comes from
//! this server's own origin or one the operator allowed, and everything past the static
//! pages carries a key as a bearer: the server's token, which the command line holds, or a
//! browser's own key. A browser gets its key by redeeming a pairing code: single use, two
//! minutes, minted by the token's holder, so what passes through an opener's command line
//! is worth one pairing and soon nothing. The page keeps its key in its origin's storage,
//! which, unlike a cookie, is not shared with other ports on the same host. A websocket,
//! which cannot carry a header from a browser, offers its key as a subprotocol.
//!
//! A command line asks whether a server holds the token without sending it: the server
//! answers a nonce with an HMAC under the token, which only the holder can make.

use crate::state::{self, WebState};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a pairing code may wait for its browser.
pub const CODE_LIFETIME: Duration = Duration::from_secs(120);

/// The subprotocol a page's websocket asks for, beside the one carrying its key.
pub const PROTOCOL: &str = "amem";
const KEY_PROTOCOL: &str = "amem.key.";

/// What the gate admits, shared by every request.
#[derive(Clone, Debug)]
pub struct Access {
    token: String,
    /// Origins besides the server's own whose pages may call it, exactly as a browser
    /// sends them: `https://example.org`.
    origins: Vec<String>,
    /// Where paired browsers are remembered; `None` keeps them in memory only.
    root: Option<PathBuf>,
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    browsers: Vec<String>,
    codes: HashMap<String, Instant>,
}

impl Access {
    /// The gate for a server over `root`, with the token and paired browsers in `state`.
    #[must_use]
    pub fn new(root: Option<PathBuf>, state: &WebState, origins: Vec<String>) -> Access {
        Access {
            token: state.token.clone(),
            origins,
            root,
            inner: Arc::new(Mutex::new(Inner {
                browsers: state.browsers.clone(),
                codes: HashMap::new(),
            })),
        }
    }

    /// A gate on one token, nothing remembered: for tests.
    #[must_use]
    pub fn with_token(token: &str) -> Access {
        Access::new(
            None,
            &WebState {
                token: token.to_string(),
                ..WebState::default()
            },
            Vec::new(),
        )
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A pairing code: single use, good for [`CODE_LIFETIME`].
    ///
    /// # Errors
    ///
    /// When there is no randomness to make one.
    pub fn pairing_code(&self) -> Result<String, std::io::Error> {
        let code = random_hex::<16>()?;
        let mut i = self.inner();
        let now = Instant::now();
        i.codes
            .retain(|_, born| now.duration_since(*born) < CODE_LIFETIME);
        i.codes.insert(code.clone(), now);
        Ok(code)
    }

    /// Trade a code for a browser key, once; the key is remembered in the state file.
    fn redeem(&self, code: &str) -> Result<Option<String>, std::io::Error> {
        let mut i = self.inner();
        let Some(born) = i.codes.remove(code) else {
            return Ok(None);
        };
        if born.elapsed() >= CODE_LIFETIME {
            return Ok(None);
        }
        let key = random_hex::<32>()?;
        i.browsers.push(key.clone());
        if let Some(root) = &self.root {
            let (mut s, _) = state::load(root)?;
            if same(&s.token, &self.token) {
                s.browsers.clone_from(&i.browsers);
                state::save(root, &s)?;
            }
        }
        Ok(Some(key))
    }

    fn is_token(&self, key: &str) -> bool {
        same(key, &self.token)
    }

    fn admits(&self, key: &str) -> bool {
        self.is_token(key) || self.inner().browsers.iter().any(|b| same(b, key))
    }

    /// The answer to a nonce: proof of holding the token that does not reveal it.
    #[must_use]
    pub fn proof(&self, nonce: &str) -> String {
        proof(&self.token, nonce)
    }
}

/// HMAC-SHA256 of `amem-hello:<nonce>` under the token, hex.
#[must_use]
pub fn proof(token: &str, nonce: &str) -> String {
    hmac_sha256(token.as_bytes(), format!("amem-hello:{nonce}").as_bytes())
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// RFC 2104 over SHA-256.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |b: u8| k.iter().map(|x| x ^ b).collect::<Vec<u8>>();
    let mut inner = Sha256::new();
    inner.update(pad(0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(pad(0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// A fresh token: 32 random bytes, hex.
///
/// # Errors
///
/// When the operating system has no randomness to give.
pub fn new_token() -> Result<String, std::io::Error> {
    random_hex::<32>()
}

/// `N` random bytes, hex.
pub(crate) fn random_hex<const N: usize>() -> Result<String, std::io::Error> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// The host part of a `Host` header or an origin's authority, without the port.
fn host_only(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        // [::1]:4200
        return rest.split(']').next().unwrap_or_default();
    }
    authority.split(':').next().unwrap_or_default()
}

/// Whether a `Host` header names this machine's loopback, whatever the port.
fn loopback_host(host: &str) -> bool {
    matches!(
        host_only(host).to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// Whether an `Origin` may call: the server's own (the scheme and the authority the
/// request was sent to), or one the operator named.
fn origin_allowed(access: &Access, origin: &str, host: &str) -> bool {
    origin == format!("http://{host}") || access.origins.iter().any(|o| o == origin)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// The key a websocket offers among its subprotocols.
fn protocol_key(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)?
        .to_str()
        .ok()?
        .split(',')
        .find_map(|p| p.trim().strip_prefix(KEY_PROTOCOL))
}

/// Equal in time that does not depend on where the first difference is.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// What a path asks of the caller.
#[derive(Debug, PartialEq, Eq)]
enum Needs {
    /// The pages, their assets, the hello and the redeem: nothing of the record's.
    Nothing,
    /// Minting a pairing code: the token itself, not a browser's key.
    Token,
    /// Everything else: the token or a browser's key.
    Key,
}

fn needs(path: &str) -> Needs {
    if path == "/"
        || path == "/reader"
        || path == "/amem.js"
        || path == "/api/hello"
        || path == "/api/pair/redeem"
        || path.starts_with("/s/")
        || path.starts_with("/vendor/")
        || path.starts_with("/reader/")
    {
        Needs::Nothing
    } else if path == "/api/pair" {
        Needs::Token
    } else {
        Needs::Key
    }
}

fn refuse(status: StatusCode, why: &'static str) -> Response {
    (status, why).into_response()
}

/// The gate, in front of every route.
pub async fn gate(State(access): State<Access>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return refuse(StatusCode::BAD_REQUEST, "no Host");
    };
    if !loopback_host(host) {
        return refuse(
            StatusCode::FORBIDDEN,
            "this server answers to loopback names only",
        );
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        let ok = origin
            .to_str()
            .is_ok_and(|o| origin_allowed(&access, o, host));
        if !ok {
            return refuse(StatusCode::FORBIDDEN, "a page from another origin");
        }
    }
    let key = bearer(headers).or_else(|| protocol_key(headers));
    let ok = match needs(req.uri().path()) {
        Needs::Nothing => true,
        Needs::Token => key.is_some_and(|k| access.is_token(k)),
        Needs::Key => key.is_some_and(|k| access.admits(k)),
    };
    if !ok {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "not paired: run amem, which opens a link that pairs this browser",
        );
    }
    next.run(req).await
}

/// `POST /api/pair/redeem` with `{"code": ...}`: a browser key, or 401 for a code that
/// is unknown, used or stale.
pub(crate) fn redeem_response(access: &Access, code: &str) -> Response {
    match access.redeem(code) {
        Ok(Some(key)) => axum::Json(serde_json::json!({ "key": key })).into_response(),
        Ok(None) => refuse(
            StatusCode::UNAUTHORIZED,
            "that pairing link was used already or is more than two minutes old: run amem again",
        ),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_loopback_names_only() {
        for h in [
            "127.0.0.1:4200",
            "localhost:4200",
            "LOCALHOST",
            "[::1]:4200",
        ] {
            assert!(loopback_host(h), "{h}");
        }
        for h in [
            "evil.example:4200",
            "127.0.0.1.evil.example",
            "0.0.0.0:4200",
            "[::]:1",
        ] {
            assert!(!loopback_host(h), "{h}");
        }
    }

    #[test]
    fn an_origin_is_the_servers_own_or_named() {
        let mut a = Access::with_token("t");
        a.origins = vec!["https://amem.example".into()];
        assert!(origin_allowed(
            &a,
            "http://127.0.0.1:4200",
            "127.0.0.1:4200"
        ));
        assert!(origin_allowed(&a, "https://amem.example", "127.0.0.1:4200"));
        // another port on the same host is another origin
        assert!(!origin_allowed(
            &a,
            "http://127.0.0.1:3000",
            "127.0.0.1:4200"
        ));
        assert!(!origin_allowed(&a, "null", "127.0.0.1:4200"));
        assert!(!origin_allowed(
            &a,
            "https://amem.example.evil",
            "127.0.0.1:4200"
        ));
    }

    #[test]
    fn a_code_pairs_one_browser_once() {
        let a = Access::with_token(&"t".repeat(64));
        let code = a.pairing_code().unwrap();
        let key = a.redeem(&code).unwrap().unwrap();
        assert!(
            a.admits(&key) && !a.is_token(&key),
            "a browser key is not the token"
        );
        assert_eq!(a.redeem(&code).unwrap(), None, "a code is single use");
        assert_eq!(a.redeem("not-a-code").unwrap(), None);
        let other = a.pairing_code().unwrap();
        assert_ne!(other, code);
        // a stale code is refused
        a.inner()
            .codes
            .insert("stale".into(), Instant::now() - CODE_LIFETIME);
        assert_eq!(a.redeem("stale").unwrap(), None);
    }

    #[test]
    fn a_paired_browser_is_remembered_in_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let (s, _) = state::load(dir.path()).unwrap();
        state::save(dir.path(), &s).unwrap();
        let a = Access::new(Some(dir.path().to_path_buf()), &s, vec![]);
        let key = a.redeem(&a.pairing_code().unwrap()).unwrap().unwrap();
        let (again, _) = state::load(dir.path()).unwrap();
        assert_eq!(again.browsers, vec![key.clone()]);
        assert!(
            Access::new(None, &again, vec![]).admits(&key),
            "a restart keeps it"
        );
    }

    #[test]
    fn the_proof_needs_the_token_and_the_nonce() {
        let p = proof("token-a", "n1");
        assert_eq!(p.len(), 64);
        assert_eq!(p, proof("token-a", "n1"));
        assert_ne!(p, proof("token-b", "n1"));
        assert_ne!(p, proof("token-a", "n2"));
        assert!(!p.contains("token-a"));
        // RFC 4231, test cases 2 and 6: a short key, and one longer than the block
        let hex = |b: [u8; 32]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(
            hex(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn keys_come_from_the_bearer_or_a_subprotocol_and_paths_ask_for_what_they_need() {
        let mut h = HeaderMap::new();
        h.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            "amem, amem.key.k1".parse().unwrap(),
        );
        assert_eq!(protocol_key(&h), Some("k1"));
        h.insert(header::AUTHORIZATION, "Bearer k2".parse().unwrap());
        assert_eq!(bearer(&h), Some("k2"));
        assert_eq!(needs("/"), Needs::Nothing);
        assert_eq!(needs("/api/pair/redeem"), Needs::Nothing);
        assert_eq!(needs("/api/pair"), Needs::Token);
        assert_eq!(needs("/api/sessions"), Needs::Key);
        assert_eq!(needs("/ws/abc"), Needs::Key);
        assert_eq!(needs("/bundle/d/l/manifest.yaml"), Needs::Key);
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
        assert_eq!(new_token().unwrap().len(), 64);
    }
}
