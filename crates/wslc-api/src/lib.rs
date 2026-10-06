//! The WSL containers C API (`wslcsdk.dll`), which Microsoft supports for production
//! where the `wslc` command line is not. Bound by hand from `wslcsdk.h` in the
//! `Microsoft.WSL.Containers` NuGet package: the settings structs are opaque, aligned
//! byte blocks of the sizes the header gives, and every call returns an HRESULT.
//!
//! What a caller asks for, the containers, processes, mounts and images, is described in
//! [`spec`] and builds anywhere; the calls that act on it are Windows only, loaded at run
//! time so a binary starts without the DLL.
//!
//! A session created through the API has its own image store, separate from the one the
//! `wslc` command line uses, so an image is provisioned into it before a container can be
//! made from it: loaded from a `wslc save` archive, or pulled from a registry.

#[cfg(windows)]
mod dll;
mod raw;
pub mod spec;

#[cfg(windows)]
pub use dll::{Api, Container, Running, Session, Sink};
pub use raw::WslcSessionSettings;
pub use spec::{Bind, ContainerSpec, Error, ImageInfo, Mount, Networking, ProcessSpec};

/// Why the API is not there, off Windows.
pub const WINDOWS_ONLY: &str =
    "the WSL containers C API is Windows only; from a WSL distribution the launcher drives wslc.exe";
