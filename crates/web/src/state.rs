//! What a web launcher over a root leaves in `state/web.json`: the token that is its
//! master key, where the last one listened and where its sessions start, and the keys of
//! the browsers paired with it. Owner-only, enforced on every write; a file found readable
//! by others is not trusted, and its token and browser keys are replaced.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Relative to the root.
pub const FILE: &str = "state/web.json";

/// The file's contents.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebState {
    /// The master key: the command line holds it, and with it mints pairing codes.
    pub token: String,
    /// Where the running server listens, `http://127.0.0.1:4200`; absent once it stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Where its sessions start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// Keys handed to paired browsers, each a bearer of its own.
    #[serde(default)]
    pub browsers: Vec<String>,
}

fn path(root: &Path) -> PathBuf {
    root.join(FILE)
}

#[cfg(unix)]
fn too_open(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o077 != 0)
}

#[cfg(not(unix))]
fn too_open(_: &Path) -> bool {
    false
}

/// The state, or a fresh one: with a new token when there is no file, it does not read,
/// or others could read it. The second value says why it is fresh, for the operator.
///
/// # Errors
///
/// When there is no randomness for a token.
pub fn load(root: &Path) -> Result<(WebState, Option<String>), std::io::Error> {
    let p = path(root);
    let fresh = |why: Option<String>| -> Result<(WebState, Option<String>), std::io::Error> {
        Ok((
            WebState {
                token: crate::access::new_token()?,
                ..WebState::default()
            },
            why,
        ))
    };
    let Ok(text) = std::fs::read_to_string(&p) else {
        return fresh(None);
    };
    if too_open(&p) {
        return fresh(Some(format!(
            "{} was readable by others; its token and paired browsers are replaced",
            p.display()
        )));
    }
    match serde_json::from_str::<WebState>(&text) {
        Ok(s) if s.token.len() >= 32 => Ok((s, None)),
        _ => fresh(Some(format!("{} did not read; a new token", p.display()))),
    }
}

/// Write the state readable by its owner only: through a file made that way and renamed
/// over the old one, so a file that was too open is replaced rather than kept.
///
/// # Errors
///
/// When the file cannot be written.
pub fn save(root: &Path, state: &WebState) -> std::io::Result<()> {
    let p = path(root);
    let dir = p.parent().unwrap_or(root);
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".web.json.{}", std::process::id()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    {
        let mut f = o.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // a leftover temporary file keeps its old mode through open; set it outright
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        std::io::Write::write_all(&mut f, serde_json::to_string_pretty(state)?.as_bytes())?;
    }
    std::fs::rename(&tmp, &p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_round_trips_and_a_missing_file_is_a_fresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let (s, why) = load(dir.path()).unwrap();
        assert!(why.is_none());
        assert_eq!(s.token.len(), 64);
        let s = WebState {
            url: Some("http://127.0.0.1:4200".into()),
            browsers: vec!["b".repeat(64)],
            ..s
        };
        save(dir.path(), &s).unwrap();
        assert_eq!(load(dir.path()).unwrap(), (s, None));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_others_can_read_is_not_trusted_and_is_replaced_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (s, _) = load(dir.path()).unwrap();
        save(dir.path(), &s).unwrap();
        let p = dir.path().join(FILE);
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (fresh, why) = load(dir.path()).unwrap();
        assert_ne!(
            fresh.token, s.token,
            "a token others could read is not reused"
        );
        assert!(why.unwrap().contains("readable by others"));
        save(dir.path(), &fresh).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
