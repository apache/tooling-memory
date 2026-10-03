//! The WSL containers C API (`wslcsdk.dll`), which Microsoft supports for production
//! where the `wslc` command line is not. Bound by hand from `wslcsdk.h` in the
//! `Microsoft.WSL.Containers` NuGet package: the settings structs are opaque, aligned
//! byte blocks of the sizes the header gives, and every call returns an HRESULT.
//!
//! Windows only, loaded at run time so the binary starts without the DLL. First cut: a
//! session is created and terminated, which proves the binding; running containers
//! through it is the next step, and needs the image provisioned into the API's own
//! session store (pull from a registry or load from a tar), since the `wslc` command
//! line's images are not visible to it.

#![allow(clippy::missing_safety_doc)]

use std::path::Path;

/// `WslcSessionSettings`: 72 bytes, 8-aligned, opaque.
#[repr(C, align(8))]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct WslcSessionSettings {
    _opaque: [u8; 72],
}

#[cfg(windows)]
mod dll {
    use super::*;
    use libloading::{Library, Symbol};
    use std::ffi::c_void;

    type Hresult = i32;
    type Pcwstr = *const u16;
    type Pwstr = *mut u16;
    type WslcSession = *mut c_void;

    type InitSessionSettings =
        unsafe extern "system" fn(Pcwstr, Pcwstr, *mut WslcSessionSettings) -> Hresult;
    type SetSessionSettingsCpuCount =
        unsafe extern "system" fn(*mut WslcSessionSettings, u32) -> Hresult;
    type SetSessionSettingsMemory =
        unsafe extern "system" fn(*mut WslcSessionSettings, u32) -> Hresult;
    type CreateSession = unsafe extern "system" fn(
        *mut WslcSessionSettings,
        *mut WslcSession,
        *mut Pwstr,
    ) -> Hresult;
    type TerminateSession = unsafe extern "system" fn(WslcSession) -> Hresult;
    type ReleaseSession = unsafe extern "system" fn(WslcSession) -> Hresult;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn message(p: Pwstr) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut len = 0;
        // SAFETY: the API returns a NUL-terminated UTF-16 string it allocated.
        unsafe {
            while *p.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
        }
    }

    /// Create a session with `name` and `storage`, then terminate and release it.
    /// Returns the DLL path used and how long the round trip took.
    pub fn session_round_trip(name: &str, storage: &Path) -> Result<String, String> {
        let lib = unsafe { Library::new("wslcsdk.dll") }
            .map_err(|e| format!("wslcsdk.dll: {e} (it ships in the Microsoft.WSL.Containers NuGet package, not with WSL)"))?;
        unsafe {
            let init: Symbol<InitSessionSettings> = lib
                .get(b"WslcInitSessionSettings\0")
                .map_err(|e| e.to_string())?;
            let cpus: Symbol<SetSessionSettingsCpuCount> = lib
                .get(b"WslcSetSessionSettingsCpuCount\0")
                .map_err(|e| e.to_string())?;
            let mem: Symbol<SetSessionSettingsMemory> = lib
                .get(b"WslcSetSessionSettingsMemory\0")
                .map_err(|e| e.to_string())?;
            let create: Symbol<CreateSession> =
                lib.get(b"WslcCreateSession\0").map_err(|e| e.to_string())?;
            let terminate: Symbol<TerminateSession> = lib
                .get(b"WslcTerminateSession\0")
                .map_err(|e| e.to_string())?;
            let release: Symbol<ReleaseSession> = lib
                .get(b"WslcReleaseSession\0")
                .map_err(|e| e.to_string())?;
            let mut settings = WslcSessionSettings { _opaque: [0; 72] };
            let n = wide(name);
            let s = wide(&storage.display().to_string());
            let started = std::time::Instant::now();
            let hr = init(n.as_ptr(), s.as_ptr(), &mut settings);
            if hr < 0 {
                return Err(format!("WslcInitSessionSettings: 0x{:08x}", hr as u32));
            }
            let _ = cpus(&mut settings, 2);
            let _ = mem(&mut settings, 2048);
            let mut session: WslcSession = std::ptr::null_mut();
            let mut err: Pwstr = std::ptr::null_mut();
            let hr = create(&mut settings, &mut session, &mut err);
            if hr < 0 {
                return Err(format!(
                    "WslcCreateSession: 0x{:08x} {}",
                    hr as u32,
                    message(err)
                ));
            }
            let _ = terminate(session);
            let _ = release(session);
            Ok(format!(
                "session {name} created and terminated in {:?}",
                started.elapsed()
            ))
        }
    }
}

/// Prove the binding: create and terminate a session. Windows only.
///
/// # Errors
///
/// When the DLL is missing or a call fails, with the HRESULT.
pub fn check(name: &str, storage: &Path) -> Result<String, String> {
    #[cfg(windows)]
    {
        dll::session_round_trip(name, storage)
    }
    #[cfg(not(windows))]
    {
        let _ = (name, storage);
        Err("the WSL containers C API is Windows only; from a WSL distribution the launcher drives wslc.exe".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_block_matches_the_header() {
        assert_eq!(std::mem::size_of::<WslcSessionSettings>(), 72);
        assert_eq!(std::mem::align_of::<WslcSessionSettings>(), 8);
    }

    #[test]
    #[cfg(not(windows))]
    fn off_windows_the_check_says_so() {
        assert!(check("x", Path::new("/tmp"))
            .unwrap_err()
            .contains("Windows only"));
    }
}
