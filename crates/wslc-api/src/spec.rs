//! What a caller asks of the API, in plain Rust: the containers, processes, volumes and
//! images. Platform independent, so a caller builds and tests these anywhere and only the
//! calls that act on them are Windows only.

use std::fmt;

/// How a container reaches the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Networking {
    /// Loopback only.
    None,
    /// The session's bridge, and through it whatever the session VM reaches.
    Bridged,
}

/// A host directory mounted into a container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bind {
    /// The directory as Windows names it: a drive path, or a `\\wsl.localhost` UNC name.
    pub host: String,
    /// The absolute path inside the container.
    pub container: String,
    /// Mounted read-only.
    pub read_only: bool,
}

/// A session volume mounted into a container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    /// The volume's name in the session.
    pub volume: String,
    /// The absolute path inside the container.
    pub container: String,
    /// Mounted read-only.
    pub read_only: bool,
}

/// A process: what runs, with what environment, where.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessSpec {
    /// The command line, program first.
    pub argv: Vec<String>,
    /// The environment, complete: nothing is inherited from the image beyond what the
    /// runtime itself adds.
    pub env: Vec<(String, String)>,
    /// The working directory, or the image's.
    pub cwd: Option<String>,
}

/// A container: its image, name, network, mounts and first process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerSpec {
    /// The image, by the name the session store knows it by.
    pub image: String,
    /// A name, or one the runtime picks.
    pub name: Option<String>,
    /// How it reaches the network.
    pub networking: Networking,
    /// Host directories.
    pub binds: Vec<Bind>,
    /// Session volumes.
    pub mounts: Vec<Mount>,
    /// The init process; the image's command when `argv` is empty.
    pub init: ProcessSpec,
    /// Removed by the runtime when its init process exits.
    pub auto_remove: bool,
}

/// An image in a session's store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    /// The name it is stored under.
    pub name: String,
    /// The image id: the sha256 of its configuration, as lowercase hex.
    pub id: String,
    /// Its size.
    pub size_bytes: i64,
}

impl ImageInfo {
    /// Whether this is the image `reference` names: by its stored name, or by a name that
    /// differs only in the registry the runtime fills in for a short name.
    #[must_use]
    pub fn answers_to(&self, reference: &str) -> bool {
        let short = |s: &str| {
            let s = s.strip_prefix("docker.io/").unwrap_or(s);
            let s = s.strip_prefix("library/").unwrap_or(s);
            if s.rsplit('/').next().is_some_and(|last| last.contains(':')) || s.contains('@') {
                s.to_string()
            } else {
                format!("{s}:latest")
            }
        };
        short(&self.name) == short(reference)
    }
}

/// An id as `wslc image inspect` prints it, `sha256:<hex>`, or the bare hex, reduced to
/// the hex, lowercase.
#[must_use]
pub fn bare_id(id: &str) -> String {
    id.trim()
        .strip_prefix("sha256:")
        .unwrap_or(id.trim())
        .to_ascii_lowercase()
}

/// A failed call: which, its HRESULT, and the message the API gave when it gave one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// The API function, or what failed before one was called.
    pub call: &'static str,
    /// The HRESULT, or 0 where none was returned.
    pub hr: u32,
    /// The API's message.
    pub message: String,
}

impl Error {
    /// A failure that is not an HRESULT: a missing DLL, a string with a NUL in it.
    #[must_use]
    pub fn other(call: &'static str, message: impl Into<String>) -> Self {
        Self {
            call,
            hr: 0,
            message: message.into(),
        }
    }

    /// The header's name for this HRESULT, when it has one.
    #[must_use]
    pub fn name(&self) -> Option<&'static str> {
        hresult_name(self.hr)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.call)?;
        if self.hr != 0 {
            write!(f, ": 0x{:08x}", self.hr)?;
            if let Some(n) = self.name() {
                write!(f, " {n}")?;
            }
        }
        if !self.message.is_empty() {
            write!(f, ": {}", self.message.trim())?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

/// The WSLC error codes `wslcsdk.h` defines, and the common Windows ones the API returns.
#[must_use]
pub fn hresult_name(hr: u32) -> Option<&'static str> {
    const WSLC: [&str; 19] = [
        "WSLC_E_IMAGE_NOT_FOUND",
        "WSLC_E_CONTAINER_PREFIX_AMBIGUOUS",
        "WSLC_E_CONTAINER_NOT_FOUND",
        "WSLC_E_VOLUME_NOT_FOUND",
        "WSLC_E_CONTAINER_NOT_RUNNING",
        "WSLC_E_CONTAINER_IS_RUNNING",
        "WSLC_E_SESSION_RESERVED",
        "WSLC_E_INVALID_SESSION_NAME",
        "WSLC_E_NETWORK_NOT_FOUND",
        "WSLC_E_WU_SEARCH_FAILED",
        "WSLC_E_SDK_UPDATE_NEEDED",
        "WSLC_E_CONTAINER_DISABLED",
        "WSLC_E_REGISTRY_BLOCKED_BY_POLICY",
        "WSLC_E_VOLUME_NOT_AVAILABLE",
        "WSLC_E_SESSION_NOT_FOUND",
        "WSLC_E_VM_NOT_RUNNING",
        "WSLC_E_EVENTS_LOST",
        "WSLC_E_EVENT_STREAM_FINISHED",
        "WSLC_E_CONTAINER_DELETED",
    ];
    // MAKE_HRESULT(SEVERITY_ERROR, FACILITY_ITF, 0x0600 + n)
    if let Some(name) = hr
        .checked_sub(0x8004_0601)
        .and_then(|n| WSLC.get(n as usize))
    {
        return Some(name);
    }
    match hr {
        0x8007_0002 => Some("ERROR_FILE_NOT_FOUND"),
        0x8007_0005 => Some("E_ACCESSDENIED"),
        0x8007_0020 => Some("ERROR_SHARING_VIOLATION"),
        0x8007_00b7 => Some("ERROR_ALREADY_EXISTS"),
        0x8007_0057 => Some("E_INVALIDARG"),
        0x8000_4001 => Some("E_NOTIMPL"),
        0x8000_4005 => Some("E_FAIL"),
        0x8000_ffff => Some("E_UNEXPECTED"),
        0x8004_01f0 => Some("CO_E_NOTINITIALIZED"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_error_codes_are_named() {
        assert_eq!(hresult_name(0x8004_0601), Some("WSLC_E_IMAGE_NOT_FOUND"));
        assert_eq!(hresult_name(0x8004_0607), Some("WSLC_E_SESSION_RESERVED"));
        assert_eq!(hresult_name(0x8004_0613), Some("WSLC_E_CONTAINER_DELETED"));
        assert_eq!(hresult_name(0x8004_0614), None);
        assert_eq!(hresult_name(0x8004_0600), None);
        assert_eq!(hresult_name(0x8007_0057), Some("E_INVALIDARG"));
        let e = Error {
            call: "WslcCreateContainer",
            hr: 0x8004_0601,
            message: "no such image\r\n".into(),
        };
        assert_eq!(
            e.to_string(),
            "WslcCreateContainer: 0x80040601 WSLC_E_IMAGE_NOT_FOUND: no such image"
        );
        assert_eq!(
            Error::other("wslcsdk.dll", "not found").to_string(),
            "wslcsdk.dll: not found"
        );
    }

    #[test]
    fn an_image_answers_to_its_name_with_or_without_the_default_registry() {
        let i = ImageInfo {
            name: "durable-memory/harness:dev".into(),
            id: "1d37".into(),
            size_bytes: 1,
        };
        assert!(i.answers_to("durable-memory/harness:dev"));
        assert!(i.answers_to("docker.io/durable-memory/harness:dev"));
        assert!(!i.answers_to("durable-memory/harness:other"));
        assert!(!i.answers_to("durable-memory/harness"));
        let latest = ImageInfo {
            name: "docker.io/library/python:latest".into(),
            ..i.clone()
        };
        assert!(latest.answers_to("python"));
        assert!(!latest.answers_to("python:3"));
        let registry = ImageInfo {
            name: "localhost:5000/x".into(),
            ..i
        };
        assert!(
            registry.answers_to("localhost:5000/x:latest"),
            "a port is not a tag"
        );
    }

    #[test]
    fn an_id_is_compared_as_bare_lowercase_hex() {
        assert_eq!(bare_id("sha256:1D37ab\n"), "1d37ab");
        assert_eq!(bare_id("1d37ab"), "1d37ab");
    }
}
