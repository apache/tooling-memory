//! Run requests: how the launcher inside the egress container asks the host to run the
//! harness container, over the shared volume both can see.
//!
//! The inside writes `req-<n>.json`; the host runs it and the harness container leaves
//! `req-<n>.exit` (and `req-<n>.out` when captured); the inside writes `done` with the
//! final exit code when it is finished. Files, not sockets, because the two sides are in
//! different kernels with only a filesystem in common.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The shared volume's mount point in both containers.
pub const REQUEST_DIR_INSIDE: &str = "/run/memory";
/// The file whose presence ends the run; its content is the exit code.
pub const DONE: &str = "done";

/// One run of the harness container.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequest {
    /// The command: the relay, then the harness and its arguments.
    pub argv: Vec<String>,
    /// The environment, complete.
    pub env: BTreeMap<String, String>,
    /// The working directory inside.
    pub cwd: String,
    /// The private home, relative to the record root, for the host to mount at
    /// `/home/user`.
    pub home: String,
    /// The record's projection, relative to the record root, for the host to mount
    /// read-only at `/memory`; absent when none was rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<String>,
    /// Attached to the terminal, or run with its output captured.
    pub interactive: bool,
}

impl RunRequest {
    /// The request file for run `n`.
    #[must_use]
    pub fn file(n: usize) -> String {
        format!("req-{n}.json")
    }

    /// Where the harness container leaves run `n`'s exit code.
    #[must_use]
    pub fn exit_file(n: usize) -> String {
        format!("{REQUEST_DIR_INSIDE}/req-{n}.exit")
    }

    /// Where the harness container leaves run `n`'s output, when captured.
    #[must_use]
    pub fn out_file(n: usize) -> String {
        format!("{REQUEST_DIR_INSIDE}/req-{n}.out")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_and_names_its_files() {
        let r = RunRequest {
            argv: vec!["a".into()],
            env: BTreeMap::new(),
            cwd: "/work".into(),
            home: "state/sandbox/claude".into(),
            projection: None,
            interactive: true,
        };
        let back: RunRequest = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        assert_eq!(RunRequest::file(2), "req-2.json");
        assert_eq!(RunRequest::exit_file(2), "/run/memory/req-2.exit");
        assert_eq!(RunRequest::out_file(2), "/run/memory/req-2.out");
    }
}
