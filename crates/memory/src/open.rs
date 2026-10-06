//! `amem` with no command: find the web launcher running over this root, or start one,
//! and open the browser on it. What a running server is lives in `state/web.json`: its
//! address and its token. The token outlives the server, so a tab left open, or a hosted
//! page paired with it, keeps working across a restart; deleting the file rotates it.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Relative to the root.
pub const STATE: &str = "state/web.json";

/// The first port tried when none is given, and how many above it.
const PORT: u16 = 4200;
const PORTS: u16 = 10;

/// What `state/web.json` holds.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// The token every request past the static pages carries.
    pub token: String,
    /// Where the last server listened, `http://127.0.0.1:4200`; absent once it stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

fn path(root: &Path) -> PathBuf {
    root.join(STATE)
}

/// The state, if the file is there and reads.
#[must_use]
pub fn load(root: &Path) -> Option<State> {
    serde_json::from_str(&std::fs::read_to_string(path(root)).ok()?).ok()
}

/// Write the state readable by its owner only: it holds the token.
///
/// # Errors
///
/// When the file cannot be written.
pub fn save(root: &Path, state: &State) -> std::io::Result<()> {
    let p = path(root);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(&p)?;
    std::io::Write::write_all(&mut f, serde_json::to_string_pretty(state)?.as_bytes())
}

/// The token in the state, or a new one.
///
/// # Errors
///
/// When there is no randomness to make one.
pub fn token(root: &Path) -> std::io::Result<String> {
    match load(root) {
        Some(s) if s.token.len() >= 32 => Ok(s.token),
        _ => dm_web::access::new_token(),
    }
}

/// Whether a server over this root answers at `url` with this token: the address can
/// outlive the process, or belong by now to something else.
pub async fn answers(url: &str, token: &str, root: &Path) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Some(authority) = url.strip_prefix("http://") else {
        return false;
    };
    let probe = async {
        let mut s = tokio::net::TcpStream::connect(authority).await.ok()?;
        let req = format!(
            "GET /api/health HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        );
        s.write_all(req.as_bytes()).await.ok()?;
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.ok()?;
        let text = String::from_utf8_lossy(&out).into_owned();
        let (head, body) = text.split_once("\r\n\r\n")?;
        if !head.starts_with("HTTP/1.1 200") {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        Some(v["server"] == "amem" && v["root"] == root.display().to_string())
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(2), probe).await,
        Ok(Some(true))
    )
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

/// Open `url` in the browser, or say where it is. The link carries the token, so it is
/// printed for the operator to use and handed only to the system's own opener.
pub fn show(url: &str, open: bool) {
    eprintln!("amem: {url}");
    if open && !browser(url) {
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

    #[test]
    fn the_token_is_kept_and_the_file_is_the_owners() {
        let dir = tempfile::tempdir().unwrap();
        let t = token(dir.path()).unwrap();
        assert_eq!(t.len(), 64);
        save(
            dir.path(),
            &State {
                token: t.clone(),
                url: Some("http://127.0.0.1:4200".into()),
            },
        )
        .unwrap();
        assert_eq!(token(dir.path()).unwrap(), t, "a restart keeps the token");
        assert_eq!(
            load(dir.path()).unwrap().url.as_deref(),
            Some("http://127.0.0.1:4200")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(STATE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn nothing_listening_is_not_a_server() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        assert!(!answers(&url, "t", Path::new("/r")).await);
        assert!(!answers("https://example.org", "t", Path::new("/r")).await);
    }
}
