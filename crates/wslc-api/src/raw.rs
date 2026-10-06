//! The header's types, laid out as `wslcsdk.h` gives them. Compiled everywhere so their
//! layout is tested everywhere; only Windows calls through them.
#![cfg_attr(not(windows), allow(dead_code))]

use std::ffi::{c_char, c_void};

pub(crate) type Pcstr = *const c_char;
pub(crate) type Pcwstr = *const u16;
pub(crate) type Pwstr = *mut u16;
pub(crate) type Handle = *mut c_void;

/// `WslcSessionSettings`: 72 bytes, 8-aligned, opaque.
#[repr(C, align(8))]
pub struct WslcSessionSettings {
    pub(crate) _opaque: [u8; 72],
}

/// `WslcContainerSettings`: 104 bytes, 8-aligned, opaque.
#[repr(C, align(8))]
pub(crate) struct ContainerSettings {
    pub(crate) _opaque: [u8; 104],
}

/// `WslcProcessSettings`: 72 bytes, 8-aligned, opaque.
#[repr(C, align(8))]
pub(crate) struct ProcessSettings {
    pub(crate) _opaque: [u8; 72],
}

/// `WslcContainerVolume`.
#[repr(C)]
pub(crate) struct Volume {
    pub(crate) windows_path: Pcwstr,
    pub(crate) container_path: Pcstr,
    pub(crate) read_only: i32,
}

/// `WslcContainerNamedVolume`.
#[repr(C)]
pub(crate) struct NamedVolume {
    pub(crate) name: Pcstr,
    pub(crate) container_path: Pcstr,
    pub(crate) read_only: i32,
}

/// `WslcVhdRequirements`.
#[repr(C)]
pub(crate) struct VhdRequirements {
    pub(crate) name: Pcstr,
    pub(crate) size_bytes: u64,
    pub(crate) kind: i32,
    pub(crate) flags: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

/// `WslcImageInfo`.
#[repr(C)]
pub(crate) struct ImageInfo {
    pub(crate) name: [c_char; 256],
    pub(crate) sha256: [u8; 32],
    pub(crate) size_bytes: i64,
    pub(crate) created_unix_time: u64,
}

/// `WslcContainerImageProgressCallback`.
pub(crate) type ProgressCallback =
    Option<unsafe extern "system" fn(progress: *const c_void, context: *mut c_void) -> i32>;

/// `WslcPullImageOptions`.
#[repr(C)]
pub(crate) struct PullOptions {
    pub(crate) uri: Pcstr,
    pub(crate) progress: ProgressCallback,
    pub(crate) context: *mut c_void,
    pub(crate) registry_auth: Pcstr,
}

/// `WslcStdIOCallback`.
pub(crate) type IoCallback =
    Option<unsafe extern "system" fn(handle: i32, data: *const u8, len: u32, ctx: *mut c_void)>;
/// `WslcProcessExitCallback`.
pub(crate) type ExitCallback = Option<unsafe extern "system" fn(code: i32, ctx: *mut c_void)>;

/// `WslcProcessCallbacks`.
#[repr(C)]
pub(crate) struct Callbacks {
    pub(crate) on_stdout: IoCallback,
    pub(crate) on_stderr: IoCallback,
    pub(crate) on_exit: ExitCallback,
}

/// `WslcVersion`.
#[repr(C)]
#[derive(Default)]
pub(crate) struct Version {
    pub(crate) major: u32,
    pub(crate) minor: u32,
    pub(crate) revision: u32,
}

pub(crate) const NETWORKING_NONE: i32 = 0;
pub(crate) const NETWORKING_BRIDGED: i32 = 1;
pub(crate) const CONTAINER_FLAG_AUTO_REMOVE: u32 = 0x1;
pub(crate) const START_FLAG_ATTACH: u32 = 0x1;
pub(crate) const DELETE_FLAG_FORCE: u32 = 0x1;
pub(crate) const VHD_DYNAMIC: i32 = 0;
pub(crate) const STDOUT: i32 = 1;
pub(crate) const STDERR: i32 = 2;
pub(crate) const SIGKILL: i32 = 9;
pub(crate) const SIGTERM: i32 = 15;
/// `WslcContainerState`: running.
pub(crate) const STATE_RUNNING: i32 = 2;

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, size_of};

    #[test]
    fn the_opaque_blocks_match_the_header() {
        assert_eq!(size_of::<WslcSessionSettings>(), 72);
        assert_eq!(align_of::<WslcSessionSettings>(), 8);
        assert_eq!(size_of::<ContainerSettings>(), 104);
        assert_eq!(align_of::<ContainerSettings>(), 8);
        assert_eq!(size_of::<ProcessSettings>(), 72);
        assert_eq!(align_of::<ProcessSettings>(), 8);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn the_public_structs_have_the_header_layout_on_64_bit() {
        assert_eq!(size_of::<Volume>(), 24);
        assert_eq!(size_of::<NamedVolume>(), 24);
        assert_eq!(size_of::<VhdRequirements>(), 32);
        assert_eq!(std::mem::offset_of!(VhdRequirements, size_bytes), 8);
        assert_eq!(std::mem::offset_of!(VhdRequirements, gid), 28);
        assert_eq!(size_of::<ImageInfo>(), 304);
        assert_eq!(std::mem::offset_of!(ImageInfo, sha256), 256);
        assert_eq!(std::mem::offset_of!(ImageInfo, size_bytes), 288);
        assert_eq!(size_of::<PullOptions>(), 32);
        assert_eq!(size_of::<Callbacks>(), 24);
        assert_eq!(size_of::<Version>(), 12);
    }
}
