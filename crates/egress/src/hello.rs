//! The first bytes of a tunnel: the client's TLS ClientHello, read before anything is
//! dialled, and the server name in it checked against the host the CONNECT was allowed
//! for.
//!
//! The allowlist is checked against the CONNECT's host, but a shared front end (a CDN,
//! a cloud's storage or user-content hosts) routes on the server name in the
//! ClientHello, which the client picks. Without this check a client could CONNECT to an
//! allowed name and ask the front end behind it for another tenant. So the hello must
//! name the CONNECT host, in the clear, and nothing else: one with another name, with
//! two, or with none is refused, and so is one that hides its name with Encrypted
//! Client Hello or one of its drafts, whose name in the clear is a decoy. A CONNECT to
//! an address is the one case where a hello with no name passes, since a client sends
//! none for an address.
//!
//! What is checked: the first ClientHello, before the dial, which must end with the
//! record that carries it; then the upstream's ServerHello, sent on to the client as it
//! comes, and when that is a HelloRetryRequest, the second ClientHello, after an
//! optional ChangeCipherSpec, held to the same rule before it goes upstream. Nothing the
//! client sends after its first hello goes upstream until the ServerHello is read.
//! After that the bytes are copied both ways unread.
//!
//! What is not: the egress cannot see inside TLS. A front end that still routes on the
//! HTTP Host header inside the session (domain fronting) can be asked for another
//! tenant under an allowed name, and a shared host serves every path on it to anyone:
//! any bucket on storage.googleapis.com, any repository on raw.githubusercontent.com.
//! An entry for a shared host admits all of it. A TLS 1.2 renegotiation sends its
//! ClientHello inside the session, encrypted, so the name in it is not seen.

use std::net::IpAddr;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

/// The largest TLS record fragment, 2^14 bytes.
const RECORD_MAX: usize = 16 * 1024;

/// The most of one side's bytes the egress holds before it decides on them: one TLS
/// record of the largest size and its header. A ClientHello or a ServerHello is a few
/// hundred bytes to a few kilobytes; one that needs more is refused.
pub(crate) const HELLO_MAX: usize = RECORD_MAX + 5;

/// How long a tunnel has, once it opens, for the client's ClientHello, the upstream's
/// ServerHello, and after a HelloRetryRequest the second ClientHello, all together.
pub(crate) const HELLO_WAIT: Duration = Duration::from_secs(10);

/// A fatal TLS alert, `access_denied`, for a client whose hello is refused, so its TLS
/// library reports a refusal rather than a dropped connection.
pub(crate) const REFUSED_ALERT: [u8; 7] = [21, 3, 3, 0, 2, 2, 49];

/// The random a ServerHello carries when it is a HelloRetryRequest, RFC 8446 4.1.3:
/// SHA-256 of "HelloRetryRequest".
pub(crate) const RETRY_RANDOM: [u8; 32] = [
    0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8, 0x91,
    0xc2, 0xa2, 0x11, 0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8, 0x33, 0x9c,
];

/// The ChangeCipherSpec record a client may send before its second ClientHello; its
/// third byte, the minor version, may be any.
const CHANGE_CIPHER_SPEC: [u8; 6] = [20, 3, 3, 0, 1, 1];

const HANDSHAKE: u8 = 22;
const CLIENT_HELLO: u8 = 1;
const SERVER_HELLO: u8 = 2;
const SERVER_NAME: u16 = 0;
const HOST_NAME: u8 = 0;
/// Encrypted Client Hello's drafts, from 0xfe00 on, and its final codepoint, 0xfe0d.
const ECH_DRAFT_FIRST: u16 = 0xfe00;
const ENCRYPTED_CLIENT_HELLO: u16 = 0xfe0d;
/// Encrypted SNI, the draft ECH replaced.
const ENCRYPTED_SERVER_NAME: u16 = 0xffce;

const NOT_TLS: &str = "the first bytes are not a TLS handshake";
const NOT_HELLO: &str = "the first handshake message is not a ClientHello";
const MALFORMED: &str = "the ClientHello is malformed";
const TOO_LONG: &str = "the ClientHello is longer than one TLS record";
const AFTER_HELLO: &str = "the ClientHello's record carries more after it";
const HIDDEN: &str =
    "the ClientHello uses Encrypted Client Hello (ECH), so the name in the clear is a decoy";
const TWO_NAMES: &str = "the ClientHello carries two server names";
const NOT_CCS: &str = "the record before the second ClientHello is not a ChangeCipherSpec";

/// What a tunnel's first bytes say.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Not enough bytes yet to tell.
    More,
    /// A ClientHello that names the CONNECT host and hides no other name, in this many
    /// bytes: the records that carry it.
    Allowed(usize),
    /// Anything else, and why.
    Refused(String),
}

/// What `first`, a tunnel's first bytes, say about a tunnel to `host`, as the module
/// docs have it.
pub(crate) fn verdict(first: &[u8], host: &str) -> Verdict {
    let (hello, length) = match client_hello(first) {
        Ok(Some(found)) => found,
        Ok(None) => return Verdict::More,
        Err(why) => return Verdict::Refused(why.into()),
    };
    match server_name(&hello) {
        Ok(None) if host.parse::<IpAddr>().is_ok() => Verdict::Allowed(length),
        Ok(None) => Verdict::Refused("the ClientHello names no server".into()),
        Ok(Some(name)) if same_host(name, host) => Verdict::Allowed(length),
        Ok(Some(name)) => Verdict::Refused(format!(
            "the ClientHello names {:?}, not {host}",
            String::from_utf8_lossy(name)
        )),
        Err(why) => Verdict::Refused(why.into()),
    }
}

/// [`verdict`] on the client's bytes after a HelloRetryRequest: the second ClientHello,
/// after the ChangeCipherSpec record a client in middlebox compatibility mode sends
/// first. Any other record before the hello is refused.
pub(crate) fn second_verdict(first: &[u8], host: &str) -> Verdict {
    if first.first() != Some(&CHANGE_CIPHER_SPEC[0]) {
        return verdict(first, host);
    }
    let expected = first
        .iter()
        .zip(CHANGE_CIPHER_SPEC)
        .enumerate()
        .all(|(i, (got, want))| i == 2 || *got == want);
    if !expected {
        return Verdict::Refused(NOT_CCS.into());
    }
    let Some(hello) = first.get(CHANGE_CIPHER_SPEC.len()..) else {
        return Verdict::More;
    };
    match verdict(hello, host) {
        Verdict::Allowed(length) => Verdict::Allowed(CHANGE_CIPHER_SPEC.len() + length),
        other => other,
    }
}

/// A ClientHello as read from the client: the bytes that carry it, and any the client
/// sent after them in the same reads.
pub(crate) struct Flight {
    /// The records the hello takes, a ChangeCipherSpec before it included.
    pub(crate) hello: Vec<u8>,
    /// What came after them.
    pub(crate) after: Vec<u8>,
}

/// Read from `client`, after the bytes `first` already holds, until [`verdict`] (or
/// [`second_verdict`], after a retry request) decides for `host`, within [`HELLO_MAX`]
/// bytes and by `deadline`. The hello when it is allowed; why not otherwise.
pub(crate) async fn read_hello<R: AsyncRead + Unpin>(
    client: &mut R,
    host: &str,
    retry: bool,
    mut first: Vec<u8>,
    deadline: Instant,
) -> Result<Flight, String> {
    let judge = if retry { second_verdict } else { verdict };
    let read = async {
        let mut chunk = [0u8; 4096];
        loop {
            match judge(&first, host) {
                Verdict::Allowed(length) => {
                    let after = first.get(length..).map(<[u8]>::to_vec);
                    first.truncate(length);
                    let after = after.unwrap_or_default();
                    return Ok(Flight {
                        hello: first,
                        after,
                    });
                }
                Verdict::Refused(why) => return Err(why),
                Verdict::More if first.len() >= HELLO_MAX => {
                    return Err(format!("no whole ClientHello in {HELLO_MAX} bytes"))
                }
                Verdict::More => {}
            }
            let room = chunk.len().min(HELLO_MAX - first.len());
            match client.read(&mut chunk[..room]).await {
                Ok(0) => return Err("the client closed before a whole ClientHello".into()),
                Ok(n) => first.extend_from_slice(&chunk[..n]),
                Err(e) => return Err(format!("reading the ClientHello: {e}")),
            }
        }
    };
    tokio::time::timeout_at(deadline, read)
        .await
        .unwrap_or_else(|_| Err("no whole ClientHello in time".into()))
}

/// What an upstream's first bytes say.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    /// Not enough bytes yet to tell.
    More,
    /// A HelloRetryRequest: the client sends a second ClientHello next.
    Retry,
    /// Anything that asks for no second hello: a ServerHello, an alert.
    Other,
    /// A handshake that could be a retry request and cannot be read, and why.
    Refused(String),
}

/// What `seen`, an upstream's first bytes, say about the client's next ones.
pub(crate) fn answer(seen: &[u8]) -> Answer {
    match handshake(seen, SERVER_HELLO) {
        Ok(None) => Answer::More,
        Ok(Some(found)) if found.message.get(6..38) == Some(&RETRY_RANDOM[..]) => Answer::Retry,
        Ok(Some(_)) => Answer::Other,
        Err(why) if why == NOT_TLS || why == NOT_HELLO => Answer::Other,
        Err(_) => Answer::Refused("the upstream's ServerHello cannot be read".into()),
    }
}

/// Read from `upstream` until [`answer`] decides, sending each read on to `client` as
/// it comes, within [`HELLO_MAX`] bytes and by `deadline`. Whether the upstream asked
/// for a second ClientHello; why the tunnel stops otherwise.
pub(crate) async fn read_answer<U, C>(
    upstream: &mut U,
    client: &mut C,
    deadline: Instant,
) -> Result<bool, String>
where
    U: AsyncRead + Unpin,
    C: AsyncWrite + Unpin,
{
    let read = async {
        let mut seen = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match answer(&seen) {
                Answer::Retry => return Ok(true),
                Answer::Other => return Ok(false),
                Answer::Refused(why) => return Err(why),
                Answer::More if seen.len() >= HELLO_MAX => {
                    return Err(format!("no whole ServerHello in {HELLO_MAX} bytes"))
                }
                Answer::More => {}
            }
            let room = chunk.len().min(HELLO_MAX - seen.len());
            let n = match upstream.read(&mut chunk[..room]).await {
                Ok(0) => return Err("the upstream closed before its ServerHello".into()),
                Ok(n) => n,
                Err(e) => return Err(format!("reading the ServerHello: {e}")),
            };
            client
                .write_all(&chunk[..n])
                .await
                .map_err(|e| format!("sending the ServerHello on: {e}"))?;
            seen.extend_from_slice(&chunk[..n]);
        }
    };
    tokio::time::timeout_at(deadline, read)
        .await
        .unwrap_or_else(|_| Err("no whole ServerHello in time".into()))
}

/// The ClientHello at the start of `first` and the bytes its records take; `None`
/// while it is incomplete. An error for bytes that do not start a TLS handshake with a
/// ClientHello, or whose last record carries more after it.
fn client_hello(first: &[u8]) -> Result<Option<(Vec<u8>, usize)>, &'static str> {
    match handshake(first, CLIENT_HELLO)? {
        None => Ok(None),
        Some(found) if found.trailing => Err(AFTER_HELLO),
        Some(found) => Ok(Some((found.message, found.records))),
    }
}

/// The first handshake message of a side, as its records carry it.
struct Handshake {
    /// The message, its 4-byte header included.
    message: Vec<u8>,
    /// The bytes the records that carry it take.
    records: usize,
    /// Whether its last record carries more after it.
    trailing: bool,
}

/// The handshake message of type `kind` at the start of `bytes`, put together from
/// the TLS records it spans; `None` while it is incomplete.
fn handshake(bytes: &[u8], kind: u8) -> Result<Option<Handshake>, &'static str> {
    let mut message = Vec::new();
    let mut rest = bytes;
    loop {
        if let Some(length) = message_length(&message, kind)? {
            if message.len() >= length {
                return Ok(Some(Handshake {
                    trailing: message.len() > length,
                    message,
                    records: bytes.len() - rest.len(),
                }));
            }
        }
        let Some((fragment, after)) = next_record(rest)? else {
            return Ok(None);
        };
        message.extend_from_slice(fragment);
        rest = after;
    }
}

/// The length of a handshake message of type `kind`, its 4-byte header included, once
/// `message` holds that header; an error when it is of another type or too long.
fn message_length(message: &[u8], kind: u8) -> Result<Option<usize>, &'static str> {
    match message {
        [] => Ok(None),
        [first, ..] if *first != kind => Err(NOT_HELLO),
        [_, a, b, c, ..] => {
            let length = 4 + (usize::from(*a) << 16 | usize::from(*b) << 8 | usize::from(*c));
            if length > RECORD_MAX {
                return Err(TOO_LONG);
            }
            Ok(Some(length))
        }
        _ => Ok(None),
    }
}

/// A record's fragment, and the bytes after the record.
type Record<'a> = (&'a [u8], &'a [u8]);

/// The fragment of the handshake record at the start of `rest`, and what follows it;
/// `None` while it is incomplete. The first two bytes are checked as they arrive, so
/// bytes that are not TLS are refused without waiting for more.
fn next_record(rest: &[u8]) -> Result<Option<Record<'_>>, &'static str> {
    match rest {
        [kind, ..] if *kind != HANDSHAKE => return Err(NOT_TLS),
        [_, major, ..] if *major != 3 => return Err(NOT_TLS),
        _ => {}
    }
    let Some((&[_, _, _, hi, lo], after)) = rest.split_first_chunk::<5>() else {
        return Ok(None);
    };
    let length = usize::from(u16::from_be_bytes([hi, lo]));
    // an empty handshake record is not allowed, and a longer one is not TLS
    if length == 0 || length > RECORD_MAX {
        return Err(MALFORMED);
    }
    Ok(after.split_at_checked(length))
}

/// The host name a whole ClientHello handshake message carries, `None` for one with
/// no server_name extension. An error for a malformed hello, one with two names, or one
/// with an encrypted name.
fn server_name(hello: &[u8]) -> Result<Option<&[u8]>, &'static str> {
    let mut r = Reader(hello);
    // the handshake header, then legacy_version and random
    r.take(4 + 2 + 32)?;
    r.vec8()?; // legacy_session_id
    r.vec16()?; // cipher_suites
    r.vec8()?; // legacy_compression_methods
    if r.0.is_empty() {
        // a hello from before extensions
        return Ok(None);
    }
    let mut extensions = Reader(r.vec16()?);
    r.end()?;
    let mut name = None;
    while !extensions.0.is_empty() {
        let kind = extensions.u16()?;
        let data = extensions.vec16()?;
        match kind {
            ECH_DRAFT_FIRST..=ENCRYPTED_CLIENT_HELLO | ENCRYPTED_SERVER_NAME => return Err(HIDDEN),
            SERVER_NAME if name.is_some() => return Err(TWO_NAMES),
            SERVER_NAME => name = Some(host_name(data)?),
            _ => {}
        }
    }
    Ok(name)
}

/// The one host name in a server_name extension's data. A list with another kind of
/// name, or with more than one, is refused.
fn host_name(data: &[u8]) -> Result<&[u8], &'static str> {
    let mut r = Reader(data);
    let mut list = Reader(r.vec16()?);
    r.end()?;
    let kind = list.u8()?;
    let name = list.vec16()?;
    if !list.0.is_empty() {
        return Err(TWO_NAMES);
    }
    if kind != HOST_NAME || name.is_empty() {
        return Err(MALFORMED);
    }
    Ok(name)
}

/// Whether a server name is the CONNECT host: ASCII case aside, and a trailing dot.
fn same_host(name: &[u8], host: &str) -> bool {
    fn bare(s: &[u8]) -> &[u8] {
        s.strip_suffix(b".").unwrap_or(s)
    }
    bare(name).eq_ignore_ascii_case(bare(host.as_bytes()))
}

/// The bytes of a handshake message not yet read; every length is checked against
/// them, and running short is [`MALFORMED`].
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        let (head, rest) = self.0.split_at_checked(n).ok_or(MALFORMED)?;
        self.0 = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, &'static str> {
        let (&first, rest) = self.0.split_first().ok_or(MALFORMED)?;
        self.0 = rest;
        Ok(first)
    }

    fn u16(&mut self) -> Result<u16, &'static str> {
        let (&pair, rest) = self.0.split_first_chunk::<2>().ok_or(MALFORMED)?;
        self.0 = rest;
        Ok(u16::from_be_bytes(pair))
    }

    /// A vector with a one-byte length.
    fn vec8(&mut self) -> Result<&'a [u8], &'static str> {
        let n = self.u8()?;
        self.take(usize::from(n))
    }

    /// A vector with a two-byte length.
    fn vec16(&mut self) -> Result<&'a [u8], &'static str> {
        let n = self.u16()?;
        self.take(usize::from(n))
    }

    /// Nothing left over.
    fn end(&self) -> Result<(), &'static str> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(MALFORMED)
        }
    }
}

/// ClientHellos for tests: as rustls sends one, and built by hand.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::sync::Arc;

    /// The first flight rustls sends for `name`: its ClientHello in one record, with
    /// the name as its server name when `sni`. rustls sends none for an address.
    pub(crate) fn rustls_hello(name: &str, sni: bool) -> Vec<u8> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        config.enable_sni = sni;
        let name = rustls::pki_types::ServerName::try_from(name.to_string()).unwrap();
        let mut conn = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
        let mut out = Vec::new();
        conn.write_tls(&mut out).unwrap();
        out
    }

    /// A ClientHello handshake message with these extensions, `(type, data)`.
    pub(crate) fn message(extensions: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut exts = Vec::new();
        for (kind, data) in extensions {
            exts.extend_from_slice(&kind.to_be_bytes());
            exts.extend_from_slice(&(data.len() as u16).to_be_bytes());
            exts.extend_from_slice(data);
        }
        let mut body = vec![3, 3];
        body.extend_from_slice(&[7; 32]); // random
        body.extend_from_slice(&[0]); // no session id
        body.extend_from_slice(&[0, 2, 0x13, 0x01]); // one cipher suite
        body.extend_from_slice(&[1, 0]); // null compression
        body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        body.extend_from_slice(&exts);
        let mut message = vec![1];
        message.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        message.extend_from_slice(&body);
        message
    }

    /// A server_name extension listing these host names.
    pub(crate) fn sni(names: &[&str]) -> (u16, Vec<u8>) {
        let mut list = Vec::new();
        for name in names {
            list.push(0);
            list.extend_from_slice(&(name.len() as u16).to_be_bytes());
            list.extend_from_slice(name.as_bytes());
        }
        let mut data = (list.len() as u16).to_be_bytes().to_vec();
        data.extend_from_slice(&list);
        (0, data)
    }

    /// An encrypted_client_hello extension, its payload opaque here.
    pub(crate) fn ech() -> (u16, Vec<u8>) {
        (0xfe0d, vec![0, 0, 1, 0, 1, 7, 0, 0, 0, 0])
    }

    /// A ServerHello handshake message with this random and these extensions.
    pub(crate) fn server_hello(random: [u8; 32], extensions: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut exts = Vec::new();
        for (kind, data) in extensions {
            exts.extend_from_slice(&kind.to_be_bytes());
            exts.extend_from_slice(&(data.len() as u16).to_be_bytes());
            exts.extend_from_slice(data);
        }
        let mut body = vec![3, 3];
        body.extend_from_slice(&random);
        body.extend_from_slice(&[0]); // no session id
        body.extend_from_slice(&[0x13, 0x01, 0]); // the cipher suite, null compression
        body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        body.extend_from_slice(&exts);
        let mut message = vec![2];
        message.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        message.extend_from_slice(&body);
        message
    }

    /// What a TLS 1.3 front end that wants another key share answers a ClientHello with:
    /// a HelloRetryRequest, then a ChangeCipherSpec for middleboxes.
    pub(crate) fn retry_request() -> Vec<u8> {
        let versions = (0x2b, vec![3, 4]);
        let key_share = (0x33, vec![0, 0x1d]);
        let mut out = records(
            &server_hello(super::RETRY_RANDOM, &[versions, key_share]),
            512,
        );
        out.extend_from_slice(&[20, 3, 3, 0, 1, 1]);
        out
    }

    /// `message` in TLS handshake records of at most `size` bytes each.
    pub(crate) fn records(message: &[u8], size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for fragment in message.chunks(size) {
            out.extend_from_slice(&[22, 3, 1]);
            out.extend_from_slice(&(fragment.len() as u16).to_be_bytes());
            out.extend_from_slice(fragment);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{ech, message, records, retry_request, rustls_hello, server_hello, sni};
    use super::*;
    use tokio::io::AsyncWriteExt;

    const HOST: &str = "upstream.example";

    /// A deadline `wait` from now.
    fn soon(wait: Duration) -> Instant {
        Instant::now() + wait
    }

    fn refused(first: &[u8]) -> String {
        match verdict(first, HOST) {
            Verdict::Refused(why) => why,
            other => panic!("refused, and judged {other:?}"),
        }
    }

    #[test]
    fn a_hello_that_names_the_connect_host_is_allowed() {
        let real = rustls_hello(HOST, true);
        assert_eq!(verdict(&real, HOST), Verdict::Allowed(real.len()));
        let hand = records(&message(&[(10, vec![0, 2, 0, 29]), sni(&[HOST])]), 512);
        assert_eq!(verdict(&hand, HOST), Verdict::Allowed(hand.len()));
    }

    #[test]
    fn a_name_is_compared_without_case_or_a_trailing_dot() {
        for (name, host) in [
            ("UPSTREAM.Example", HOST),
            ("upstream.example.", HOST),
            ("Upstream.Example.", HOST),
            (HOST, "Upstream.example."),
        ] {
            let first = records(&message(&[sni(&[name])]), 512);
            let judged = verdict(&first, host);
            assert_eq!(judged, Verdict::Allowed(first.len()), "{name} for {host}");
        }
    }

    #[test]
    fn a_hello_that_names_another_host_is_refused() {
        for name in [
            "other-tenant.example",
            "upstream.example.evil",
            "evil.upstream.example",
            "upstream.example..",
            "upstream.exampl",
            "upstream.example\0",
        ] {
            let why = refused(&records(&message(&[sni(&[name])]), 512));
            assert!(why.contains("not upstream.example"), "{name}: {why}");
        }
        // the name a client asks for, not some other allowed one
        let why = refused(&rustls_hello("github.com", true));
        assert!(why.contains("\"github.com\""), "{why}");
    }

    #[test]
    fn a_hello_with_no_name_is_refused_unless_the_connect_named_an_address() {
        let why = refused(&rustls_hello(HOST, false));
        assert!(why.contains("names no server"), "{why}");
        let why = refused(&records(&message(&[]), 512));
        assert!(why.contains("names no server"), "{why}");
        // a hello from before extensions
        let mut bare = message(&[]);
        bare.truncate(bare.len() - 2);
        bare[3] -= 2;
        let why = refused(&records(&bare, 512));
        assert!(why.contains("names no server"), "{why}");
        // rustls, like any client, sends no name for an address
        for ip in ["198.41.0.4", "2001:503:ba3e::2:30"] {
            let real = rustls_hello(ip, true);
            assert_eq!(verdict(&real, ip), Verdict::Allowed(real.len()), "{ip}");
            let named = records(&message(&[sni(&["other-tenant.example"])]), 512);
            assert!(matches!(verdict(&named, ip), Verdict::Refused(_)), "{ip}");
        }
    }

    #[test]
    fn a_hello_that_hides_its_name_is_refused() {
        for extensions in [
            vec![sni(&[HOST]), ech()],
            vec![ech(), sni(&[HOST])],
            vec![sni(&[HOST]), (0xffce, vec![0; 8])],
        ] {
            let why = refused(&records(&message(&extensions), 512));
            assert_eq!(why, HIDDEN);
        }
    }

    #[test]
    fn a_hello_that_uses_a_draft_of_ech_is_refused_and_the_reason_says_ech() {
        for kind in [0xfe00, 0xfe08] {
            let why = refused(&records(&message(&[sni(&[HOST]), (kind, vec![0; 8])]), 512));
            assert_eq!(why, HIDDEN, "{kind:#x}");
        }
        assert!(HIDDEN.contains("Encrypted Client Hello (ECH)"), "{HIDDEN}");
    }

    #[test]
    fn a_hello_whose_record_carries_another_handshake_message_is_refused() {
        // a front end would read the second message, another ClientHello here, as the
        // client's next one
        let mut both = message(&[sni(&[HOST])]);
        both.extend_from_slice(&message(&[sni(&["other-tenant.example"])]));
        let why = refused(&records(&both, 512));
        assert!(why.contains("carries more after it"), "{why}");
        // and in the record that ends the hello, when the hello spans two
        let hello = message(&[sni(&[HOST])]);
        let mut first = records(&hello[..20], 512);
        let mut rest = hello[20..].to_vec();
        rest.extend_from_slice(&[20, 0, 0, 0]);
        first.extend_from_slice(&records(&rest, 512));
        let why = refused(&first);
        assert!(why.contains("carries more after it"), "{why}");
    }

    #[test]
    fn a_hello_with_two_names_is_refused() {
        let why = refused(&records(&message(&[sni(&[HOST]), sni(&[HOST])]), 512));
        assert_eq!(why, TWO_NAMES);
        let why = refused(&records(&message(&[sni(&[HOST, "other.example"])]), 512));
        assert_eq!(why, TWO_NAMES);
        let why = refused(&records(&message(&[sni(&["other.example", HOST])]), 512));
        assert_eq!(why, TWO_NAMES);
    }

    #[test]
    fn bytes_that_are_not_a_tls_client_hello_are_refused_from_the_first_byte_that_says_so() {
        assert_eq!(refused(b"G"), NOT_TLS);
        assert_eq!(refused(b"GET / HTTP/1.1\r\n"), NOT_TLS);
        assert_eq!(refused(b"ping"), NOT_TLS);
        assert_eq!(refused(&[22, 2]), NOT_TLS);
        // SSLv2's hello
        assert_eq!(refused(&[0x80, 0x2e, 1, 3, 1]), NOT_TLS);
        // a ServerHello, and an alert, in the first record
        assert_eq!(refused(&records(&[2, 0, 0, 0], 512)), NOT_HELLO);
        assert_eq!(refused(&[21, 3, 3, 0, 2, 2, 40]), NOT_TLS);
        // an empty handshake record
        assert_eq!(refused(&[22, 3, 1, 0, 0]), MALFORMED);
        // a hello followed by an application record before it ends
        let hello = message(&[sni(&[HOST])]);
        let mut first = records(&hello[..10], 512);
        first.extend_from_slice(&[23, 3, 3, 0, 1, 0]);
        assert_eq!(refused(&first), NOT_TLS);
    }

    #[test]
    fn a_truncated_hello_waits_for_more_and_never_passes() {
        for first in [
            rustls_hello(HOST, true),
            records(&message(&[sni(&[HOST])]), 512),
            records(&message(&[sni(&[HOST])]), 7),
        ] {
            for end in 0..first.len() {
                assert_eq!(verdict(&first[..end], HOST), Verdict::More, "{end}");
            }
            assert_eq!(verdict(&first, HOST), Verdict::Allowed(first.len()));
        }
    }

    #[test]
    fn a_hello_over_several_records_is_put_together() {
        let hello = message(&[(10, vec![0, 2, 0, 29]), sni(&[HOST])]);
        for size in [1, 2, 5, 40, hello.len() - 1] {
            let first = records(&hello, size);
            let judged = verdict(&first, HOST);
            assert_eq!(judged, Verdict::Allowed(first.len()), "{size}");
        }
        let other = message(&[sni(&["other-tenant.example"])]);
        assert!(matches!(
            verdict(&records(&other, 3), HOST),
            Verdict::Refused(_)
        ));
        // bytes after the hello's records, such as early data, are not the hello's
        let mut first = records(&hello, 50);
        let length = first.len();
        first.extend_from_slice(&[20, 3, 3, 0, 1, 1, 23, 3, 3, 0, 2, 9, 9]);
        assert_eq!(verdict(&first, HOST), Verdict::Allowed(length));
    }

    #[test]
    fn lengths_that_disagree_are_malformed() {
        let hello = message(&[sni(&[HOST])]);
        // the hello says it is longer than its extensions make it
        let mut long = hello.clone();
        long[3] += 1;
        long.push(0);
        assert_eq!(refused(&records(&long, 512)), MALFORMED);
        // and shorter
        let mut short = hello.clone();
        short[3] -= 1;
        short.pop();
        assert_eq!(refused(&records(&short, 512)), MALFORMED);
        // a server_name list longer than its extension
        let (_, mut data) = sni(&[HOST]);
        data[1] += 1;
        assert_eq!(refused(&records(&message(&[(0, data)]), 512)), MALFORMED);
        // an empty name, and a name of another kind
        let (_, mut data) = sni(&[""]);
        assert_eq!(
            refused(&records(&message(&[(0, data.clone())]), 512)),
            MALFORMED
        );
        data = sni(&[HOST]).1;
        data[2] = 1;
        assert_eq!(refused(&records(&message(&[(0, data)]), 512)), MALFORMED);
    }

    #[test]
    fn a_hello_longer_than_a_record_is_refused_when_it_says_so() {
        let padding = (21, vec![0; RECORD_MAX]);
        let hello = message(&[sni(&[HOST]), padding]);
        // as soon as the record that carries the hello's header is in
        let first = records(&hello, 100);
        assert_eq!(refused(&first[..105]), TOO_LONG);
        let first = records(&hello, RECORD_MAX);
        assert_eq!(verdict(&first[..HELLO_MAX - 1], HOST), Verdict::More);
        assert_eq!(refused(&first[..HELLO_MAX]), TOO_LONG);
        // a record longer than TLS allows
        assert_eq!(refused(&[22, 3, 1, 0x40, 1, 1]), MALFORMED);
    }

    /// Hostile bytes: random strings, and a valid hello with each byte changed and cut
    /// short. None may panic, and a hello whose name was changed never passes.
    #[test]
    fn no_bytes_make_the_check_panic() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..500 {
            let len = (next() % 600) as usize;
            let mut bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if next() % 2 == 0 && bytes.len() >= 6 {
                // past the first checks, into the parser
                bytes[..3].copy_from_slice(&[22, 3, 1]);
                bytes[5] = 1;
            }
            let _ = verdict(&bytes, HOST);
        }
        let hello = records(&message(&[(10, vec![0, 2, 0, 29]), sni(&[HOST])]), 512);
        let name_at = hello.len() - HOST.len();
        for at in 0..hello.len() {
            for value in [0, 1, 0x7f, 0x80, 0xff, hello[at].wrapping_add(1)] {
                let mut changed = hello.clone();
                changed[at] = value;
                for end in [changed.len(), changed.len() / 2, at] {
                    let judged = verdict(&changed[..end], HOST);
                    if at >= name_at && changed[at] != hello[at] {
                        let passed = matches!(judged, Verdict::Allowed(_));
                        assert!(!passed, "byte {at} to {value}");
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn a_hello_split_across_reads_is_read_whole_with_what_follows() {
        let mut sent = rustls_hello(HOST, true);
        sent.extend_from_slice(b"after");
        let (mut client, mut egress) = tokio::io::duplex(64);
        let pieces = sent.clone();
        tokio::spawn(async move {
            for piece in pieces.chunks(3) {
                client.write_all(piece).await.unwrap();
                tokio::task::yield_now().await;
            }
            // held open: the egress decides on the bytes, not on the close
            std::future::pending::<()>().await;
        });
        let first = read_hello(&mut egress, HOST, false, Vec::new(), soon(HELLO_WAIT))
            .await
            .unwrap();
        let length = sent.len() - b"after".len();
        assert_eq!(first.hello, sent[..length], "the hello, whole");
        assert!(
            sent[length..].starts_with(&first.after),
            "{:?}",
            first.after
        );
    }

    #[tokio::test]
    async fn a_client_that_sends_too_much_too_slowly_or_too_little_is_refused() {
        // too much: a small hello in one-byte records is over the bound before it ends
        let hello = message(&[(21, vec![0; 3000]), sni(&[HOST])]);
        let (mut client, mut egress) = tokio::io::duplex(HELLO_MAX * 8);
        client.write_all(&records(&hello, 1)).await.unwrap();
        let why = read_hello(&mut egress, HOST, false, Vec::new(), soon(HELLO_WAIT))
            .await
            .err()
            .unwrap();
        assert!(why.contains(&format!("in {HELLO_MAX} bytes")), "{why}");
        // too slowly: nothing at all, held open
        let (_client, mut egress) = tokio::io::duplex(64);
        let wait = std::time::Duration::from_millis(50);
        let why = read_hello(&mut egress, HOST, false, Vec::new(), soon(wait))
            .await
            .err()
            .unwrap();
        assert!(why.contains("in time"), "{why}");
        // too little: half a hello, then closed
        let (mut client, mut egress) = tokio::io::duplex(4096);
        let half = rustls_hello(HOST, true);
        client.write_all(&half[..half.len() / 2]).await.unwrap();
        drop(client);
        let why = read_hello(&mut egress, HOST, false, Vec::new(), soon(HELLO_WAIT))
            .await
            .err()
            .unwrap();
        assert!(why.contains("closed before"), "{why}");
    }

    #[test]
    fn a_second_hello_may_follow_one_change_cipher_spec_and_is_held_to_the_same_rule() {
        let hello = records(&message(&[sni(&[HOST])]), 512);
        assert_eq!(second_verdict(&hello, HOST), Verdict::Allowed(hello.len()));
        for minor in [1, 3] {
            let mut first = vec![20, 3, minor, 0, 1, 1];
            first.extend_from_slice(&hello);
            assert_eq!(second_verdict(&first, HOST), Verdict::Allowed(first.len()));
            for end in 0..first.len() {
                assert_eq!(second_verdict(&first[..end], HOST), Verdict::More, "{end}");
            }
        }
        let mut other = vec![20, 3, 3, 0, 1, 1];
        other.extend_from_slice(&records(&message(&[sni(&["other-tenant.example"])]), 512));
        assert!(matches!(second_verdict(&other, HOST), Verdict::Refused(_)));
        // two ChangeCipherSpecs, one of another length, early data, or an alert first
        for before in [
            &[20, 3, 3, 0, 1, 1, 20, 3, 3, 0, 1, 1][..],
            &[20, 3, 3, 0, 2, 1, 1],
            &[20, 3, 3, 0, 1, 2],
            &[23, 3, 3, 0, 1, 0],
            &[21, 3, 3, 0, 2, 1, 0],
        ] {
            let mut first = before.to_vec();
            first.extend_from_slice(&hello);
            let judged = second_verdict(&first, HOST);
            assert!(
                matches!(judged, Verdict::Refused(_)),
                "{before:?}: {judged:?}"
            );
        }
    }

    #[test]
    fn a_retry_request_is_told_from_any_other_answer() {
        let retry = retry_request();
        assert_eq!(answer(&retry), Answer::Retry);
        for end in 0..retry.len() - 6 {
            assert_eq!(answer(&retry[..end]), Answer::More, "{end}");
        }
        // split over records of every size
        let message = server_hello(RETRY_RANDOM, &[(0x2b, vec![3, 4])]);
        for size in [1, 2, 7, 38, message.len() - 1] {
            assert_eq!(answer(&records(&message, size)), Answer::Retry, "{size}");
        }
        // a TLS 1.3 ServerHello, and a TLS 1.2 one followed in its record by more
        let mut random = RETRY_RANDOM;
        random[31] ^= 1;
        let tls13 = records(&server_hello(random, &[(0x2b, vec![3, 4])]), 512);
        assert_eq!(answer(&tls13), Answer::Other);
        let mut tls12 = server_hello([0x5a; 32], &[]);
        tls12.extend_from_slice(&[11, 0, 0, 3, 0, 0, 0]);
        assert_eq!(answer(&records(&tls12, 512)), Answer::Other);
        // an alert, or a handshake that starts with something else
        assert_eq!(answer(&[21, 3, 3, 0, 2, 2, 40]), Answer::Other);
        assert_eq!(answer(&records(&[11, 0, 0, 0], 512)), Answer::Other);
        assert_eq!(answer(b"HTTP/1.1 400 Bad Request\r\n"), Answer::Other);
        // a ServerHello that cannot be read is not taken for a plain one
        assert!(matches!(answer(&[22, 3, 3, 0, 0]), Answer::Refused(_)));
        let mut long = vec![22, 3, 3, 0, 4, 2, 0x40, 0, 1];
        long.extend_from_slice(&[0; 4]);
        assert!(matches!(answer(&long), Answer::Refused(_)));
    }

    #[test]
    fn no_cut_or_change_of_a_retry_request_makes_the_answer_panic() {
        let retry = retry_request();
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        for _ in 0..300 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let mut changed = retry.clone();
            let at = (seed % changed.len() as u64) as usize;
            changed[at] = (seed >> 32) as u8;
            let _ = answer(&changed[..at.max(1)]);
            let _ = answer(&changed);
        }
    }

    #[tokio::test]
    async fn the_upstream_s_answer_is_sent_on_as_it_is_read() {
        let mut sent = retry_request();
        sent.extend_from_slice(b"more");
        let (mut upstream, mut from_upstream) = tokio::io::duplex(64);
        let (mut to_client, mut client) = tokio::io::duplex(4096);
        let pieces = sent.clone();
        tokio::spawn(async move {
            for piece in pieces.chunks(5) {
                upstream.write_all(piece).await.unwrap();
                tokio::task::yield_now().await;
            }
            std::future::pending::<()>().await;
        });
        let retry = read_answer(&mut from_upstream, &mut to_client, soon(HELLO_WAIT)).await;
        assert_eq!(retry, Ok(true));
        drop(to_client);
        let mut got = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut client, &mut got)
            .await
            .unwrap();
        assert!(sent.starts_with(&got), "{got:?}");
        assert!(got.len() >= retry_request().len() - 6, "the request, whole");
        // an upstream that says nothing is given up on by the deadline
        let (_upstream, mut from_upstream) = tokio::io::duplex(64);
        let (mut to_client, _client) = tokio::io::duplex(64);
        let wait = Duration::from_millis(50);
        let why = read_answer(&mut from_upstream, &mut to_client, soon(wait)).await;
        assert_eq!(why, Err("no whole ServerHello in time".into()));
    }
}
