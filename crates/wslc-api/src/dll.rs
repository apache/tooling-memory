//! The calls into `wslcsdk.dll`. Windows only.
//!
//! Every pointer handed to the API points into storage that outlives the call that takes
//! it: the strings and arrays a settings block refers to are built together and dropped
//! only after the create call that reads the block has returned.

use crate::raw::{self, Handle, Pcstr, Pcwstr, Pwstr};
use crate::spec::{ContainerSpec, Error, ImageInfo, Networking, ProcessSpec};
use libloading::Library;
use std::ffi::{c_void, CStr, CString};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

type Hresult = i32;

/// The thread is already in a single-threaded apartment.
const RPC_E_CHANGED_MODE: u32 = 0x8001_0106;

macro_rules! functions {
    ($($field:ident = $sym:literal ($($arg:ty),*);)*) => {
        /// The API's functions, resolved once; valid while the library is loaded.
        struct Fns {
            $($field: unsafe extern "system" fn($($arg),*) -> Hresult,)*
        }

        impl Fns {
            /// # Safety
            ///
            /// `lib` is `wslcsdk.dll`, whose exports have the signatures the header gives.
            unsafe fn resolve(lib: &Library) -> Result<Self, Error> {
                Ok(Self {
                    $($field: {
                        // SAFETY: the name and signature are the header's (the caller's
                        // contract); the pointer is copied out and used only while `lib`,
                        // which the `Api` owns, stays loaded.
                        let sym = unsafe {
                            lib.get::<unsafe extern "system" fn($($arg),*) -> Hresult>(
                                concat!($sym, "\0").as_bytes(),
                            )
                        };
                        *sym.map_err(|e| Error::other($sym, e.to_string()))?
                    },)*
                })
            }
        }
    };
}

functions! {
    get_version = "WslcGetVersion" (*mut raw::Version);
    init_session_settings = "WslcInitSessionSettings" (Pcwstr, Pcwstr, *mut raw::WslcSessionSettings);
    create_session = "WslcCreateSession" (*mut raw::WslcSessionSettings, *mut Handle, *mut Pwstr);
    terminate_session = "WslcTerminateSession" (Handle);
    release_session = "WslcReleaseSession" (Handle);
    list_images = "WslcListSessionImages" (Handle, *mut *mut raw::ImageInfo, *mut u32);
    load_image = "WslcLoadSessionImageFromFile" (Handle, Pcwstr, *const c_void, *mut Pwstr);
    pull_image = "WslcPullSessionImage" (Handle, *const raw::PullOptions, *mut Pwstr);
    delete_image = "WslcDeleteSessionImage" (Handle, Pcstr, *mut Pwstr);
    create_volume = "WslcCreateSessionVhdVolume" (Handle, *const raw::VhdRequirements, *mut Pwstr);
    delete_volume = "WslcDeleteSessionVhdVolume" (Handle, Pcstr, *mut Pwstr);
    init_container_settings = "WslcInitContainerSettings" (Pcstr, *mut raw::ContainerSettings);
    set_container_name = "WslcSetContainerSettingsName" (*mut raw::ContainerSettings, Pcstr);
    set_container_init = "WslcSetContainerSettingsInitProcess" (*mut raw::ContainerSettings, *mut raw::ProcessSettings);
    set_container_networking = "WslcSetContainerSettingsNetworkingMode" (*mut raw::ContainerSettings, i32);
    set_container_flags = "WslcSetContainerSettingsFlags" (*mut raw::ContainerSettings, u32);
    set_container_volumes = "WslcSetContainerSettingsVolumes" (*mut raw::ContainerSettings, *const raw::Volume, u32);
    set_container_named_volumes = "WslcSetContainerSettingsNamedVolumes" (*mut raw::ContainerSettings, *const raw::NamedVolume, u32);
    create_container = "WslcCreateContainer" (Handle, *const raw::ContainerSettings, *mut Handle, *mut Pwstr);
    open_container = "WslcOpenContainer" (Handle, Pcstr, *mut Handle, *mut Pwstr);
    set_init_callbacks = "WslcSetContainerInitProcessIOCallbacks" (Handle, *const raw::Callbacks, *mut c_void);
    start_container = "WslcStartContainer" (Handle, u32, *mut Pwstr);
    container_state = "WslcGetContainerState" (Handle, *mut i32);
    stop_container = "WslcStopContainer" (Handle, i32, u32, *mut Pwstr);
    delete_container = "WslcDeleteContainer" (Handle, u32, *mut Pwstr);
    release_container = "WslcReleaseContainer" (Handle);
    init_process_settings = "WslcInitProcessSettings" (*mut raw::ProcessSettings);
    set_process_cwd = "WslcSetProcessSettingsWorkingDirectory" (*mut raw::ProcessSettings, Pcstr);
    set_process_cmdline = "WslcSetProcessSettingsCmdLine" (*mut raw::ProcessSettings, *const Pcstr, usize);
    set_process_env = "WslcSetProcessSettingsEnvVariables" (*mut raw::ProcessSettings, *const Pcstr, usize);
    set_process_callbacks = "WslcSetProcessSettingsCallbacks" (*mut raw::ProcessSettings, *const raw::Callbacks, *mut c_void);
    create_process = "WslcCreateContainerProcess" (Handle, *mut raw::ProcessSettings, *mut Handle, *mut Pwstr);
    signal_process = "WslcSignalProcess" (Handle, i32);
    release_process = "WslcReleaseProcess" (Handle);
}

/// The loaded API.
pub struct Api {
    f: Fns,
    co_task_mem_free: unsafe extern "system" fn(*mut c_void),
    _ole32: Library,
    _lib: Library,
}

// SAFETY: the API's handles are documented for use from any thread, and `Fns` holds only
// function pointers into libraries the `Api` keeps loaded.
unsafe impl Send for Api {}
// SAFETY: as above; nothing in `Api` is mutated after loading.
unsafe impl Sync for Api {}

impl Api {
    /// Load `wslcsdk.dll` by the system's search order, which starts beside the binary.
    ///
    /// # Errors
    ///
    /// When the DLL is not found or an export is missing.
    pub fn load() -> Result<Arc<Api>, Error> {
        // SAFETY: loading the vendor's DLL runs its initialisers; that is the point.
        let lib = unsafe { Library::new("wslcsdk.dll") }.map_err(|e| {
            Error::other(
                "wslcsdk.dll",
                format!("{e} (it ships in the Microsoft.WSL.Containers NuGet package, not with WSL; put it beside memory.exe)"),
            )
        })?;
        // SAFETY: ole32 is a system DLL; CoTaskMemFree has this signature.
        let ole32 = unsafe { Library::new("ole32.dll") }
            .map_err(|e| Error::other("ole32.dll", e.to_string()))?;
        // SAFETY: as above.
        let co_task_mem_free =
            *unsafe { ole32.get::<unsafe extern "system" fn(*mut c_void)>(b"CoTaskMemFree\0") }
                .map_err(|e| Error::other("CoTaskMemFree", e.to_string()))?;
        // SAFETY: as above; CoInitializeEx has this signature.
        let co_initialize_ex = *unsafe {
            ole32.get::<unsafe extern "system" fn(*mut c_void, u32) -> Hresult>(b"CoInitializeEx\0")
        }
        .map_err(|e| Error::other("CoInitializeEx", e.to_string()))?;
        // The API is COM underneath. This thread joins the multithreaded apartment and
        // never leaves it, which makes every other thread of the process an implicit
        // member, so the API may be called from any of them. A thread that is already in
        // a single-threaded apartment says so (RPC_E_CHANGED_MODE) and stays there.
        // SAFETY: no reserved pointer; COINIT_MULTITHREADED is 0.
        let hr = unsafe { co_initialize_ex(std::ptr::null_mut(), 0) };
        if hr < 0 && hr as u32 != RPC_E_CHANGED_MODE {
            return Err(Error {
                call: "CoInitializeEx",
                hr: hr as u32,
                message: String::new(),
            });
        }
        // SAFETY: `lib` is wslcsdk.dll.
        let f = unsafe { Fns::resolve(&lib) }?;
        Ok(Arc::new(Api {
            f,
            co_task_mem_free,
            _ole32: ole32,
            _lib: lib,
        }))
    }

    /// The SDK's version, `major.minor.revision`.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub fn version(&self) -> Result<String, Error> {
        let mut v = raw::Version::default();
        // SAFETY: `v` is a valid out-pointer for one WslcVersion.
        let hr = unsafe { (self.f.get_version)(&mut v) };
        self.check("WslcGetVersion", hr, std::ptr::null_mut())?;
        Ok(format!("{}.{}.{}", v.major, v.minor, v.revision))
    }

    /// Turn an HRESULT and the API's optional message into a result, freeing the message.
    fn check(&self, call: &'static str, hr: Hresult, msg: Pwstr) -> Result<(), Error> {
        let message = self.take_wide(msg);
        if hr < 0 {
            return Err(Error {
                call,
                hr: hr as u32,
                message,
            });
        }
        Ok(())
    }

    /// Read and free a UTF-16 string the API allocated.
    fn take_wide(&self, p: Pwstr) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut len = 0;
        // SAFETY: the API returns a NUL-terminated UTF-16 string it allocated with
        // CoTaskMemAlloc, which the caller owns and frees.
        unsafe {
            while *p.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            (self.co_task_mem_free)(p.cast());
            s
        }
    }
}

fn cstr(call: &'static str, s: &str) -> Result<CString, Error> {
    CString::new(s).map_err(|_| Error::other(call, format!("a NUL inside {s:?}")))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A session: a VM with its own image store, volumes and containers.
pub struct Session {
    api: Arc<Api>,
    h: Handle,
}

// SAFETY: a session handle may be used from any thread.
unsafe impl Send for Session {}
// SAFETY: as above; the handle is never mutated.
unsafe impl Sync for Session {}

impl Session {
    /// Create the session `name` with its disk under `storage`, or attach to it when it
    /// is already running.
    ///
    /// # Errors
    ///
    /// When the call fails, with the API's message.
    pub fn create(api: &Arc<Api>, name: &str, storage: &Path) -> Result<Session, Error> {
        let mut settings = raw::WslcSessionSettings { _opaque: [0; 72] };
        let n = wide(name);
        let s = wide(&storage.display().to_string());
        // SAFETY: the strings outlive the call; `settings` is the header's size and alignment.
        let hr = unsafe { (api.f.init_session_settings)(n.as_ptr(), s.as_ptr(), &mut settings) };
        api.check("WslcInitSessionSettings", hr, std::ptr::null_mut())?;
        let mut h: Handle = std::ptr::null_mut();
        let mut err: Pwstr = std::ptr::null_mut();
        // SAFETY: valid settings and out-pointers.
        let hr = unsafe { (api.f.create_session)(&mut settings, &mut h, &mut err) };
        api.check("WslcCreateSession", hr, err)?;
        Ok(Session {
            api: api.clone(),
            h,
        })
    }

    /// The images in this session's store.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub fn images(&self) -> Result<Vec<ImageInfo>, Error> {
        let mut p: *mut raw::ImageInfo = std::ptr::null_mut();
        let mut count = 0u32;
        // SAFETY: valid out-pointers; the array is the caller's to free.
        let hr = unsafe { (self.api.f.list_images)(self.h, &mut p, &mut count) };
        self.api
            .check("WslcListSessionImages", hr, std::ptr::null_mut())?;
        if p.is_null() {
            return Ok(Vec::new());
        }
        // SAFETY: on success `p` points at `count` contiguous WslcImageInfo.
        let infos = unsafe { std::slice::from_raw_parts(p, count as usize) };
        let out = infos.iter().map(image_info).collect();
        // SAFETY: allocated by the API with CoTaskMemAlloc; read above and not after.
        unsafe { (self.api.co_task_mem_free)(p.cast()) };
        Ok(out)
    }

    /// Load the images in a `docker save` archive into this session's store, under the
    /// names the archive gives.
    ///
    /// # Errors
    ///
    /// When the archive cannot be read or loaded.
    pub fn load_image(&self, archive: &Path) -> Result<(), Error> {
        let p = wide(&archive.display().to_string());
        let mut err: Pwstr = std::ptr::null_mut();
        // SAFETY: the path outlives the call; no options, so no callback.
        let hr = unsafe { (self.api.f.load_image)(self.h, p.as_ptr(), std::ptr::null(), &mut err) };
        self.api.check("WslcLoadSessionImageFromFile", hr, err)
    }

    /// Pull an image from a registry into this session's store.
    ///
    /// # Errors
    ///
    /// When the pull fails.
    pub fn pull_image(&self, reference: &str, registry_auth: Option<&str>) -> Result<(), Error> {
        let uri = cstr("WslcPullSessionImage", reference)?;
        let auth = registry_auth
            .map(|a| cstr("WslcPullSessionImage", a))
            .transpose()?;
        let options = raw::PullOptions {
            uri: uri.as_ptr(),
            progress: None,
            context: std::ptr::null_mut(),
            registry_auth: auth.as_ref().map_or(std::ptr::null(), |a| a.as_ptr()),
        };
        let mut err: Pwstr = std::ptr::null_mut();
        // SAFETY: the options and the strings they point at outlive the call.
        let hr = unsafe { (self.api.f.pull_image)(self.h, &options, &mut err) };
        self.api.check("WslcPullSessionImage", hr, err)
    }

    /// Delete an image from this session's store, by name or id.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub fn delete_image(&self, name_or_id: &str) -> Result<(), Error> {
        let n = cstr("WslcDeleteSessionImage", name_or_id)?;
        let mut err: Pwstr = std::ptr::null_mut();
        // SAFETY: the name outlives the call.
        let hr = unsafe { (self.api.f.delete_image)(self.h, n.as_ptr(), &mut err) };
        self.api.check("WslcDeleteSessionImage", hr, err)
    }

    /// Create a dynamically sized volume, `name`, of at most `size_bytes`.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub fn create_volume(&self, name: &str, size_bytes: u64) -> Result<(), Error> {
        let n = cstr("WslcCreateSessionVhdVolume", name)?;
        let req = raw::VhdRequirements {
            name: n.as_ptr(),
            size_bytes,
            kind: raw::VHD_DYNAMIC,
            flags: 0,
            uid: 0,
            gid: 0,
        };
        let mut err: Pwstr = std::ptr::null_mut();
        // SAFETY: the requirements and the name outlive the call.
        let hr = unsafe { (self.api.f.create_volume)(self.h, &req, &mut err) };
        self.api.check("WslcCreateSessionVhdVolume", hr, err)
    }

    /// Delete the volume `name`.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub fn delete_volume(&self, name: &str) -> Result<(), Error> {
        let n = cstr("WslcDeleteSessionVhdVolume", name)?;
        let mut err: Pwstr = std::ptr::null_mut();
        // SAFETY: the name outlives the call.
        let hr = unsafe { (self.api.f.delete_volume)(self.h, n.as_ptr(), &mut err) };
        self.api.check("WslcDeleteSessionVhdVolume", hr, err)
    }

    /// Create a container, not yet started.
    ///
    /// # Errors
    ///
    /// When the image is not in the store, a string has a NUL in it, or the call fails.
    pub fn create_container(&self, spec: &ContainerSpec) -> Result<Container, Error> {
        let built = BuiltContainer::new(&self.api, spec)?;
        let mut h: Handle = std::ptr::null_mut();
        let mut err: Pwstr = std::ptr::null_mut();
        // SAFETY: `built` owns the settings block and everything it points at, and lives
        // past the call.
        let hr =
            unsafe { (self.api.f.create_container)(self.h, &*built.settings, &mut h, &mut err) };
        self.api.check("WslcCreateContainer", hr, err)?;
        drop(built);
        Ok(Container {
            api: self.api.clone(),
            h,
        })
    }

    /// Open a container by name or id.
    ///
    /// # Errors
    ///
    /// `WSLC_E_CONTAINER_NOT_FOUND` when there is none.
    pub fn open_container(&self, name: &str) -> Result<Container, Error> {
        let n = cstr("WslcOpenContainer", name)?;
        let mut h: Handle = std::ptr::null_mut();
        let mut err: Pwstr = std::ptr::null_mut();
        // SAFETY: the name outlives the call; valid out-pointers.
        let hr = unsafe { (self.api.f.open_container)(self.h, n.as_ptr(), &mut h, &mut err) };
        self.api.check("WslcOpenContainer", hr, err)?;
        Ok(Container {
            api: self.api.clone(),
            h,
        })
    }

    /// Terminate the session's VM, and everything running in it.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub fn terminate(&self) -> Result<(), Error> {
        // SAFETY: a live session handle.
        let hr = unsafe { (self.api.f.terminate_session)(self.h) };
        self.api
            .check("WslcTerminateSession", hr, std::ptr::null_mut())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: a live handle, released once.
        unsafe { (self.api.f.release_session)(self.h) };
    }
}

fn image_info(i: &raw::ImageInfo) -> ImageInfo {
    // SAFETY: the name is a NUL-terminated array of 256 chars, as the header declares; a
    // name that fills it without a NUL is cut at the array's end below.
    let bytes = unsafe { std::slice::from_raw_parts(i.name.as_ptr().cast::<u8>(), i.name.len()) };
    let name = CStr::from_bytes_until_nul(bytes).map_or_else(
        |_| String::from_utf8_lossy(bytes).into_owned(),
        |c| c.to_string_lossy().into_owned(),
    );
    ImageInfo {
        name,
        id: i.sha256.iter().map(|b| format!("{b:02x}")).collect(),
        size_bytes: i.size_bytes,
    }
}

/// A process's settings and the strings they point at.
struct BuiltProcess {
    settings: Box<raw::ProcessSettings>,
    _argv: Vec<CString>,
    _argv_ptrs: Vec<Pcstr>,
    _env: Vec<CString>,
    _env_ptrs: Vec<Pcstr>,
    _cwd: Option<CString>,
}

impl BuiltProcess {
    fn new(api: &Api, spec: &ProcessSpec, io: Option<*const Io>) -> Result<Self, Error> {
        const CALL: &str = "WslcInitProcessSettings";
        let mut settings = Box::new(raw::ProcessSettings { _opaque: [0; 72] });
        // SAFETY: a settings block of the header's size and alignment.
        let hr = unsafe { (api.f.init_process_settings)(&mut *settings) };
        api.check(CALL, hr, std::ptr::null_mut())?;
        let argv = spec
            .argv
            .iter()
            .map(|a| cstr(CALL, a))
            .collect::<Result<Vec<_>, _>>()?;
        let env = spec
            .env
            .iter()
            .map(|(k, v)| cstr(CALL, &format!("{k}={v}")))
            .collect::<Result<Vec<_>, _>>()?;
        let cwd = spec.cwd.as_deref().map(|c| cstr(CALL, c)).transpose()?;
        let mut p = BuiltProcess {
            settings,
            _argv_ptrs: argv.iter().map(|a| a.as_ptr()).collect(),
            _env_ptrs: env.iter().map(|e| e.as_ptr()).collect(),
            _argv: argv,
            _env: env,
            _cwd: cwd,
        };
        p.apply(api, io)?;
        Ok(p)
    }

    fn apply(&mut self, api: &Api, io: Option<*const Io>) -> Result<(), Error> {
        let s: *mut raw::ProcessSettings = &mut *self.settings;
        // SAFETY: every pointer is into `self`, which outlives the create call the
        // settings are used in.
        unsafe {
            if !self._argv_ptrs.is_empty() {
                let hr =
                    (api.f.set_process_cmdline)(s, self._argv_ptrs.as_ptr(), self._argv_ptrs.len());
                api.check("WslcSetProcessSettingsCmdLine", hr, std::ptr::null_mut())?;
            }
            if !self._env_ptrs.is_empty() {
                let hr = (api.f.set_process_env)(s, self._env_ptrs.as_ptr(), self._env_ptrs.len());
                api.check(
                    "WslcSetProcessSettingsEnvVariables",
                    hr,
                    std::ptr::null_mut(),
                )?;
            }
            if let Some(c) = &self._cwd {
                let hr = (api.f.set_process_cwd)(s, c.as_ptr());
                api.check(
                    "WslcSetProcessSettingsWorkingDirectory",
                    hr,
                    std::ptr::null_mut(),
                )?;
            }
            if let Some(io) = io {
                let hr = (api.f.set_process_callbacks)(s, &CALLBACKS, io.cast_mut().cast());
                api.check("WslcSetProcessSettingsCallbacks", hr, std::ptr::null_mut())?;
            }
        }
        Ok(())
    }
}

/// A container's settings and everything they point at.
struct BuiltContainer {
    settings: Box<raw::ContainerSettings>,
    _image: CString,
    _name: Option<CString>,
    _hosts: Vec<Vec<u16>>,
    _paths: Vec<CString>,
    _volumes: Vec<raw::Volume>,
    _named: Vec<raw::NamedVolume>,
    _init: Option<BuiltProcess>,
}

impl BuiltContainer {
    fn new(api: &Api, spec: &ContainerSpec) -> Result<Self, Error> {
        const CALL: &str = "WslcInitContainerSettings";
        let image = cstr(CALL, &spec.image)?;
        let mut settings = Box::new(raw::ContainerSettings { _opaque: [0; 104] });
        // SAFETY: the image name outlives the settings; the block is the header's size.
        let hr = unsafe { (api.f.init_container_settings)(image.as_ptr(), &mut *settings) };
        api.check(CALL, hr, std::ptr::null_mut())?;
        let name = spec.name.as_deref().map(|n| cstr(CALL, n)).transpose()?;
        let hosts: Vec<Vec<u16>> = spec.binds.iter().map(|b| wide(&b.host)).collect();
        let mut paths = Vec::new();
        for p in spec.binds.iter().map(|b| &b.container) {
            paths.push(cstr(CALL, p)?);
        }
        for m in &spec.mounts {
            paths.push(cstr(CALL, &m.volume)?);
            paths.push(cstr(CALL, &m.container)?);
        }
        let volumes = spec
            .binds
            .iter()
            .enumerate()
            .map(|(i, b)| raw::Volume {
                windows_path: hosts[i].as_ptr(),
                container_path: paths[i].as_ptr(),
                read_only: i32::from(b.read_only),
            })
            .collect();
        let at = spec.binds.len();
        let named = spec
            .mounts
            .iter()
            .enumerate()
            .map(|(i, m)| raw::NamedVolume {
                name: paths[at + 2 * i].as_ptr(),
                container_path: paths[at + 2 * i + 1].as_ptr(),
                read_only: i32::from(m.read_only),
            })
            .collect();
        let init = if spec.init == ProcessSpec::default() {
            None
        } else {
            Some(BuiltProcess::new(api, &spec.init, None)?)
        };
        let mut c = BuiltContainer {
            settings,
            _image: image,
            _name: name,
            _hosts: hosts,
            _paths: paths,
            _volumes: volumes,
            _named: named,
            _init: init,
        };
        c.apply(api, spec)?;
        Ok(c)
    }

    fn apply(&mut self, api: &Api, spec: &ContainerSpec) -> Result<(), Error> {
        let s: *mut raw::ContainerSettings = &mut *self.settings;
        let none = std::ptr::null_mut();
        let mode = match spec.networking {
            Networking::None => raw::NETWORKING_NONE,
            Networking::Bridged => raw::NETWORKING_BRIDGED,
        };
        let flags = if spec.auto_remove {
            raw::CONTAINER_FLAG_AUTO_REMOVE
        } else {
            0
        };
        // SAFETY: every pointer is into `self`, which outlives the create call.
        unsafe {
            if let Some(n) = &self._name {
                api.check(
                    "WslcSetContainerSettingsName",
                    (api.f.set_container_name)(s, n.as_ptr()),
                    none,
                )?;
            }
            api.check(
                "WslcSetContainerSettingsNetworkingMode",
                (api.f.set_container_networking)(s, mode),
                none,
            )?;
            api.check(
                "WslcSetContainerSettingsFlags",
                (api.f.set_container_flags)(s, flags),
                none,
            )?;
            if !self._volumes.is_empty() {
                let hr = (api.f.set_container_volumes)(
                    s,
                    self._volumes.as_ptr(),
                    self._volumes.len() as u32,
                );
                api.check("WslcSetContainerSettingsVolumes", hr, none)?;
            }
            if !self._named.is_empty() {
                let hr = (api.f.set_container_named_volumes)(
                    s,
                    self._named.as_ptr(),
                    self._named.len() as u32,
                );
                api.check("WslcSetContainerSettingsNamedVolumes", hr, none)?;
            }
            if let Some(p) = &mut self._init {
                let hr = (api.f.set_container_init)(s, &mut *p.settings);
                api.check("WslcSetContainerSettingsInitProcess", hr, none)?;
            }
        }
        Ok(())
    }
}

/// Where a process's output goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sink {
    /// To this process's own stdout or stderr.
    Inherit,
    /// Kept, for [`Running::output`].
    Capture,
    /// Dropped.
    Discard,
}

/// What the callbacks share with the waiter.
struct Io {
    out: Sink,
    err: Sink,
    captured: Mutex<(Vec<u8>, Vec<u8>)>,
    exit: Mutex<Option<i32>>,
    cv: Condvar,
}

impl Io {
    fn take(&self, handle: i32, bytes: &[u8]) {
        let (sink, which) = if handle == raw::STDERR {
            (self.err, 1)
        } else if handle == raw::STDOUT {
            (self.out, 0)
        } else {
            return;
        };
        match sink {
            Sink::Discard => {}
            Sink::Inherit if which == 0 => {
                let mut o = std::io::stdout().lock();
                o.write_all(bytes).ok();
                o.flush().ok();
            }
            Sink::Inherit => {
                std::io::stderr().lock().write_all(bytes).ok();
            }
            Sink::Capture => {
                if let Ok(mut c) = self.captured.lock() {
                    if which == 0 { &mut c.0 } else { &mut c.1 }.extend_from_slice(bytes);
                }
            }
        }
    }

    fn finish(&self, code: i32) {
        if let Ok(mut e) = self.exit.lock() {
            *e = Some(code);
            self.cv.notify_all();
        }
    }
}

/// # Safety
///
/// Called by the API with the context registered alongside, an `Io` kept alive by its
/// `Running`, and a buffer of `len` bytes valid for the call.
unsafe extern "system" fn on_io(handle: i32, data: *const u8, len: u32, ctx: *mut c_void) {
    if ctx.is_null() || data.is_null() || len == 0 {
        return;
    }
    // SAFETY: the contract above.
    let (io, bytes) = unsafe {
        (
            &*ctx.cast::<Io>(),
            std::slice::from_raw_parts(data, len as usize),
        )
    };
    io.take(handle, bytes);
}

/// # Safety
///
/// As [`on_io`]; after it no IO callback follows, by the header's contract.
unsafe extern "system" fn on_exit(code: i32, ctx: *mut c_void) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: the contract above.
    unsafe { &*ctx.cast::<Io>() }.finish(code);
}

static CALLBACKS: raw::Callbacks = raw::Callbacks {
    on_stdout: Some(on_io),
    on_stderr: Some(on_io),
    on_exit: Some(on_exit),
};

/// A process that has been started: wait for it, read what it wrote.
pub struct Running {
    io: Arc<Io>,
    /// The reference handed to the API as the callbacks' context.
    lent: *const Io,
    process: Option<(Arc<Api>, Handle)>,
}

// SAFETY: `Io` is Sync; the process handle may be used from any thread.
unsafe impl Send for Running {}

impl Running {
    fn new(out: Sink, err: Sink) -> Self {
        let io = Arc::new(Io {
            out,
            err,
            captured: Mutex::new((Vec::new(), Vec::new())),
            exit: Mutex::new(None),
            cv: Condvar::new(),
        });
        let lent = Arc::into_raw(io.clone());
        Running {
            io,
            lent,
            process: None,
        }
    }

    /// The exit code, once the process has exited and its output has been delivered.
    #[must_use]
    pub fn wait(&self) -> i32 {
        let mut e = self
            .io
            .exit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while e.is_none() {
            e = self
                .io
                .cv
                .wait(e)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        e.unwrap_or(-1)
    }

    /// The exit code, if the process exits within `timeout`.
    #[must_use]
    pub fn wait_timeout(&self, timeout: Duration) -> Option<i32> {
        let e = self
            .io
            .exit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (e, _) = self
            .io
            .cv
            .wait_timeout_while(e, timeout, |e| e.is_none())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *e
    }

    /// What was captured from stdout and stderr so far.
    #[must_use]
    pub fn output(&self) -> (Vec<u8>, Vec<u8>) {
        self.io
            .captured
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default()
    }

    /// Send the process SIGTERM, or SIGKILL when `kill`.
    ///
    /// # Errors
    ///
    /// When it is not an exec'd process, or the call fails.
    pub fn signal(&self, kill: bool) -> Result<(), Error> {
        let (api, h) = self.process.as_ref().ok_or_else(|| {
            Error::other(
                "WslcSignalProcess",
                "an init process is signalled through its container",
            )
        })?;
        let sig = if kill { raw::SIGKILL } else { raw::SIGTERM };
        // SAFETY: a live process handle.
        let hr = unsafe { (api.f.signal_process)(*h, sig) };
        api.check("WslcSignalProcess", hr, std::ptr::null_mut())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some((api, h)) = self.process.take() {
            // SAFETY: a live handle, released once.
            unsafe { (api.f.release_process)(h) };
        }
        let exited = self.io.exit.lock().map(|e| e.is_some()).unwrap_or(false);
        if exited {
            // SAFETY: `lent` came from `Arc::into_raw` in `new`, and after the exit
            // callback the API makes no further call with it.
            drop(unsafe { Arc::from_raw(self.lent) });
        }
        // otherwise the API may still call back with it, and the one `Io` is left alive
    }
}

/// A container in a session.
pub struct Container {
    api: Arc<Api>,
    h: Handle,
}

// SAFETY: a container handle may be used from any thread.
unsafe impl Send for Container {}
// SAFETY: as above; the handle is never mutated.
unsafe impl Sync for Container {}

impl Container {
    /// Start the container with its init process's output going to `out` and `err`.
    ///
    /// # Errors
    ///
    /// When it cannot be started.
    pub fn start(&self, out: Sink, err: Sink) -> Result<Running, Error> {
        let running = Running::new(out, err);
        let none = std::ptr::null_mut();
        // SAFETY: the callbacks are static and the context is kept alive by `running`.
        let hr = unsafe {
            (self.api.f.set_init_callbacks)(self.h, &CALLBACKS, running.lent.cast_mut().cast())
        };
        self.api
            .check("WslcSetContainerInitProcessIOCallbacks", hr, none)?;
        let mut msg: Pwstr = none.cast();
        // SAFETY: a live container handle.
        let hr = unsafe { (self.api.f.start_container)(self.h, raw::START_FLAG_ATTACH, &mut msg) };
        self.api.check("WslcStartContainer", hr, msg)?;
        Ok(running)
    }

    /// Run a process in the running container.
    ///
    /// # Errors
    ///
    /// When the container is not running, or the call fails.
    pub fn exec(&self, spec: &ProcessSpec, out: Sink, err: Sink) -> Result<Running, Error> {
        let mut running = Running::new(out, err);
        let mut built = BuiltProcess::new(&self.api, spec, Some(running.lent))?;
        let mut h: Handle = std::ptr::null_mut();
        let mut msg: Pwstr = std::ptr::null_mut();
        // SAFETY: `built` owns the settings and what they point at, past the call.
        let hr =
            unsafe { (self.api.f.create_process)(self.h, &mut *built.settings, &mut h, &mut msg) };
        drop(built);
        self.api.check("WslcCreateContainerProcess", hr, msg)?;
        running.process = Some((self.api.clone(), h));
        Ok(running)
    }

    /// Whether the container is running.
    ///
    /// # Errors
    ///
    /// When the call fails, as it does once the container is gone.
    pub fn is_running(&self) -> Result<bool, Error> {
        let mut state = 0;
        // SAFETY: a live handle and a valid out-pointer.
        let hr = unsafe { (self.api.f.container_state)(self.h, &mut state) };
        self.api
            .check("WslcGetContainerState", hr, std::ptr::null_mut())?;
        Ok(state == raw::STATE_RUNNING)
    }

    /// Stop the container: SIGTERM, then SIGKILL after `timeout_seconds`.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub fn stop(&self, timeout_seconds: u32) -> Result<(), Error> {
        let mut msg: Pwstr = std::ptr::null_mut();
        // SAFETY: a live handle.
        let hr =
            unsafe { (self.api.f.stop_container)(self.h, raw::SIGTERM, timeout_seconds, &mut msg) };
        self.api.check("WslcStopContainer", hr, msg)
    }

    /// Delete the container, stopping it first.
    ///
    /// # Errors
    ///
    /// When the call fails.
    pub fn delete(&self) -> Result<(), Error> {
        let mut msg: Pwstr = std::ptr::null_mut();
        // SAFETY: a live handle.
        let hr = unsafe { (self.api.f.delete_container)(self.h, raw::DELETE_FLAG_FORCE, &mut msg) };
        self.api.check("WslcDeleteContainer", hr, msg)
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        // SAFETY: a live handle, released once.
        unsafe { (self.api.f.release_container)(self.h) };
    }
}
