//! `amem` with no command: find the web launcher running over this root, or start one,
//! and open the browser on a link that pairs it. What a running server is lives in
//! `state/web.json` (see `dm_web::state`): its address, where its sessions start, and its
//! token.
//!
//! The token never leaves this process for a server that has not proved it holds it:
//! the address in the file can outlive the server and be taken by anything, so the first
//! question is a nonce the server must answer under the token. And the token is never on
//! a command line: an opener gets a pairing code, single use and two minutes long.

use dm_web::state::WebState;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

/// The first port tried when none is given, and how many above it.
const PORT: u16 = 4200;
const PORTS: u16 = 10;

/// A running server that proved it holds the token, at the address it was asked at.
#[derive(Debug)]
pub struct Running {
    /// Where it listens.
    pub url: String,
    addr: SocketAddr,
}

impl Running {
    /// The address it was dialled at, for asking it more under the token.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

/// What a pairing gives: the code for the browser, and where sessions start.
#[derive(Debug)]
pub struct Pairing {
    /// Good once, for two minutes.
    pub code: String,
    /// The running server's working tree.
    pub workdir: String,
    /// The hosted origins it lets through.
    pub origins: Vec<String>,
}

/// The address an `http://` URL names, if it is a loopback one: the file names the
/// address, and nothing off this host is asked anything.
fn loopback(url: &str) -> Option<SocketAddr> {
    let addr: SocketAddr = url.strip_prefix("http://")?.parse().ok()?;
    addr.ip().is_loopback().then_some(addr)
}

/// One HTTP/1.1 exchange with a loopback server; the status and the body.
async fn exchange(
    addr: SocketAddr,
    method: &str,
    path: &str,
    auth: Option<&str>,
) -> Option<(u16, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let auth = auth
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let go = async {
        let mut s = tokio::net::TcpStream::connect(addr).await.ok()?;
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Length: 0\r\nConnection: close\r\n\r\n"
        );
        s.write_all(req.as_bytes()).await.ok()?;
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.ok()?;
        let text = String::from_utf8_lossy(&out).into_owned();
        let (head, body) = text.split_once("\r\n\r\n")?;
        let status = head.split(' ').nth(1)?.parse().ok()?;
        Some((status, body.to_string()))
    };
    tokio::time::timeout(Duration::from_secs(2), go)
        .await
        .ok()?
}

/// The server over this root at `url`, if one is there and proves it holds `token`, at
/// the address dialled and over this root, so an answer relayed from elsewhere fails.
pub async fn running(url: &str, token: &str, root: &Path) -> Option<Running> {
    let addr = loopback(url)?;
    let nonce = dm_web::access::new_token().ok()?;
    let (status, body) = exchange(addr, "GET", &format!("/api/hello?nonce={nonce}"), None).await?;
    if status != 200 {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let expected = dm_web::access::proof(token, addr, root, &nonce);
    (v["server"] == "amem" && v["proof"] == expected.as_str()).then(|| Running {
        url: url.to_string(),
        addr,
    })
}

/// A pairing code from a server that has proved it holds the token.
pub async fn pair(running: &Running, token: &str) -> Option<Pairing> {
    let (status, body) = exchange(running.addr, "POST", "/api/pair", Some(token)).await?;
    if status != 200 {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    Some(Pairing {
        code: v["code"].as_str()?.to_string(),
        workdir: v["workdir"].as_str().unwrap_or_default().to_string(),
        origins: v["origins"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|o| o.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// The state to start from. When the file was readable by others and a server still
/// proves the exposed token, that is refused: the server must stop before the token is
/// replaced, or it goes on answering to a token anyone could read.
///
/// # Errors
///
/// When the file is there and cannot be read, or a server holds an exposed token.
pub async fn state(root: &Path) -> Result<WebState, Box<dyn std::error::Error>> {
    let loaded = dm_web::state::load(root)?;
    if let Some(old) = &loaded.exposed {
        if let Some(url) = &old.url {
            if running(url, &old.token, root).await.is_some() {
                return Err(format!(
                    "{} is readable by others, and the launcher at {url} holds the token in it: \
                     stop that launcher (ctrl-c where it runs), then run amem again for a new token",
                    root.join(dm_web::state::FILE).display()
                )
                .into());
            }
        }
    }
    if let Some(why) = loaded.why {
        eprintln!("amem: {why}");
    }
    Ok(loaded.state)
}

/// A loopback listener: the address given, or the first free port from 4200.
///
/// # Errors
///
/// When the address given cannot be bound, or none of the default ports can.
pub async fn bind(listen: Option<&str>) -> Result<tokio::net::TcpListener, dm_web::Error> {
    if let Some(l) = listen {
        return dm_web::bind(l).await;
    }
    let mut last = None;
    for port in PORT..PORT + PORTS {
        match dm_web::bind(&format!("127.0.0.1:{port}")).await {
            Ok(l) => return Ok(l),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| dm_web::Error::Request("no port to listen on".into())))
}

/// A hosted copy of the pages: where it is, and the origin the server lets through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hosted {
    page: String,
    origin: String,
}

impl Hosted {
    /// A hosted page's address: https, or http on this machine for trying it out, and
    /// the root of its origin. Every page on an origin shares what the browser keeps for
    /// it, the key included, so the copy needs an origin of its own: a project site at
    /// `user.github.io/repo/` shares one with every other site of that account.
    ///
    /// # Errors
    ///
    /// When it is none of that.
    pub fn parse(url: &str) -> Result<Hosted, String> {
        let u = url::Url::parse(url).map_err(|e| format!("{url}: {e}"))?;
        let local = matches!(
            u.host(),
            Some(url::Host::Domain("localhost"))
                | Some(url::Host::Ipv4(std::net::Ipv4Addr::LOCALHOST))
                | Some(url::Host::Ipv6(std::net::Ipv6Addr::LOCALHOST))
        );
        if !(u.scheme() == "https" || (u.scheme() == "http" && local)) {
            return Err(format!(
                "{url}: a hosted page is https, or http on this machine"
            ));
        }
        if !u.username().is_empty() || u.password().is_some() {
            return Err(format!("{url}: a hosted page's address names no user"));
        }
        if u.path() != "/" {
            return Err(format!(
                "{url}: the hosted copy needs an origin of its own, at its root: every page \
                 on an origin can read what the browser keeps for it, the key included \
                 (for GitHub Pages, a custom domain on the site)"
            ));
        }
        let origin = u.origin().ascii_serialization();
        Ok(Hosted {
            page: format!("{origin}/"),
            origin,
        })
    }

    /// The origin the server lets through.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The page, told its server and the code in the fragment, which a browser does not
    /// send to the host.
    #[must_use]
    pub fn link(&self, server: &str, code: &str) -> String {
        let server = server.replace(':', "%3A").replace('/', "%2F");
        format!("{}#server={server}&pair={code}", self.page)
    }
}

/// How the pairing link is opened: in a browser or only printed, and on the server's own
/// pages or a hosted copy.
#[derive(Clone, Debug)]
pub struct Opening {
    /// Whether to start a browser on it.
    pub browser: bool,
    /// The hosted copy, when there is one.
    pub hosted: Option<Hosted>,
}

/// Open the pairing link in the browser, or say where it is. The link is good once, for
/// two minutes.
pub fn show(url: &str, code: &str, how: &Opening) {
    let link = match &how.hosted {
        Some(h) => h.link(url, code),
        None => format!("{url}/?pair={code}"),
    };
    eprintln!("amem: {link}  (pairs one browser, once, within two minutes)");
    if how.browser && !browser(&link) {
        eprintln!("amem: no browser to open; open the link above");
    }
}

/// The openers to try, in order: `BROWSER`, then the platform's own. Under WSL the
/// browser is Windows's, which `wslview` or `explorer.exe` reaches.
fn openers() -> Vec<Vec<String>> {
    let mut v: Vec<Vec<String>> = Vec::new();
    if let Ok(b) = std::env::var("BROWSER") {
        if !b.trim().is_empty() {
            v.push(vec![b]);
        }
    }
    let s = |a: &[&str]| a.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
    if cfg!(target_os = "macos") {
        v.push(s(&["open"]));
    } else if cfg!(windows) {
        v.push(s(&["rundll32", "url.dll,FileProtocolHandler"]));
    } else {
        if std::env::var_os("WSL_DISTRO_NAME").is_some() {
            v.push(s(&["wslview"]));
            v.push(s(&["explorer.exe"]));
        }
        v.push(s(&["xdg-open"]));
    }
    v
}

fn browser(url: &str) -> bool {
    for argv in openers() {
        let mut cmd = std::process::Command::new(&argv[0]);
        // the browser it starts needs none of the launcher's credentials
        for (k, _) in crate::credentials::KNOWN {
            cmd.env_remove(k);
        }
        let started = cmd
            .args(&argv[1..])
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut child) = started {
            // the opener hands the link on and exits; reap it so it is not left behind
            std::thread::spawn(move || child.wait());
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Something on a port that answers every request with `reply`, and hands back
    /// what it was sent.
    async fn listener(
        reply: &'static str,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                            reply.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        (url, rx)
    }

    #[tokio::test]
    async fn a_listener_that_cannot_prove_the_token_is_not_ours_and_never_sees_it() {
        let token = "t".repeat(64);
        let (url, mut seen) =
            listener(r#"{"server":"amem","root":"/r","proof":"made up","workdir":"/w"}"#).await;
        assert!(running(&url, &token, Path::new("/r")).await.is_none());
        let req = seen.recv().await.unwrap();
        assert!(req.starts_with("GET /api/hello?nonce="), "{req}");
        assert!(
            !req.contains(&token),
            "the token went to an unproven listener: {req}"
        );
    }

    /// A listener that answers every hello with a proof made under `token` for the
    /// address `claims` and the root `root`, as a relay of another server would.
    async fn prover(token: String, claims: Option<SocketAddr>, root: &'static str) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let own = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let nonce = req
                    .split("nonce=")
                    .nth(1)
                    .and_then(|r| r.split(' ').next())
                    .unwrap_or_default()
                    .to_string();
                let at = claims.unwrap_or(own);
                let body = serde_json::json!({
                    "server": "amem",
                    "proof": dm_web::access::proof(&token, at, Path::new(root), &nonce),
                })
                .to_string();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(reply.as_bytes()).await;
            }
        });
        format!("http://{own}")
    }

    #[tokio::test]
    async fn a_proof_for_another_root_or_relayed_from_another_port_is_not_ours() {
        let token = "t".repeat(64);
        let right = prover(token.clone(), None, "/r").await;
        assert!(running(&right, &token, Path::new("/r")).await.is_some());
        let other_root = prover(token.clone(), None, "/other").await;
        assert!(running(&other_root, &token, Path::new("/r"))
            .await
            .is_none());
        // what a squatter relaying another server's hello would hand back
        let elsewhere: SocketAddr = "127.0.0.1:4200".parse().unwrap();
        let relayed = prover(token.clone(), Some(elsewhere), "/r").await;
        assert!(running(&relayed, &token, Path::new("/r")).await.is_none());
    }

    #[test]
    fn a_hosted_page_has_an_origin_of_its_own_and_its_link_carries_the_server_in_the_fragment() {
        let h = Hosted::parse("https://Amem.Example:443/#old").unwrap();
        assert_eq!(h.origin(), "https://amem.example", "as a browser sends it");
        assert_eq!(
            h.link("http://127.0.0.1:4200", "c0de"),
            "https://amem.example/#server=http%3A%2F%2F127.0.0.1%3A4200&pair=c0de"
        );
        assert_eq!(
            Hosted::parse("http://localhost:8000/").unwrap().origin(),
            "http://localhost:8000"
        );
        assert_eq!(
            Hosted::parse("http://127.0.0.1:8000").unwrap().origin(),
            "http://127.0.0.1:8000"
        );
        for bad in [
            "http://example.org/",
            "ftp://x/",
            "someone.github.io",
            "https://",
            "https://someone.github.io/tooling-memory/",
            "http://127.0.0.1:1@evil.example/",
            "http://localhost:8000\\@evil.example/",
            "https://user:pw@amem.example/",
        ] {
            assert!(Hosted::parse(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn only_a_loopback_address_is_asked_anything() {
        assert!(loopback("http://127.0.0.1:4200").is_some());
        assert!(loopback("http://[::1]:4200").is_some());
        assert!(loopback("http://192.0.2.1:4200").is_none());
        assert!(loopback("http://10.0.0.1:4200").is_none());
        assert!(loopback("https://127.0.0.1:4200").is_none());
        assert!(
            loopback("http://localhost:4200").is_none(),
            "a name, which could resolve anywhere"
        );
    }

    #[tokio::test]
    async fn nothing_listening_is_not_a_server() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        assert!(running(&url, "t", Path::new("/r")).await.is_none());
    }
}
