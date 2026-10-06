//! `amem` with no command: find the web launcher running over this root, or start one,
//! and open the browser on a link that pairs it. What a running server is lives in
//! `state/web.json` (see `dm_web::state`): its address, where its sessions start, its
//! token, and the browsers paired with it.
//!
//! The token never leaves this process for a server that has not proved it holds it:
//! the address in the file can outlive the server and be taken by anything, so the first
//! question is a nonce the server must answer under the token. And the token is never on
//! a command line: an opener gets a pairing code, single use and two minutes long.

use dm_web::state::WebState;
use std::path::Path;
use std::time::Duration;

/// The first port tried when none is given, and how many above it.
const PORT: u16 = 4200;
const PORTS: u16 = 10;

/// A running server, as it answered.
#[derive(Debug)]
pub struct Running {
    /// Where it listens.
    pub url: String,
    /// Where its sessions start.
    pub workdir: String,
}

/// One HTTP/1.1 exchange with a loopback server; the status and the body.
async fn exchange(
    url: &str,
    method: &str,
    path: &str,
    auth: Option<&str>,
) -> Option<(u16, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let authority = url.strip_prefix("http://")?;
    // the file names the address; only a loopback one is asked anything
    let addr: std::net::SocketAddr = authority.parse().ok()?;
    if !addr.ip().is_loopback() {
        return None;
    }
    let auth = auth
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let go = async {
        let mut s = tokio::net::TcpStream::connect(addr).await.ok()?;
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {authority}\r\n{auth}Content-Length: 0\r\nConnection: close\r\n\r\n"
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

/// The server over this root at `url`, if one is there and proves it holds `token`.
pub async fn running(url: &str, token: &str, root: &Path) -> Option<Running> {
    let nonce = dm_web::access::new_token().ok()?;
    let (status, body) = exchange(url, "GET", &format!("/api/hello?nonce={nonce}"), None).await?;
    if status != 200 {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let proved = v["proof"] == dm_web::access::proof(token, &nonce).as_str();
    let ours = v["server"] == "amem" && v["root"] == root.display().to_string().as_str();
    (proved && ours).then(|| Running {
        url: url.to_string(),
        workdir: v["workdir"].as_str().unwrap_or_default().to_string(),
    })
}

/// A pairing code from a server that has proved it holds the token.
pub async fn pairing_code(running: &Running, token: &str) -> Option<String> {
    let (status, body) = exchange(&running.url, "POST", "/api/pair", Some(token)).await?;
    if status != 200 {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    v["code"].as_str().map(str::to_string)
}

/// The state, saying so when it had to be replaced.
///
/// # Errors
///
/// When there is no randomness for a token.
pub fn state(root: &Path) -> std::io::Result<WebState> {
    let (s, why) = dm_web::state::load(root)?;
    if let Some(why) = why {
        eprintln!("amem: {why}");
    }
    Ok(s)
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

/// Open the pairing link in the browser, or say where it is. The link is good once, for
/// two minutes.
pub fn show(url: &str, code: &str, open: bool) {
    let link = format!("{url}/?pair={code}");
    eprintln!("amem: {link}  (pairs one browser, once, within two minutes)");
    if open && !browser(&link) {
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
        let started = std::process::Command::new(&argv[0])
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

    #[tokio::test]
    async fn a_server_that_proves_the_token_over_another_root_is_not_ours() {
        let token = "t".repeat(64);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let t = token.clone();
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
                let body = serde_json::json!({
                    "server": "amem", "root": "/other", "workdir": "/w",
                    "proof": dm_web::access::proof(&t, &nonce),
                })
                .to_string();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(reply.as_bytes()).await;
            }
        });
        assert!(running(&url, &token, Path::new("/r")).await.is_none());
        let r = running(&url, &token, Path::new("/other")).await.unwrap();
        assert_eq!(r.workdir, "/w");
    }

    #[tokio::test]
    async fn nothing_listening_and_an_address_off_this_host_are_not_servers() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        assert!(running(&url, "t", Path::new("/r")).await.is_none());
        assert!(exchange("http://192.0.2.1:4200", "GET", "/", None)
            .await
            .is_none());
        assert!(exchange("https://example.org", "GET", "/", None)
            .await
            .is_none());
    }
}
