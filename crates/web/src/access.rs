//! Who may talk to the server. It listens on loopback, which keeps other hosts out but
//! not other pages: any site the browser has open can aim a request or a websocket at
//! `127.0.0.1`, and a name that rebinds to loopback arrives with a foreign `Host`. So
//! every request names a loopback host, a request that carries an `Origin` comes from
//! this server's own origin or one the operator allowed, and everything past the static
//! pages carries the token the server was started with: in a cookie the landing link
//! sets, or as a bearer.

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

/// What the gate admits.
#[derive(Clone, Debug)]
pub struct Access {
    /// The secret every non-static request carries.
    pub token: String,
    /// Origins besides the server's own whose pages may call it, exactly as a browser
    /// sends them: `https://example.org`.
    pub origins: Vec<String>,
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

/// The cookie that carries the token, named for the port so two servers on one
/// machine, which share a cookie jar per host, do not overwrite each other's.
fn cookie_name(host: &str) -> String {
    let port = host.rsplit_once(':').map_or("80", |(_, p)| p);
    format!("amem-{port}")
}

fn cookie_token<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Equal in time that does not depend on where the first difference is.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// The static pages and their assets: nothing in them is the record's, and a page
/// that loads without the token can say how to get one.
fn is_static(path: &str) -> bool {
    path == "/"
        || path == "/reader"
        || path.starts_with("/s/")
        || path.starts_with("/vendor/")
        || path.starts_with("/reader/")
}

/// `token=<t>` out of a query, and the query without it.
fn split_token(query: &str) -> (Option<&str>, String) {
    let mut token = None;
    let rest: Vec<&str> = query
        .split('&')
        .filter(|kv| match kv.strip_prefix("token=") {
            Some(t) => {
                token = Some(t);
                false
            }
            None => !kv.is_empty(),
        })
        .collect();
    (token, rest.join("&"))
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
    let host = host.to_string();
    if let Some(origin) = headers.get(header::ORIGIN) {
        let ok = origin
            .to_str()
            .is_ok_and(|o| origin_allowed(&access, o, &host));
        if !ok {
            return refuse(StatusCode::FORBIDDEN, "a page from another origin");
        }
    }
    let name = cookie_name(&host);
    let path = req.uri().path().to_string();
    // the landing link: trade the token in the query for a cookie, and drop it from the
    // address bar so it is not left in history or shown on screen
    if let Some(query) = req.uri().query() {
        let (given, rest) = split_token(query);
        if let Some(given) = given {
            if !same(given, &access.token) {
                return refuse(
                    StatusCode::UNAUTHORIZED,
                    "that link's token is not this server's",
                );
            }
            let to = if rest.is_empty() {
                path
            } else {
                format!("{path}?{rest}")
            };
            let mut r = Redirect::to(&to).into_response();
            let cookie = format!("{name}={given}; Path=/; HttpOnly; SameSite=Strict");
            if let Ok(v) = HeaderValue::from_str(&cookie) {
                r.headers_mut().insert(header::SET_COOKIE, v);
            }
            return r;
        }
    }
    if !is_static(&path) {
        let given = bearer_token(headers).or_else(|| cookie_token(headers, &name));
        if !given.is_some_and(|t| same(t, &access.token)) {
            return refuse(
                StatusCode::UNAUTHORIZED,
                "no token: open the link `amem` printed when it started",
            );
        }
    }
    next.run(req).await
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
        let a = Access {
            token: "t".into(),
            origins: vec!["https://amem.example".into()],
        };
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
    fn the_token_comes_out_of_the_query_and_the_rest_stays() {
        assert_eq!(split_token("token=abc"), (Some("abc"), String::new()));
        assert_eq!(
            split_token("bundle=x&token=abc&y=1"),
            (Some("abc"), "bundle=x&y=1".into())
        );
        assert_eq!(split_token("bundle=x"), (None, "bundle=x".into()));
    }

    #[test]
    fn tokens_compare_whole() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "ab"));
        assert_eq!(new_token().unwrap().len(), 64);
        assert_ne!(new_token().unwrap(), new_token().unwrap());
    }
}
