//! What MCP 2026-07-28 asks of a request and a result, for the hand-written client and
//! server in this binary (the hook's client, the hosted-tool client, `amem tool-server`):
//! no handshake, so every request names its protocol version and the client's
//! capabilities in `_meta`; over streamable HTTP it mirrors its method, and the name it
//! calls, into headers; every result says what kind it is in `resultType`.
//!
//! The revisions before it opened a session with `initialize`; each piece here still
//! speaks that for a counterpart that knows nothing newer.

use base64::Engine as _;
use serde_json::{json, Value};

/// The revision without a handshake, which every request names in its `_meta`.
pub const MODERN: &str = "2026-07-28";

/// The revisions with an `initialize` handshake still answered or spoken, newest first.
pub const LEGACY: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];

/// The `_meta` key naming a request's protocol version.
pub const PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
/// The `_meta` key carrying the client's capabilities for a request.
pub const CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
/// The `_meta` key naming the client.
pub const CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
/// The `_meta` key naming the server on a result.
pub const SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// The header mirroring the request's protocol version.
pub const VERSION_HEADER: &str = "mcp-protocol-version";
/// The header mirroring the request's method.
pub const METHOD_HEADER: &str = "mcp-method";
/// The header mirroring the tool, prompt, or resource a request names.
pub const NAME_HEADER: &str = "mcp-name";

/// The headers do not match the body, or one the request needs is missing.
pub const HEADER_MISMATCH: i64 = -32020;
/// The request names a protocol version the server does not speak.
pub const UNSUPPORTED_VERSION: i64 = -32022;
/// The errors only a server of 2026-07-28 or later answers with; a client that sees
/// one is talking to such a server and does not fall back to the handshake.
pub const MODERN_ERRORS: [i64; 3] = [HEADER_MISMATCH, -32021, UNSUPPORTED_VERSION];

/// Every version spoken, newest first.
#[must_use]
pub fn supported() -> Vec<&'static str> {
    std::iter::once(MODERN).chain(LEGACY).collect()
}

/// The methods whose request names a tool, prompt, or resource, which goes in the
/// `Mcp-Name` header, and the parameter it is read from.
#[must_use]
pub fn named_by(method: &str) -> Option<&'static str> {
    match method {
        "tools/call" | "prompts/get" => Some("name"),
        "resources/read" => Some("uri"),
        _ => None,
    }
}

/// A 2026-07-28 request from the client `client`: `params` with the `_meta` every
/// request carries. This client asks for no optional capability.
#[must_use]
pub fn request(id: u64, method: &str, mut params: Value, client: &str) -> Value {
    if !params.is_object() {
        params = json!({});
    }
    params["_meta"] = json!({
        PROTOCOL_VERSION: MODERN,
        CLIENT_CAPABILITIES: {},
        CLIENT_INFO: { "name": client, "version": env!("CARGO_PKG_VERSION") },
    });
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// The headers a streamable HTTP POST of `request` carries: its version, its method,
/// and, for a method that names something, that name. Only the name may go encoded
/// ([`encode`]); 2026-07-28 allows that for `Mcp-Name` and `Mcp-Param-*` alone, and a
/// server compares `Mcp-Method` with the body as sent.
#[must_use]
pub fn headers(request: &Value) -> Vec<(&'static str, String)> {
    let method = request["method"].as_str().unwrap_or_default();
    let mut out = vec![
        (VERSION_HEADER, MODERN.to_string()),
        (METHOD_HEADER, method.to_string()),
    ];
    if let Some(key) = named_by(method) {
        if let Some(name) = request["params"][key].as_str() {
            out.push((NAME_HEADER, encode(name)));
        }
    }
    out
}

const SENTINEL: (&str, &str) = ("=?base64?", "?=");

/// A value as a header carries it: as is when it is visible ASCII with no space at
/// either end, else its UTF-8 in base64 between `=?base64?` and `?=`, as is a value
/// that already looks like that.
#[must_use]
pub fn encode(value: &str) -> String {
    let plain = !value.is_empty()
        && value.bytes().all(|b| (0x20..=0x7e).contains(&b))
        && !value.starts_with(' ')
        && !value.ends_with(' ')
        && !(value.starts_with(SENTINEL.0) && value.ends_with(SENTINEL.1));
    if plain {
        value.to_string()
    } else {
        let b64 = base64::engine::general_purpose::STANDARD.encode(value);
        format!("{}{b64}{}", SENTINEL.0, SENTINEL.1)
    }
}

/// A header value as [`encode`] wrote it, decoded; `None` when the base64 in it does
/// not decode to UTF-8.
#[must_use]
pub fn decode(header: &str) -> Option<String> {
    match header
        .strip_prefix(SENTINEL.0)
        .and_then(|h| h.strip_suffix(SENTINEL.1))
    {
        Some(b64) => base64::engine::general_purpose::STANDARD
            .decode(b64)
            .ok()
            .and_then(|b| String::from_utf8(b).ok()),
        None => Some(header.to_string()),
    }
}

/// Whether a result is a final one. A server before 2026-07-28 sends no `resultType`,
/// which counts as complete; a kind this client does not take is refused.
///
/// # Errors
///
/// A `resultType` other than `complete`, named.
pub fn complete(result: &Value) -> Result<(), String> {
    match result.get("resultType") {
        None => Ok(()),
        Some(Value::String(t)) if t == "complete" => Ok(()),
        Some(other) => Err(format!(
            "the server answered a {other} result, and only a complete one is taken"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_carries_its_version_and_capabilities_in_meta_and_headers() {
        let r = request(
            7,
            "tools/call",
            json!({"name": "get", "arguments": {}}),
            "t",
        );
        assert_eq!(r["params"]["_meta"][PROTOCOL_VERSION], MODERN);
        assert!(r["params"]["_meta"][CLIENT_CAPABILITIES].is_object());
        assert_eq!(r["params"]["_meta"][CLIENT_INFO]["name"], "t");
        assert_eq!(r["params"]["name"], "get");
        assert_eq!(
            headers(&r),
            vec![
                (VERSION_HEADER, MODERN.to_string()),
                (METHOD_HEADER, "tools/call".to_string()),
                (NAME_HEADER, "get".to_string()),
            ]
        );
        let d = request(1, "server/discover", Value::Null, "t");
        assert_eq!(d["params"]["_meta"][PROTOCOL_VERSION], MODERN);
        assert_eq!(headers(&d).len(), 2);
    }

    /// 2026-07-28 lets the sentinel stand for a value in `Mcp-Name` and `Mcp-Param-*`
    /// only; `Mcp-Method` carries the method as it is, even one that looks like it.
    #[test]
    fn the_method_header_carries_the_method_as_it_is() {
        let looks_encoded = "=?base64?dG9vbHMvbGlzdA==?=";
        let r = request(1, looks_encoded, Value::Null, "t");
        assert_eq!(headers(&r)[1], (METHOD_HEADER, looks_encoded.to_string()));
    }

    #[test]
    fn a_value_unsafe_in_a_header_goes_as_base64_and_comes_back() {
        assert_eq!(encode("fetch"), "fetch");
        for v in [
            "Hello, 世界",
            " padded ",
            "line1\nline2",
            "=?base64?literal?=",
            "",
        ] {
            let e = encode(v);
            assert!(
                e.starts_with("=?base64?") && e.ends_with("?="),
                "{v:?}: {e}"
            );
            assert_eq!(decode(&e).as_deref(), Some(v));
        }
        assert_eq!(encode("Hello, 世界"), "=?base64?SGVsbG8sIOS4lueVjA==?=");
        assert_eq!(decode("plain").as_deref(), Some("plain"));
        assert_eq!(decode("=?base64?!!?="), None);
    }

    #[test]
    fn a_result_without_a_kind_is_complete_and_an_unknown_kind_is_refused() {
        assert!(complete(&json!({"content": []})).is_ok());
        assert!(complete(&json!({"resultType": "complete"})).is_ok());
        let e = complete(&json!({"resultType": "input_required"})).unwrap_err();
        assert!(e.contains("input_required"), "{e}");
        assert!(complete(&json!({"resultType": 3})).is_err());
    }
}
