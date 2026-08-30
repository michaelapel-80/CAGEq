//! Getting CAGEq's APO installed, attached, and back off again.
//!
//! ## Why this is split in two
//! Setup divides along a line that is not about installers versus wizards:
//!
//! * **Machine-wide and one-time** — registering the COM server, the
//!   `DisableProtectedAudioDG` gate, the config directory's ACL. An installer can do these,
//!   and they are the same for everyone.
//! * **Per-endpoint and recurring** — attaching the APO to *one device*. This cannot be
//!   install-time work: at install nobody knows which endpoint the user wants, and the answer
//!   changes afterwards when they buy a DAC or switch headphones. It has to live in the app.
//!
//! ## Reading is free; changing is not
//! Every question in [`SetupStatus`] is answered by reading `HKLM`, which any user can do. So
//! the app can show an honest, complete picture of what is set up **without ever prompting**,
//! and spend a UAC prompt only when the user actually asks for a change. That is what makes a
//! wizard tolerable rather than a sequence of dialogs.
//!
//! ## The elevated half
//! [`Action`] values are performed by a small separate executable (`cageq-apo-setup.exe`)
//! launched with the `runas` verb — see [`run_elevated`]. Deliberately *not* by elevating
//! CAGEq itself: a Tauri app running as administrator means a webview running as
//! administrator, which is a bad trade for a handful of registry writes.

use std::path::PathBuf;

/// CAGEq's APO CLSID. Must match `CLSID_CageqApo` in `cageq-apo/shim/cageq_apo.cpp` and
/// `scripts/register.ps1`.
pub const CLSID: &str = "{530052E1-2CD4-400A-AC2B-0D19273AD5B7}";

const CLSID_KEY: &str = r"SOFTWARE\Classes\CLSID";
const AUDIO_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Audio";
const DG_VALUE: &str = "DisableProtectedAudioDG";
const RENDER_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render";

/// Effect-CLSID property, per slot. `,7` is EFX — the slot Equalizer APO uses and the only
/// one the spike could get audiodg to load from (SFX `,5` and LFX `,1` were silently skipped).
const FX_CLSID_PROP: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}";
/// Supported-processing-modes property, per slot.
const FX_MODES_PROP: &str = "{d3993a3f-99c2-4402-b5ec-a92a0367664b}";
/// `PKEY_AudioEndpoint_Disable_SysFx` — set means Windows bypasses the endpoint's whole
/// effect chain, so no APO runs at all.
const FX_DISABLE_SYSFX: &str = "{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5";
/// `AUDIO_SIGNALPROCESSINGMODE_DEFAULT`.
const MODE_DEFAULT: &str = "{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}";
/// The slot to attach in, as an index into the per-slot property names above.
const EFX_SLOT: &str = "7";


/// Where the APO DLL is installed, machine-wide: `%ProgramFiles%\CAGEq`.
///
/// **Not** the application's own directory, for two independent reasons — either alone would
/// be enough:
///
/// * `audiodg` runs as **LocalService**, and Tauri's default NSIS install is **per-user**
///   into `%LOCALAPPDATA%`. A service account cannot read another account's profile, so a DLL
///   left in the app folder would simply never load — and audiodg reports nothing when it
///   skips an APO, so the failure would be silent.
/// * A DLL loaded into a service process **must not be writable by unprivileged users**.
///   Anywhere the user can write is somewhere an attacker running as that user can swap the
///   DLL that audiodg then loads, which turns a convenience into privilege escalation.
///   `%ProgramFiles%` is administrator-write, everyone-read by default, which is exactly the
///   shape required.
///
/// Copying it out also decouples the APO from the app's install location, so updating or
/// moving CAGEq cannot leave a registration pointing at a file that is no longer there.
pub fn install_dir() -> PathBuf {
    std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
        .join("CAGEq")
}

/// The installed DLL's path — what gets registered, and what `status()` reports.
pub fn installed_dll() -> PathBuf {
    install_dir().join("CAGEqApo.dll")
}
/// Everything the UI needs to describe the current setup, all of it readable unelevated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupStatus {
    /// Path to the registered `CAGEqApo.dll`, if the COM server is registered at all.
    pub registered_dll: Option<PathBuf>,
    /// Whether the registered DLL is actually present on disk. A registration pointing at a
    /// deleted file looks fine in the registry and fails silently at load — worth telling
    /// apart from "not registered".
    pub dll_present: bool,
    /// `DisableProtectedAudioDG = 1`. **Without this the APO will not load at all**: Windows'
    /// APO signature check rejects unsigned *and* self-signed DLLs, so this key is the gate
    /// (verified on the VM). Equalizer APO's own installer sets exactly the same value.
    pub gate_open: bool,
    /// Endpoints this APO is currently attached to.
    pub attached: Vec<String>,
    /// Endpoints where the effect chain is switched off wholesale, so no APO can run there
    /// however it is attached — reported separately because attaching one looks successful
    /// and does nothing.
    pub effects_disabled: Vec<String>,
}

impl SetupStatus {
    /// Is the machine-wide half done — the part an installer would normally have handled?
    pub fn machine_ready(&self) -> bool {
        self.registered_dll.is_some() && self.dll_present && self.gate_open
    }

    /// The single next thing to do, or `None` when this endpoint is fully set up.
    ///
    /// Ordered rather than presented as a checklist of equals, because the order is load
    /// bearing: attaching an endpoint while the gate is shut *appears* to succeed and then
    /// silently does nothing, which is exactly the failure that cost a VM cycle. The UI
    /// should never offer "attach" as an available action before the gate is open.
    pub fn next_step(&self, endpoint_id: &str) -> Option<Action> {
        if self.registered_dll.is_none() || !self.dll_present {
            return Some(Action::RegisterServer);
        }
        if !self.gate_open {
            return Some(Action::OpenGate);
        }
        if self.effects_disabled.iter().any(|e| e == endpoint_id) {
            return Some(Action::Attach(endpoint_id.to_string()));
        }
        if !self.attached.iter().any(|a| a == endpoint_id) {
            return Some(Action::Attach(endpoint_id.to_string()));
        }
        None
    }
}

/// A change that needs administrator rights. Performed by the helper executable, never
/// in-process — see the module doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Register the COM server (`regsvr32 CAGEqApo.dll`).
    RegisterServer,
    UnregisterServer,
    /// Set `DisableProtectedAudioDG=1`. Machine-wide, and the one step that reduces a
    /// security mitigation — it deserves its own consent in the UI rather than being folded
    /// into a general "set up" button.
    OpenGate,
    CloseGate,
    /// Attach to one endpoint, and clear anything that would stop it running there.
    Attach(String),
    Detach(String),
}

impl Action {
    /// The helper's command line for this action.
    pub fn argv(&self) -> Vec<String> {
        match self {
            Action::RegisterServer => vec!["register".into()],
            Action::UnregisterServer => vec!["unregister".into()],
            Action::OpenGate => vec!["open-gate".into()],
            Action::CloseGate => vec!["close-gate".into()],
            Action::Attach(id) => vec!["attach".into(), id.clone()],
            Action::Detach(id) => vec!["detach".into(), id.clone()],
        }
    }

    /// Parse the helper's command line. `None` for anything unrecognised — the helper runs
    /// elevated, so it refuses to guess at what it was asked to do.
    pub fn from_argv(args: &[String]) -> Option<Action> {
        match args.first().map(String::as_str)? {
            "register" => Some(Action::RegisterServer),
            "unregister" => Some(Action::UnregisterServer),
            "open-gate" => Some(Action::OpenGate),
            "close-gate" => Some(Action::CloseGate),
            "attach" => args.get(1).map(|id| Action::Attach(id.clone())),
            "detach" => args.get(1).map(|id| Action::Detach(id.clone())),
            _ => None,
        }
    }

    /// One line describing what the user is about to authorise. Shown *before* the UAC
    /// prompt, because Windows' own dialog only names the executable.
    pub fn describe(&self) -> String {
        match self {
            Action::RegisterServer => "Register CAGEq's audio effect with Windows.".into(),
            Action::UnregisterServer => "Remove CAGEq's audio effect registration.".into(),
            Action::OpenGate => {
                "Allow Windows to load audio effects that are not signed by Microsoft \
                 (DisableProtectedAudioDG). Equalizer APO's installer sets the same option. \
                 Apps that require a protected audio path may refuse to play while it is set."
                    .into()
            }
            Action::CloseGate => {
                "Restore Windows' requirement that audio effects be signed by Microsoft. \
                 CAGEq's APO will stop loading."
                    .into()
            }
            Action::Attach(id) => format!("Attach CAGEq's audio effect to device {id}."),
            Action::Detach(id) => format!("Remove CAGEq's audio effect from device {id}."),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("'{0}' is not an endpoint GUID")]
    BadEndpointId(String),
    #[error("no endpoint {0} on this machine")]
    NoSuchEndpoint(String),
    #[error("CAGEqApo.dll not found next to the setup helper")]
    DllMissing,
    #[error("{0} failed: {1}")]
    Win32(&'static str, std::io::Error),
    #[error("the user declined the elevation prompt")]
    Declined,
    #[error("setup helper exited with code {0}")]
    HelperFailed(i32),
    /// The helper failed and said why. Carried through so a wizard can show the reason
    /// rather than an exit code.
    #[error("{0}")]
    HelperSaid(String),
    #[error("cageq-apo-setup.exe not found next to the application")]
    HelperMissing,
}

// ---------------------------------------------------------------------------
// Status — pure reads, no elevation
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub fn status() -> SetupStatus {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let mut out = SetupStatus::default();

    if let Ok(server) = hklm.open_subkey(format!(r"{CLSID_KEY}\{CLSID}\InprocServer32")) {
        if let Ok(path) = server.get_value::<String, _>("") {
            let path = PathBuf::from(path);
            out.dll_present = path.exists();
            out.registered_dll = Some(path);
        }
    }

    out.gate_open = hklm
        .open_subkey(AUDIO_KEY)
        .and_then(|k| k.get_value::<u32, _>(DG_VALUE))
        .map(|v| v == 1)
        .unwrap_or(false);

    if let Ok(render) = hklm.open_subkey_with_flags(RENDER_KEY, KEY_READ) {
        for id in render.enum_keys().flatten() {
            let Ok(fx) = render.open_subkey(format!(r"{id}\FxProperties")) else { continue };
            let slot = format!("{FX_CLSID_PROP},{EFX_SLOT}");
            if fx
                .get_value::<String, _>(&slot)
                .is_ok_and(|v| v.trim().eq_ignore_ascii_case(CLSID))
            {
                out.attached.push(id.clone());
            }
            // Any present value means the chain is bypassed; the value itself is a
            // PROPVARIANT blob, so its type varies and only presence is meaningful here.
            if fx.get_raw_value(FX_DISABLE_SYSFX).is_ok() {
                out.effects_disabled.push(id);
            }
        }
    }
    out.attached.sort();
    out.effects_disabled.sort();
    out
}

#[cfg(not(windows))]
pub fn status() -> SetupStatus {
    SetupStatus::default()
}

// ---------------------------------------------------------------------------
// Performing an action — runs *inside the elevated helper*
// ---------------------------------------------------------------------------

/// Carry out `action`. **Requires administrator rights**; called by the helper executable,
/// not by the app.
#[cfg(windows)]
pub fn perform(action: &Action) -> Result<(), SetupError> {
    match action {
        Action::RegisterServer => regsvr32(false),
        Action::UnregisterServer => regsvr32(true),
        Action::OpenGate => set_gate(true),
        Action::CloseGate => set_gate(false),
        Action::Attach(id) => attach(id),
        Action::Detach(id) => detach(id),
    }
}

#[cfg(not(windows))]
pub fn perform(_action: &Action) -> Result<(), SetupError> {
    Err(SetupError::Win32(
        "setup",
        std::io::Error::other("Windows only"),
    ))
}

#[cfg(windows)]
fn regsvr32(unregister: bool) -> Result<(), SetupError> {
    use std::process::Command;

    let target = if unregister {
        installed_dll()
    } else {
        // Copy the shipped DLL into its machine-wide home first, and register THAT — see
        // `install_dir` for why it cannot be registered where the app happens to be installed.
        // Re-copied on every register so an app update refreshes it; the registry entry points
        // at a fixed path, so nothing else has to change.
        let source = shipped_dll()?;
        let dest = installed_dll();
        std::fs::create_dir_all(install_dir())
            .map_err(|e| SetupError::Win32("create install directory", e))?;
        // A running audiodg holds the old DLL open, so an in-place overwrite fails. Renaming
        // the loaded file out of the way is allowed even while it is mapped, and Windows
        // cleans the stale copy up on the next reboot.
        if dest.exists() && std::fs::copy(&source, &dest).is_err() {
            let parked = dest.with_extension("dll.old");
            let _ = std::fs::remove_file(&parked);
            std::fs::rename(&dest, &parked)
                .map_err(|e| SetupError::Win32("replace the installed DLL", e))?;
            std::fs::copy(&source, &dest)
                .map_err(|e| SetupError::Win32("install the DLL", e))?;
        } else if !dest.exists() {
            std::fs::copy(&source, &dest).map_err(|e| SetupError::Win32("install the DLL", e))?;
        }
        dest
    };

    let mut cmd = Command::new("regsvr32");
    cmd.arg("/s");
    if unregister {
        cmd.arg("/u");
    }
    cmd.arg(&target);
    // `/s` plus an explicit status check: regsvr32 is a GUI-subsystem binary and reports
    // failure in a message box, which nobody will see when it runs from an elevated helper.
    let status = cmd.status().map_err(|e| SetupError::Win32("regsvr32", e))?;
    if !status.success() {
        return Err(SetupError::HelperFailed(status.code().unwrap_or(-1)));
    }
    if unregister {
        // Leave nothing behind, but do not fail the unregister if the file is still mapped —
        // the registration is gone, which is what was asked for.
        let _ = std::fs::remove_file(&target);
    }
    Ok(())
}

/// The DLL as shipped beside the helper, i.e. inside the application's resources.
#[cfg(windows)]
fn shipped_dll() -> Result<PathBuf, SetupError> {
    let exe = std::env::current_exe().map_err(|e| SetupError::Win32("current_exe", e))?;
    let dll = exe.with_file_name("CAGEqApo.dll");
    if dll.exists() { Ok(dll) } else { Err(SetupError::DllMissing) }
}

#[cfg(windows)]
fn set_gate(open: bool) -> Result<(), SetupError> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_SET_VALUE};

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm
        .open_subkey_with_flags(AUDIO_KEY, KEY_SET_VALUE)
        .map_err(|e| SetupError::Win32("open Audio key", e))?;
    if open {
        key.set_value(DG_VALUE, &1u32)
            .map_err(|e| SetupError::Win32("set DisableProtectedAudioDG", e))?;
    } else {
        // Deleted rather than set to 0, so the machine is left as it was found rather than
        // carrying a value CAGEq invented.
        let _ = key.delete_value(DG_VALUE);
    }
    restart_audio()
}

#[cfg(windows)]
fn fx_key(endpoint_id: &str) -> Result<String, SetupError> {
    let id = cageq_apo::config::normalize_endpoint_id(endpoint_id)
        .ok_or_else(|| SetupError::BadEndpointId(endpoint_id.to_string()))?;
    Ok(format!(r"{RENDER_KEY}\{id}\FxProperties"))
}

#[cfg(windows)]
fn attach(endpoint_id: &str) -> Result<(), SetupError> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_SET_VALUE};

    let path = fx_key(endpoint_id)?;
    // MMDevices is owned by TrustedInstaller, so even an administrator cannot write here
    // until ownership is taken — see `take_ownership`.
    take_ownership(&path)?;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let fx = hklm
        .open_subkey_with_flags(&path, KEY_SET_VALUE)
        .map_err(|_| SetupError::NoSuchEndpoint(endpoint_id.to_string()))?;

    // Three values, not one. Missing any of them makes Windows skip the APO with no error
    // anywhere — which is precisely how an earlier build appeared to work only on machines
    // that had once had Equalizer APO installed, silently inheriting its declarations.
    fx.set_value(format!("{FX_CLSID_PROP},{EFX_SLOT}"), &CLSID.to_string())
        .map_err(|e| SetupError::Win32("set effect CLSID", e))?;

    let modes_name = format!("{FX_MODES_PROP},{EFX_SLOT}");
    if fx.get_value::<Vec<String>, _>(&modes_name).is_err() {
        fx.set_value(&modes_name, &vec![MODE_DEFAULT.to_string()])
            .map_err(|e| SetupError::Win32("declare processing modes", e))?;
    }

    // With this present Windows bypasses the endpoint's entire effect chain, so an attached
    // APO simply never runs.
    let _ = fx.delete_value(FX_DISABLE_SYSFX);

    restart_audio()
}

#[cfg(windows)]
fn detach(endpoint_id: &str) -> Result<(), SetupError> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_SET_VALUE};

    let path = fx_key(endpoint_id)?;
    take_ownership(&path)?;
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let fx = hklm
        .open_subkey_with_flags(&path, KEY_SET_VALUE)
        .map_err(|_| SetupError::NoSuchEndpoint(endpoint_id.to_string()))?;

    // Only our own slot value is removed. The processing-modes declaration is left alone:
    // it is not ours specifically, another APO in that slot needs it, and removing something
    // we did not necessarily create is how a "clean uninstall" breaks somebody's audio.
    let slot = format!("{FX_CLSID_PROP},{EFX_SLOT}");
    if fx.get_value::<String, _>(&slot).is_ok_and(|v| v.trim().eq_ignore_ascii_case(CLSID)) {
        let _ = fx.delete_value(&slot);
    }
    restart_audio()
}

#[cfg(windows)]
fn restart_audio() -> Result<(), SetupError> {
    use std::process::Command;
    // audiodg only loads APOs when the audio service builds a stream's graph, so nothing
    // takes effect until this happens.
    let status = Command::new("net")
        .args(["stop", "audiosrv", "/y"])
        .status()
        .map_err(|e| SetupError::Win32("stop audiosrv", e))?;
    let _ = status;
    Command::new("net")
        .args(["start", "audiosrv"])
        .status()
        .map_err(|e| SetupError::Win32("start audiosrv", e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Taking ownership of a TrustedInstaller-owned registry key
// ---------------------------------------------------------------------------

/// Make the local Administrators group the owner of `key_path`, then grant it full control.
///
/// `MMDevices` is owned by TrustedInstaller and its DACL does not grant administrators write
/// access, so elevation alone is not enough — an admin has to take ownership first, which
/// needs `SeTakeOwnershipPrivilege` explicitly enabled on the process token (it is present
/// but *disabled* by default, and Windows will not enable it implicitly).
///
/// Equalizer APO does the same thing for the same reason; there is no gentler route to
/// attaching an APO to an endpoint.
#[cfg(windows)]
fn take_ownership(key_path: &str) -> Result<(), SetupError> {
    use std::io::Error;

    // --- minimal Win32 surface, hand-declared rather than pulling in a crate -------------
    type Handle = *mut std::ffi::c_void;
    const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;
    const TOKEN_QUERY: u32 = 0x0008;
    const SE_PRIVILEGE_ENABLED: u32 = 0x0002;
    const SE_REGISTRY_KEY: u32 = 4;
    const OWNER_SECURITY_INFORMATION: u32 = 0x0000_0001;
    const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
    const WIN_BUILTIN_ADMINISTRATORS_SID: i32 = 26;
    const ERROR_SUCCESS: u32 = 0;

    #[repr(C)]
    struct Luid {
        low: u32,
        high: i32,
    }
    #[repr(C)]
    struct LuidAndAttributes {
        luid: Luid,
        attributes: u32,
    }
    // `#[repr(C)]` with exactly one entry: the API takes a variable-length array, and one
    // privilege is all this needs. (The PowerShell equivalent has to force Pack=4 here; Rust's
    // C layout already matches.)
    #[repr(C)]
    struct TokenPrivileges {
        count: u32,
        privileges: [LuidAndAttributes; 1],
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
        fn LookupPrivilegeValueW(system: *const u16, name: *const u16, luid: *mut Luid) -> i32;
        fn AdjustTokenPrivileges(
            token: Handle,
            disable_all: i32,
            new_state: *const TokenPrivileges,
            buffer_len: u32,
            previous: *mut TokenPrivileges,
            return_len: *mut u32,
        ) -> i32;
        fn CreateWellKnownSid(
            kind: i32,
            domain_sid: *mut std::ffi::c_void,
            sid: *mut std::ffi::c_void,
            size: *mut u32,
        ) -> i32;
        fn SetNamedSecurityInfoW(
            object_name: *mut u16,
            object_type: u32,
            security_info: u32,
            owner: *mut std::ffi::c_void,
            group: *mut std::ffi::c_void,
            dacl: *mut std::ffi::c_void,
            sacl: *mut std::ffi::c_void,
        ) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn CloseHandle(h: Handle) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    // 1. Enable SeTakeOwnershipPrivilege on this process.
    let mut token: Handle = std::ptr::null_mut();
    // SAFETY: `token` receives a handle we close below.
    let ok = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    };
    if ok == 0 {
        return Err(SetupError::Win32("OpenProcessToken", Error::last_os_error()));
    }

    let name = wide("SeTakeOwnershipPrivilege");
    let mut luid = Luid { low: 0, high: 0 };
    // SAFETY: `name` is NUL-terminated; `luid` is written on success.
    if unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) } == 0 {
        unsafe { CloseHandle(token) };
        return Err(SetupError::Win32("LookupPrivilegeValue", Error::last_os_error()));
    }

    let privs = TokenPrivileges {
        count: 1,
        privileges: [LuidAndAttributes { luid, attributes: SE_PRIVILEGE_ENABLED }],
    };
    // SAFETY: single-entry TOKEN_PRIVILEGES matching `count`.
    let adjusted = unsafe {
        AdjustTokenPrivileges(token, 0, &privs, 0, std::ptr::null_mut(), std::ptr::null_mut())
    };
    // AdjustTokenPrivileges reports success even when it granted nothing, so the real
    // verdict is GetLastError — a plain `!= 0` check here would sail past "not all assigned"
    // and fail later at the write with a far less obvious error.
    let last = Error::last_os_error();
    if adjusted == 0 || last.raw_os_error().unwrap_or(0) != 0 {
        unsafe { CloseHandle(token) };
        return Err(SetupError::Win32("AdjustTokenPrivileges", last));
    }
    unsafe { CloseHandle(token) };

    // 2. Build the Administrators SID and make it the owner.
    let mut sid = vec![0u8; 68]; // SECURITY_MAX_SID_SIZE
    let mut size = sid.len() as u32;
    // SAFETY: `sid` is at least SECURITY_MAX_SID_SIZE; `size` is in/out.
    if unsafe {
        CreateWellKnownSid(
            WIN_BUILTIN_ADMINISTRATORS_SID,
            std::ptr::null_mut(),
            sid.as_mut_ptr().cast(),
            &mut size,
        )
    } == 0
    {
        return Err(SetupError::Win32("CreateWellKnownSid", Error::last_os_error()));
    }

    let mut object = wide(&format!(r"MACHINE\{key_path}"));
    // SAFETY: NUL-terminated name; only OWNER is being set, so the ACL pointers stay null.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            object.as_mut_ptr(),
            SE_REGISTRY_KEY,
            OWNER_SECURITY_INFORMATION,
            sid.as_mut_ptr().cast(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(SetupError::Win32(
            "SetNamedSecurityInfo (owner)",
            Error::from_raw_os_error(rc as i32),
        ));
    }

    // 3. Owning the key grants WRITE_DAC implicitly, but not write access to its *values* —
    // so hand the DACL back to inheritance, which is what restores administrator write here
    // without inventing an ACL of our own.
    const UNPROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x2000_0000;
    // SAFETY: as above; a null DACL pointer with UNPROTECTED means "re-inherit".
    let rc = unsafe {
        SetNamedSecurityInfoW(
            object.as_mut_ptr(),
            SE_REGISTRY_KEY,
            DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(SetupError::Win32(
            "SetNamedSecurityInfo (dacl)",
            Error::from_raw_os_error(rc as i32),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Launching the helper elevated — runs in the app
// ---------------------------------------------------------------------------

/// Run `action` in the elevated helper, waiting for it to finish.
///
/// One UAC prompt per call. [`SetupError::Declined`] when the user cancels it, which is an
/// ordinary outcome and not something to report as a failure.
#[cfg(windows)]
pub fn run_elevated(action: &Action) -> Result<String, SetupError> {
    run_elevated_at(&helper_path()?, action)
}

/// Re-launch **this** executable elevated for `action` — how the helper self-elevates when
/// someone runs it by hand from an ordinary prompt.
///
/// Split from [`run_elevated`] because the two resolve a different executable: the app
/// launches the helper beside it, the helper relaunches itself.
#[cfg(windows)]
pub fn run_elevated_self(action: &Action) -> Result<String, SetupError> {
    let me = std::env::current_exe().map_err(|e| SetupError::Win32("current_exe", e))?;
    run_elevated_at(&me, action)
}

/// Is this process running with administrator rights?
///
/// Used to decide whether an action needs a prompt at all. Asked rather than assumed, because
/// the helper is invoked both ways: already elevated from the app's `runas`, and plainly from
/// a console where nothing has elevated it.
#[cfg(windows)]
pub fn is_elevated() -> bool {
    type Handle = *mut std::ffi::c_void;
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_ELEVATION: i32 = 20;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
        fn GetTokenInformation(
            token: Handle,
            class: i32,
            info: *mut std::ffi::c_void,
            len: u32,
            ret_len: *mut u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn CloseHandle(h: Handle) -> i32;
    }

    let mut token: Handle = std::ptr::null_mut();
    // SAFETY: `token` receives a handle closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevated: u32 = 0;
    let mut len: u32 = 0;
    // SAFETY: TOKEN_ELEVATION is a single u32.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TOKEN_ELEVATION,
            (&mut elevated as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
            &mut len,
        )
    };
    unsafe { CloseHandle(token) };
    ok != 0 && elevated != 0
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

#[cfg(windows)]
pub fn run_elevated_at(exe: &std::path::Path, action: &Action) -> Result<String, SetupError> {
    use std::os::windows::ffi::OsStrExt;

    type Handle = *mut std::ffi::c_void;
    const SEE_MASK_NOCLOSEPROCESS: u32 = 0x0000_0040;
    const SW_HIDE: i32 = 0;
    const ERROR_CANCELLED: i32 = 1223;
    const INFINITE: u32 = 0xFFFF_FFFF;

    #[repr(C)]
    struct ShellExecuteInfoW {
        cb_size: u32,
        mask: u32,
        hwnd: Handle,
        verb: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show: i32,
        inst_app: Handle,
        id_list: *mut std::ffi::c_void,
        class: *const u16,
        key_class: Handle,
        hot_key: u32,
        icon_or_monitor: Handle,
        process: Handle,
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteExW(info: *mut ShellExecuteInfoW) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
        fn GetExitCodeProcess(h: Handle, code: *mut u32) -> i32;
        fn CloseHandle(h: Handle) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let file: Vec<u16> = exe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // The child writes what it would have printed to a file named by this token — an elevated
    // process launched through ShellExecuteEx has its own console (here, none at all), so its
    // output cannot reach us any other way.
    let token = log_token();
    let log = log_path_for(&token).ok_or(SetupError::HelperMissing)?;
    let _ = std::fs::remove_file(&log);
    let mut argv = action.argv();
    argv.push("--log".into());
    argv.push(token);
    // Quoted: an endpoint GUID is brace-wrapped and paths may contain spaces.
    let params = wide(&argv.iter().map(quoted).collect::<Vec<_>>().join(" "));
    let verb = wide("runas"); // the elevation prompt

    let mut info = ShellExecuteInfoW {
        cb_size: std::mem::size_of::<ShellExecuteInfoW>() as u32,
        mask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: std::ptr::null_mut(),
        verb: verb.as_ptr(),
        file: file.as_ptr(),
        parameters: params.as_ptr(),
        directory: std::ptr::null(),
        show: SW_HIDE,
        inst_app: std::ptr::null_mut(),
        id_list: std::ptr::null_mut(),
        class: std::ptr::null(),
        key_class: std::ptr::null_mut(),
        hot_key: 0,
        icon_or_monitor: std::ptr::null_mut(),
        process: std::ptr::null_mut(),
    };

    // SAFETY: every pointer above outlives the call.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let err = std::io::Error::last_os_error();
        return if err.raw_os_error() == Some(ERROR_CANCELLED) {
            Err(SetupError::Declined)
        } else {
            Err(SetupError::Win32("ShellExecuteEx", err))
        };
    }

    // SAFETY: SEE_MASK_NOCLOSEPROCESS means `process` is a handle we own and must close.
    let mut code: u32 = 0;
    unsafe {
        WaitForSingleObject(info.process, INFINITE);
        GetExitCodeProcess(info.process, &mut code);
        CloseHandle(info.process);
    }
    // Whatever the child printed, so a caller can show the real reason rather than a number.
    let output = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    if code == 0 {
        Ok(output)
    } else if output.trim().is_empty() {
        Err(SetupError::HelperFailed(code as i32))
    } else {
        Err(SetupError::HelperSaid(output.trim().to_string()))
    }
}

/// Quote one argument for a `ShellExecuteEx` parameter string.
fn quoted(a: &String) -> String {
    format!("\"{a}\"")
}

#[cfg(not(windows))]
pub fn run_elevated(_action: &Action) -> Result<String, SetupError> {
    Err(SetupError::HelperMissing)
}


/// Where the elevated child writes what it would have printed.
///
/// An elevated process launched through `ShellExecuteEx` gets its own console — or, with
/// `SW_HIDE`, none at all — so its stdout never reaches the caller. Without this a failure
/// surfaces as a bare exit code, which is no use to a person at a prompt and no use to a
/// wizard trying to explain what went wrong.
///
/// **The child picks the directory, the parent only supplies a token.** That asymmetry is the
/// point: letting an *unelevated* caller name a path an *elevated* process then writes to is a
/// classic way to turn a helper into an arbitrary-file-write primitive. The token is validated
/// as hex and used only as a filename, rooted in the child's own temp directory.
pub fn log_path_for(token: &str) -> Option<PathBuf> {
    if token.is_empty()
        || token.len() > 32
        || !token.chars().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    Some(std::env::temp_dir().join(format!("cageq-apo-setup-{token}.log")))
}

/// A token for [`log_path_for`]. Not a secret — it only stops two concurrent runs colliding,
/// and the security property comes from the validation above, not from unpredictability.
fn log_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    format!("{:08x}{:08x}", std::process::id(), nanos & 0xffff_ffff)
}
/// Where the helper executable lives: beside the application.
pub fn helper_path() -> Result<PathBuf, SetupError> {
    let exe = std::env::current_exe().map_err(|e| SetupError::Win32("current_exe", e))?;
    let helper = exe.with_file_name("cageq-apo-setup.exe");
    if helper.exists() { Ok(helper) } else { Err(SetupError::HelperMissing) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log token names a file an ELEVATED process writes, chosen by an UNELEVATED one.
    /// If the parent could supply a path rather than a token, the helper would become an
    /// arbitrary-file-write primitive for anything running as the user — so the token is
    /// validated as hex and only ever used as a filename inside the child's own temp
    /// directory.
    #[test]
    fn the_log_token_cannot_name_a_path() {
        assert!(log_path_for("a1b2c3d4").is_some());

        for bad in [
            "",
            "../../windows/system32/x",
            r"..\..\evil",
            "a/b",
            r"a\b",
            "C:/x",
            "has space",
            "semi;colon",
            "nothex!",
            &"f".repeat(33),
        ] {
            assert!(log_path_for(bad).is_none(), "accepted token {bad:?}");
        }

        // Whatever it does accept stays inside temp, under our own name.
        let p = log_path_for("deadbeef").unwrap();
        assert_eq!(p.parent(), Some(std::env::temp_dir().as_path()));
        assert!(
            p.file_name().unwrap().to_string_lossy().starts_with("cageq-apo-setup-"),
            "{p:?}",
        );
    }

    /// The helper runs elevated, so its argument handling is a trust boundary of sorts: it
    /// must round-trip exactly and refuse anything it does not recognise rather than guessing.
    #[test]
    fn actions_round_trip_through_the_command_line() {
        let cases = [
            Action::RegisterServer,
            Action::UnregisterServer,
            Action::OpenGate,
            Action::CloseGate,
            Action::Attach("{6cafe423-cde5-4ec1-a1e2-e3fcec778349}".into()),
            Action::Detach("{6cafe423-cde5-4ec1-a1e2-e3fcec778349}".into()),
        ];
        for action in cases {
            assert_eq!(Action::from_argv(&action.argv()), Some(action.clone()), "{action:?}");
            assert!(!action.describe().is_empty());
        }
        assert_eq!(Action::from_argv(&[]), None);
        assert_eq!(Action::from_argv(&["nonsense".into()]), None);
        // `attach` without a device must not be read as some other action.
        assert_eq!(Action::from_argv(&["attach".into()]), None);
    }

    /// The step order is the point of `next_step`: attaching an endpoint while the gate is
    /// shut *looks* like it worked and silently does nothing, so the UI must never offer it
    /// first. This is the failure that cost a VM cycle during stage B.
    #[test]
    fn setup_steps_are_offered_in_an_order_that_can_actually_work() {
        let ep = "{6cafe423-cde5-4ec1-a1e2-e3fcec778349}";
        let nothing = SetupStatus::default();
        assert_eq!(nothing.next_step(ep), Some(Action::RegisterServer));
        assert!(!nothing.machine_ready());

        let registered = SetupStatus {
            registered_dll: Some(PathBuf::from("C:/x/CAGEqApo.dll")),
            dll_present: true,
            ..Default::default()
        };
        assert_eq!(registered.next_step(ep), Some(Action::OpenGate), "gate before attach");
        assert!(!registered.machine_ready());

        let gated = SetupStatus { gate_open: true, ..registered.clone() };
        assert_eq!(gated.next_step(ep), Some(Action::Attach(ep.into())));
        assert!(gated.machine_ready());

        let done = SetupStatus { attached: vec![ep.to_string()], ..gated.clone() };
        assert_eq!(done.next_step(ep), None, "fully set up");
        // A different endpoint is still not attached.
        assert!(done.next_step("{11112222-3333-4444-5555-666677778888}").is_some());
    }

    /// A registration pointing at a deleted DLL reads as "registered" in the registry and
    /// fails silently at load. It has to come back as work still to do.
    #[test]
    fn a_registration_pointing_at_a_missing_dll_is_not_ready() {
        let stale = SetupStatus {
            registered_dll: Some(PathBuf::from("C:/gone/CAGEqApo.dll")),
            dll_present: false,
            gate_open: true,
            ..Default::default()
        };
        assert!(!stale.machine_ready());
        assert_eq!(stale.next_step("{6cafe423-cde5-4ec1-a1e2-e3fcec778349}"), Some(Action::RegisterServer));
    }

    /// An endpoint with the effect chain switched off needs fixing even when the APO is
    /// already attached to it — attaching alone achieves nothing there.
    #[test]
    fn an_endpoint_with_effects_disabled_still_needs_work() {
        let ep = "{6cafe423-cde5-4ec1-a1e2-e3fcec778349}";
        let status = SetupStatus {
            registered_dll: Some(PathBuf::from("C:/x/CAGEqApo.dll")),
            dll_present: true,
            gate_open: true,
            attached: vec![ep.to_string()],
            effects_disabled: vec![ep.to_string()],
        };
        assert_eq!(status.next_step(ep), Some(Action::Attach(ep.into())));
    }

    /// Status must be readable without elevation and without panicking, whatever this
    /// machine's registry looks like.
    #[test]
    fn reading_status_needs_no_privileges() {
        let s = status();
        assert!(s.attached.windows(2).all(|w| w[0] <= w[1]), "attached should be sorted");
    }
}
