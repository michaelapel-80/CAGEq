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

/// Every effect slot index, in the order Windows evaluates them.
///
/// `register.ps1` cleared the slots CAGEq does not occupy, and the Rust rewrite dropped that.
/// The consequence, found on a real machine: Equalizer APO sitting in slots 5 and 6 while
/// CAGEq sat in 7, **all three running**. Audio was filtered by both corrections at once, so
/// "switch to dry" left EqAPO's still applied and every comparison measured the wrong thing.
///
/// EqAPO does NOT use the EFX slot, so attaching does not displace it — it stacks with it,
/// which is worse because nothing looks broken.
const ALL_SLOTS: [&str; 5] = ["1", "2", "5", "6", "7"];


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
    /// Whether the registered DLL is byte-for-byte the one shipped with *this build* of the
    /// app. `register` always re-copies the shipped DLL over the installed one (see its own
    /// doc), so this is what makes an app update actually take effect on the audio side — but
    /// nothing ever prompted for it, and `next_step` used to consider setup finished forever
    /// once the DLL merely *existed*, with no version or hash check at all. An app update
    /// shipping a fixed/newer CAGEqApo.dll left the OLD one running, attached and apparently
    /// healthy, indefinitely. `true` when there is nothing shipped to compare against (a bare
    /// CLI/dev context) or nothing registered yet — nothing to warn about in either case.
    pub dll_current: bool,
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
    /// Includes `dll_current`: a stale DLL is not "ready" in the sense that matters here, even
    /// though it is still running and still doing something (see `dll_current`'s own doc).
    pub fn machine_ready(&self) -> bool {
        self.registered_dll.is_some() && self.dll_present && self.dll_current && self.gate_open
    }

    /// The single next thing to do, or `None` when this endpoint is fully set up.
    ///
    /// Ordered rather than presented as a checklist of equals, because the order is load
    /// bearing: attaching an endpoint while the gate is shut *appears* to succeed and then
    /// silently does nothing, which is exactly the failure that cost a VM cycle. The UI
    /// should never offer "attach" as an available action before the gate is open.
    ///
    /// A stale DLL (`!dll_current`) re-triggers the *same* `RegisterServer` action a first-time
    /// setup uses — `register` already always re-copies the shipped DLL (see its own doc), so
    /// there is no separate "update" action to add; the app can still tell the two situations
    /// apart for its own wording via `dll_present`/`dll_current` directly, without needing a
    /// distinct step.
    pub fn next_step(&self, endpoint_id: &str) -> Option<Action> {
        if self.registered_dll.is_none() || !self.dll_present || !self.dll_current {
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



/// Where a displaced effect registration is remembered, so `detach` can put it back.
///
/// Attaching writes CAGEq's CLSID into the endpoint's EFX slot — **the same slot Equalizer
/// APO uses**. Overwriting it without recording what was there destroys the user's existing
/// setup silently and permanently: EqAPO stops processing, nothing says why, and detaching
/// CAGEq later leaves the endpoint with no effect at all rather than the one it had.
/// `register.ps1` backed the slots up for exactly this reason; the Rust rewrite dropped it.
const DISPLACED_KEY: &str = r"SOFTWARE\CAGEq\apo\displaced";

/// What CAGEq displaced on `endpoint_id`: `slot=clsid` pairs, comma separated.
#[cfg(windows)]
pub fn displaced_effect(endpoint_id: &str) -> Option<String> {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;
    let id = cageq_apo::config::normalize_endpoint_id(endpoint_id)?;
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(DISPLACED_KEY)
        .ok()?
        .get_value::<String, _>(&id)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

#[cfg(not(windows))]
pub fn displaced_effect(_endpoint_id: &str) -> Option<String> {
    None
}

/// Is this CLSID Equalizer APO's? Used to name what was displaced, since "an effect" is much
/// less useful than "Equalizer APO" when someone is wondering why their EQ stopped.
pub fn describe_effect(clsid: &str) -> String {
    let bare = clsid.trim().trim_start_matches('{').trim_end_matches('}');
    if cageq_backend::EQAPO_APO_CLSIDS
        .iter()
        .any(|c| c.eq_ignore_ascii_case(bare))
    {
        "Equalizer APO".to_string()
    } else {
        format!("another effect ({clsid})")
    }
}
/// One playback endpoint, and what is actually attached to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointStatus {
    pub id: String,
    pub name: String,
    /// CAGEq's own APO is attached here.
    pub cageq: bool,
    /// **Equalizer APO is attached to this same endpoint.** Both would run, so both
    /// corrections apply — the audio is filtered twice and every measurement taken through it
    /// is wrong. Worth reporting loudly rather than leaving to be discovered by ear.
    pub eqapo: bool,
    /// Windows has the endpoint's whole effect chain switched off, so nothing attached to it
    /// runs at all — attaching here looks successful and does nothing.
    pub effects_disabled: bool,
    /// What CAGEq displaced when it attached here, if anything — Equalizer APO uses the same
    /// EFX slot, so attaching takes it over. Recorded so `detach` can put it back, and
    /// reported so the takeover is not invisible.
    pub displaced: Option<String>,
}

impl EndpointStatus {
    /// Would CAGEq and Equalizer APO both process this endpoint?
    pub fn double_filtered(&self) -> bool {
        self.cageq && self.eqapo && !self.effects_disabled
    }
}

/// Every playback endpoint with its per-endpoint state.
///
/// Separate from [`status`] because it enumerates devices, which is slower and can fail,
/// while `status` answers the machine-wide questions from three registry reads.
#[cfg(windows)]
pub fn endpoints() -> Vec<EndpointStatus> {
    // Only .attached/.effects_disabled are read below — dll_current plays no part here, so
    // there is nothing for a caller to pass in.
    let s = status(None);
    cageq_backend::list_render_devices()
        .into_iter()
        .map(|d| EndpointStatus {
            cageq: s.attached.iter().any(|a| a.eq_ignore_ascii_case(&d.id)),
            eqapo: cageq_backend::endpoint_has_apo(&d.id, &cageq_backend::EQAPO_APO_CLSIDS),
            effects_disabled: s.effects_disabled.iter().any(|e| e.eq_ignore_ascii_case(&d.id)),
            displaced: displaced_effect(&d.id),
            id: d.id,
            name: d.name,
        })
        .collect()
}

#[cfg(not(windows))]
pub fn endpoints() -> Vec<EndpointStatus> {
    Vec::new()
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
    /// Remove CAGEq from the machine entirely: detach every endpoint (restoring whatever was
    /// displaced), unregister the COM server, delete the installed DLL.
    ///
    /// Exists because doing that by hand is several commands in an order that matters, and
    /// getting it wrong leaves the machine half-configured in ways that are hard to see —
    /// our CLSID still in a slot, or a registration pointing at a file that is gone.
    Reset,
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
            Action::Reset => vec!["reset".into()],
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
            "reset" => Some(Action::Reset),
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
            Action::Reset => {
                "Remove CAGEq from this machine: detach every device (restoring whatever was \n                 there before), unregister the effect, and delete the installed file."
                    .into()
            }
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

/// `shipped_dll_path`: where *this* caller's copy of the shipped `CAGEqApo.dll` is, if it
/// knows — `None` if it has no way to find one (nothing to compare, not "stale").
///
/// **Not `shipped_dll()`.** That helper resolves the DLL beside `current_exe()`, which is
/// correct for [`register`] (it runs *inside the elevated helper process*, staged next to its
/// own DLL — see `build-apo.ps1`'s doc) but wrong here: `status` runs unelevated, in-process,
/// inside the *main app*, whose own exe is never beside the bundled `apo/` resources. The
/// caller (which holds the Tauri `AppHandle` this crate deliberately does not depend on)
/// resolves the real one via `resource_dir()` and passes it in.
#[cfg(windows)]
pub fn status(shipped_dll_path: Option<&std::path::Path>) -> SetupStatus {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let mut out = SetupStatus::default();
    out.dll_current = true; // overridden below only on a confirmed mismatch — see its own doc

    if let Ok(server) = hklm.open_subkey(format!(r"{CLSID_KEY}\{CLSID}\InprocServer32")) {
        if let Ok(path) = server.get_value::<String, _>("") {
            let path = PathBuf::from(path);
            out.dll_present = path.exists();
            if out.dll_present {
                // Both sides read unelevated (the shipped copy is just a file, and the
                // installed one is world-readable), so this needs no privilege — consistent
                // with the rest of `status()`. `None` from either side (caller couldn't
                // resolve one, or can't hash it) means there is nothing trustworthy to compare,
                // so it stays `true` rather than guessing.
                if let (Some(shipped), Some(installed_hash)) = (shipped_dll_path, file_hash(&path)) {
                    if let Some(shipped_hash) = file_hash(shipped) {
                        out.dll_current = shipped_hash == installed_hash;
                    }
                }
            }
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
pub fn status(_shipped_dll_path: Option<&std::path::Path>) -> SetupStatus {
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
        Action::RegisterServer => secure_config_dir().and_then(|()| regsvr32(false)),
        Action::UnregisterServer => {
            // Detach every endpoint FIRST. Unregistering a COM server that endpoints still
            // point at leaves the machine referencing a CLSID nothing implements: Windows
            // silently skips it, so audio keeps working, but the registry keeps stale
            // references to CAGEq forever and the next register looks like it did nothing.
            // Making the user detach by hand first was busywork for something only this code
            // knows the full list for.
            for id in status(None).attached {
                detach(&id)?;
            }
            regsvr32(true)
        }
        Action::OpenGate => set_gate(true),
        Action::CloseGate => set_gate(false),
        // One restart, after the registry work: audiodg only picks up an attachment when the
        // audio service rebuilds a stream's graph.
        Action::Attach(id) => attach(id).and_then(|()| restart_audio()),
        Action::Detach(id) => detach(id).and_then(|()| restart_audio()),
        // `reset` stops and starts audio itself: it has to touch the DLL, which audiodg holds
        // open while it lives.
        Action::Reset => reset(),
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

    // Audio goes down FIRST, before any file is touched. audiodg maps the APO for as long as
    // it lives, so replacing a registered DLL while audio runs means fighting a file that is
    // in use — which is what made updating the APO a dance of detach, unregister, restart and
    // try again, twice. With the service stopped it is one step, and detaching is not needed
    // at all to swap the binary.
    stop_audio()?;

    let target = if unregister {
        installed_dll()
    } else {
        // Copy the shipped DLL into its machine-wide home and register THAT — see
        // `install_dir` for why it cannot be registered where the app happens to sit.
        // Re-copied every time, so a register after an app update refreshes it while the
        // registry entry keeps pointing at one fixed path.
        let source = shipped_dll()?;
        let dest = installed_dll();
        std::fs::create_dir_all(install_dir())
            .map_err(|e| SetupError::Win32("create install directory", e))?;
        std::fs::copy(&source, &dest).map_err(|e| SetupError::Win32("install the DLL", e))?;
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
        // Bring audio back even on failure — leaving the machine silent because a
        // registration did not take is a far worse outcome than the failure itself.
        let _ = start_audio();
        return Err(SetupError::HelperFailed(status.code().unwrap_or(-1)));
    }
    if unregister {
        // Nothing holds it now, so this simply works; still not fatal if it does not, since
        // the registration — the thing actually asked for — is already gone.
        let _ = std::fs::remove_file(&target);
    }
    start_audio()
}

/// Grant CAGEq's own (unelevated) process write access to the directory its persistent
/// corrections live in — `%ProgramData%\CAGEq\apo`, read by `audiodg` (LocalService) at
/// `LockForProcess` (see `cageq_apo::config::config_dir`'s own doc).
///
/// **This was missing entirely.** The only place this ACL was ever actually set was
/// `cageq-apo/scripts/write-config.ps1 -Elevated` — a manual, run-once-by-hand bring-up script,
/// not part of `RegisterServer`/`Attach`/anything a real install runs. A machine that had never
/// had that script run against it (i.e. every real install, and any dev box that only ever used
/// the in-app wizard) was left with `%ProgramData%\CAGEq\apo` at whatever ACL `fs::create_dir_all`
/// running unelevated happened to produce — not guaranteed to include write access for the very
/// account that needs it — so `apply()`'s config-file write could fail from the very first
/// correction, invisibly to everything except the "could not write" error it surfaces, and
/// nothing in `RegisterServer`/`OpenGate`/`Attach` ever fixed it no matter how many times they
/// ran, since none of them touched this directory at all.
///
/// Protected and reset every run rather than only-if-missing: a directory left more restrictive
/// by an earlier attempt (elevated or not) must not stay that way, and setting the same DACL
/// twice is harmless. SIDs, not names — see the localized-account-names lesson elsewhere in
/// this codebase: `S-1-5-18`/`S-1-5-32-544`/`S-1-5-11` are SYSTEM, Administrators, and
/// Authenticated Users on every locale, where `BUILTIN\Administrators` etc. would not resolve.
/// `D:P` = protected DACL, not inherited from `%ProgramData%`. SYSTEM and Administrators get
/// full control (`FA`); Authenticated Users get `0x1301bf` — FILE_GENERIC_READ|WRITE|EXECUTE
/// plus DELETE, the same mask .NET's `FileSystemRights.Modify` sets (matching
/// `write-config.ps1`'s own choice) — not merely read-only, since CAGEq runs unelevated and has
/// to write here itself. `OICI` (object-inherit, container-inherit) applies both rules to files
/// created inside the directory, not just the directory object. SIDs, not names — see the
/// localized-account-names lesson elsewhere in this codebase: `SY`/`BA`/`AU` (SYSTEM,
/// Administrators, Authenticated Users) resolve on every locale, where spelled-out account
/// names would not.
const CONFIG_DIR_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1301bf;;;AU)";

#[cfg(windows)]
type SdHandle = *mut std::ffi::c_void;

#[cfg(windows)]
#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string_security_descriptor: *const u16,
        string_sd_revision: u32,
        security_descriptor: *mut SdHandle,
        security_descriptor_size: *mut u32,
    ) -> i32;
    fn SetFileSecurityW(file_name: *const u16, security_information: u32, security_descriptor: SdHandle) -> i32;
    fn LocalFree(h_mem: SdHandle) -> SdHandle;
}

#[cfg(windows)]
fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// Build the security descriptor [`CONFIG_DIR_SDDL`] describes. `None` if the string itself
/// fails to parse — checked by its own test, but kept as a real runtime path rather than an
/// `unwrap`, since a Win32 SDDL parser is not something to trust blindly from Rust.
#[cfg(windows)]
fn config_dir_security_descriptor() -> Option<SdHandle> {
    const SDDL_REVISION_1: u32 = 1;
    let sddl = wide(CONFIG_DIR_SDDL);
    let mut psd: SdHandle = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; the callee writes at most one pointer to `psd`.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut psd, std::ptr::null_mut())
    };
    if ok == 0 || psd.is_null() { None } else { Some(psd) }
}

/// Grant CAGEq's own (unelevated) process write access to the directory its persistent
/// corrections live in — `%ProgramData%\CAGEq\apo`, read by `audiodg` (LocalService) at
/// `LockForProcess` (see `cageq_apo::config::config_dir`'s own doc).
///
/// **This was missing entirely.** The only place this ACL was ever actually set was
/// `cageq-apo/scripts/write-config.ps1 -Elevated` — a manual, run-once-by-hand bring-up script,
/// not part of `RegisterServer`/`Attach`/anything a real install runs. A machine that had never
/// had that script run against it (i.e. every real install, and any dev box that only ever used
/// the in-app wizard) was left with `%ProgramData%\CAGEq\apo` at whatever ACL `fs::create_dir_all`
/// running unelevated happened to produce — not guaranteed to include write access for the very
/// account that needs it — so `apply()`'s config-file write could fail from the very first
/// correction, invisibly to everything except the "could not write" error it surfaces, and
/// nothing in `RegisterServer`/`OpenGate`/`Attach` ever fixed it no matter how many times they
/// ran, since none of them touched this directory at all.
///
/// Reset every run rather than only-if-missing: a directory left more restrictive by an earlier
/// attempt (elevated or not) must not stay that way, and setting the same DACL twice is
/// harmless.
#[cfg(windows)]
fn secure_config_dir() -> Result<(), SetupError> {
    let dir = cageq_apo::config::config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| SetupError::Win32("create the config directory", e))?;

    const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
    const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;

    let Some(psd) = config_dir_security_descriptor() else {
        return Err(SetupError::Win32(
            "build the config directory's security descriptor",
            std::io::Error::last_os_error(),
        ));
    };

    let path = wide(dir.as_os_str());
    // SAFETY: `path` is NUL-terminated; `psd` was just built above and is freed below regardless
    // of outcome.
    let applied = unsafe { SetFileSecurityW(path.as_ptr(), DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, psd) };
    let err = std::io::Error::last_os_error();
    unsafe { LocalFree(psd) };
    if applied == 0 {
        return Err(SetupError::Win32("set the config directory's permissions", err));
    }
    Ok(())
}

#[cfg(not(windows))]
fn secure_config_dir() -> Result<(), SetupError> {
    Ok(())
}

/// The DLL as shipped beside the helper, i.e. inside the application's resources.
#[cfg(windows)]
fn shipped_dll() -> Result<PathBuf, SetupError> {
    let exe = std::env::current_exe().map_err(|e| SetupError::Win32("current_exe", e))?;
    let dll = exe.with_file_name("CAGEqApo.dll");
    if dll.exists() { Ok(dll) } else { Err(SetupError::DllMissing) }
}

/// SHA-256 of a file's contents, or `None` if it cannot be read — never a reason to fail a
/// status read (see `status`'s own doc: every field there is a best-effort, unelevated
/// snapshot, and a hash that cannot be computed is exactly as informative as one that mismatches
/// would be misleading, so it is treated as "nothing to compare" rather than "stale").
#[cfg(windows)]
fn file_hash(path: &std::path::Path) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    Some(Sha256::digest(&bytes).into())
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


/// Record what CAGEq displaced on `endpoint_id`.
#[cfg(windows)]
fn remember_displaced(endpoint_id: &str, clsid: &str) -> Result<(), SetupError> {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;
    let Some(id) = cageq_apo::config::normalize_endpoint_id(endpoint_id) else { return Ok(()) };
    let (key, _) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .create_subkey(DISPLACED_KEY)
        .map_err(|e| SetupError::Win32("create the displaced-effect key", e))?;
    key.set_value(&id, &clsid.to_string())
        .map_err(|e| SetupError::Win32("record the displaced effect", e))
}

/// Forget the record once it has been put back.
#[cfg(windows)]
fn forget_displaced(endpoint_id: &str) {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_SET_VALUE};
    let Some(id) = cageq_apo::config::normalize_endpoint_id(endpoint_id) else { return };
    if let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(DISPLACED_KEY, KEY_SET_VALUE)
    {
        let _ = key.delete_value(&id);
    }
}
#[cfg(windows)]
fn attach(endpoint_id: &str) -> Result<(), SetupError> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE};

    let path = fx_key(endpoint_id)?;
    // MMDevices is owned by TrustedInstaller, so even an administrator cannot write here
    // until ownership is taken — see `take_ownership`.
    take_ownership(&path)?;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let fx = hklm
        .open_subkey_with_flags(&path, KEY_QUERY_VALUE | KEY_SET_VALUE)
        .map_err(|_| SetupError::NoSuchEndpoint(endpoint_id.to_string()))?;

    // Take over every effect slot, remembering what was in each.
    //
    // Not just our own slot: Windows runs ALL of them, so leaving Equalizer APO in slots 5
    // and 6 while CAGEq occupies 7 means both corrections apply — the audio is filtered
    // twice, "switch to dry" leaves the other one running, and nothing anywhere says so.
    // That silently invalidated every listening comparison made against it.
    //
    // Everything removed is recorded first and put back by `detach`, so this is a takeover
    // for as long as CAGEq is attached, not a deletion.
    let mut displaced: Vec<String> = Vec::new();
    for slot in ALL_SLOTS {
        let name = format!("{FX_CLSID_PROP},{slot}");
        let Ok(existing) = fx.get_value::<String, _>(&name) else { continue };
        let existing = existing.trim().to_string();
        if existing.is_empty() || existing.eq_ignore_ascii_case(CLSID) {
            continue;
        }
        displaced.push(format!("{slot}={existing}"));
        if slot != EFX_SLOT {
            let _ = fx.delete_value(&name);
        }
    }
    if !displaced.is_empty() {
        remember_displaced(endpoint_id, &displaced.join(","))?;
    }

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

    // No restart here: `perform` does it once, so unregistering several endpoints does not
    // stop and start the audio service once per endpoint.
    Ok(())
}

#[cfg(windows)]
fn detach(endpoint_id: &str) -> Result<(), SetupError> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE};

    let path = fx_key(endpoint_id)?;
    take_ownership(&path)?;
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let fx = hklm
        .open_subkey_with_flags(&path, KEY_QUERY_VALUE | KEY_SET_VALUE)
        .map_err(|_| SetupError::NoSuchEndpoint(endpoint_id.to_string()))?;

    // Only our own slot value is removed. The processing-modes declaration is left alone:
    // it is not ours specifically, another APO in that slot needs it, and removing something
    // we did not necessarily create is how a "clean uninstall" breaks somebody's audio.
    // Put every displaced effect back, not just our own slot. Detaching should return the
    // machine to how it was found — leaving Equalizer APO removed because CAGEq once took
    // the endpoint over would be the same silent damage in the other direction.
    let slot = format!("{FX_CLSID_PROP},{EFX_SLOT}");
    if fx.get_value::<String, _>(&slot).is_ok_and(|v| v.trim().eq_ignore_ascii_case(CLSID)) {
        let _ = fx.delete_value(&slot);
    }
    if let Some(record) = displaced_effect(endpoint_id) {
        for entry in record.split(',') {
            let Some((s, clsid)) = entry.split_once('=') else { continue };
            if !ALL_SLOTS.contains(&s) || clsid.trim().is_empty() {
                continue;
            }
            fx.set_value(format!("{FX_CLSID_PROP},{s}"), &clsid.trim().to_string())
                .map_err(|e| SetupError::Win32("restore a displaced effect", e))?;
        }
        forget_displaced(endpoint_id);
    }
    Ok(())
}

/// Stop the audio service, which takes `audiodg` down with it.
///
/// **This is what releases the DLL.** `audiodg` maps an APO for as long as it is alive, so a
/// registered DLL cannot be replaced while audio is running — the file is in use. Stopping
/// first is the difference between "replace the DLL" being one step and being a dance of
/// detach, unregister, restart, retry.
#[cfg(windows)]
fn stop_audio() -> Result<(), SetupError> {
    use std::process::Command;
    // Failure is not fatal: the service may already be stopped, which is the state we want.
    let _ = Command::new("net")
        .args(["stop", "audiosrv", "/y"])
        .status()
        .map_err(|e| SetupError::Win32("stop audiosrv", e))?;
    Ok(())
}

/// Start the audio service again. APOs are loaded when a stream's graph is built, so nothing
/// takes effect until this happens *and* something plays.
#[cfg(windows)]
fn start_audio() -> Result<(), SetupError> {
    use std::process::Command;
    Command::new("net")
        .args(["start", "audiosrv"])
        .status()
        .map_err(|e| SetupError::Win32("start audiosrv", e))?;
    Ok(())
}


/// Remove CAGEq from every endpoint and from the machine, then say what is left.
///
/// Scans **every** slot of **every** endpoint for our CLSID rather than trusting a list:
/// the point of a reset is to work when the state is already wrong, and a reset that only
/// undoes what it expected to find is no use precisely when it is needed.
#[cfg(windows)]
#[cfg(windows)]
fn reset() -> Result<(), SetupError> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE};

    stop_audio()?;
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let mut cleared = 0usize;
    let mut restored = 0usize;
    // Every failure is collected rather than swallowed. An earlier version skipped an endpoint
    // it could not open and deleted values with the result ignored, so "reset" reported success
    // while leaving the machine exactly as it was — which is the one outcome a reset must never
    // produce silently.
    let mut problems: Vec<String> = Vec::new();

    for d in cageq_backend::list_render_devices() {
        let path = format!(r"{RENDER_KEY}\{}\FxProperties", d.id);
        // Ownership may already be ours, but a reset has to work on an endpoint it never
        // successfully attached to.
        if let Err(e) = take_ownership(&path) {
            problems.push(format!("{}: could not take ownership: {e}", d.name));
            continue;
        }
        let fx = match hklm.open_subkey_with_flags(&path, KEY_QUERY_VALUE | KEY_SET_VALUE) {
            Ok(fx) => fx,
            Err(e) => {
                problems.push(format!("{}: could not open FxProperties for writing: {e}", d.name));
                continue;
            }
        };

        for slot in ALL_SLOTS {
            let name = format!("{FX_CLSID_PROP},{slot}");
            let is_ours = fx
                .get_value::<String, _>(&name)
                .is_ok_and(|v| v.trim().eq_ignore_ascii_case(CLSID));
            if !is_ours {
                continue;
            }
            match fx.delete_value(&name) {
                Ok(()) => cleared += 1,
                Err(e) => problems.push(format!("{}: slot {slot} would not clear: {e}", d.name)),
            }
        }

        if let Some(record) = displaced_effect(&d.id) {
            for entry in record.split(',') {
                let Some((s, clsid)) = entry.split_once('=') else { continue };
                if !ALL_SLOTS.contains(&s) || clsid.trim().is_empty() {
                    continue;
                }
                match fx.set_value(format!("{FX_CLSID_PROP},{s}"), &clsid.trim().to_string()) {
                    Ok(()) => restored += 1,
                    Err(e) => problems.push(format!("{}: could not restore slot {s}: {e}", d.name)),
                }
            }
            forget_displaced(&d.id);
        }
    }

    // Best-effort, but reported: a reset that stops at the first thing already gone would
    // leave the rest behind.
    let installed = installed_dll();
    if installed.exists() {
        let _ = std::process::Command::new("regsvr32")
            .args(["/s", "/u"])
            .arg(&installed)
            .status();
        if let Err(e) = std::fs::remove_file(&installed) {
            problems.push(format!("could not delete {}: {e}", installed.display()));
        }
    }
    start_audio()?;

    if !problems.is_empty() {
        // Every detail goes into the ERROR, not to stdout. When the helper self-elevates it
        // has no console, and only text carried back through the log reaches the caller —
        // printing the problems would discard exactly the information needed.
        return Err(SetupError::HelperSaid(format!(
            "reset incomplete - cleared {cleared} slot(s), restored {restored}, but:
  {}",
            problems.join("
  "),
        )));
    }
    Ok(())
}
#[cfg(windows)]
fn restart_audio() -> Result<(), SetupError> {
    stop_audio()?;
    start_audio()
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
    // Layout is an ABI here: without repr(C) the compiler may reorder these and
    // AdjustTokenPrivileges would read garbage. The fields are written for the OS, never
    // read back by us, hence the dead_code allowance.
    #[allow(dead_code)]
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

    /// The SDDL must actually parse — a typo here would silently leave the config directory
    /// unsecured (or `secure_config_dir` erroring instead of fixing anything), and the whole
    /// point of this string is the ACL that a real install's very first correction depends on.
    #[test]
    fn the_config_dir_sddl_is_valid() {
        let psd = config_dir_security_descriptor();
        assert!(psd.is_some(), "CONFIG_DIR_SDDL failed to parse: {CONFIG_DIR_SDDL}");
        if let Some(psd) = psd {
            unsafe { LocalFree(psd) };
        }
    }

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
            dll_current: true,
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

    /// **The bug this fixes.** An app update can ship a newer `CAGEqApo.dll` while an OLD one
    /// is still registered, attached, and working — `dll_present` alone cannot tell, since the
    /// stale file is genuinely still there. Before `dll_current`, `next_step` considered this
    /// endpoint fully set up forever: nothing ever re-offered `RegisterServer` (the same action
    /// that already always re-copies the shipped DLL — see its own doc), so the old file just
    /// kept running, unannounced, however many releases later.
    #[test]
    fn a_stale_dll_is_offered_register_again_even_though_everything_else_looks_done() {
        let ep = "{6cafe423-cde5-4ec1-a1e2-e3fcec778349}";
        let stale_but_attached = SetupStatus {
            registered_dll: Some(PathBuf::from("C:/x/CAGEqApo.dll")),
            dll_present: true,
            dll_current: false,
            gate_open: true,
            attached: vec![ep.to_string()],
            effects_disabled: vec![],
        };
        assert!(!stale_but_attached.machine_ready(), "a stale DLL is not \"ready\", even though it is running");
        assert_eq!(
            stale_but_attached.next_step(ep),
            Some(Action::RegisterServer),
            "re-offered exactly the action that refreshes the DLL, ahead of gate/attach checks \
             that are already satisfied and would otherwise make this read as fully done",
        );
    }

    /// `file_hash` itself, isolated from the registry reads `status()` wraps it in: identical
    /// content hashes equal regardless of path, a single changed byte must not, and a missing
    /// file is `None` (a hard failure here would take down the whole unelevated status read,
    /// which is the one thing this module's own doc insists must always stay available).
    #[test]
    fn file_hash_distinguishes_content_not_missing_files() {
        let dir = std::env::temp_dir().join(format!("cageq-apo-backend-hash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let a = dir.join("a.bin");
        let b_same = dir.join("b-same.bin");
        let c_different = dir.join("c-different.bin");
        std::fs::write(&a, b"CAGEqApo build 1").unwrap();
        std::fs::write(&b_same, b"CAGEqApo build 1").unwrap();
        std::fs::write(&c_different, b"CAGEqApo build 2").unwrap();

        let hash_a = file_hash(&a).expect("a real file must hash");
        assert_eq!(hash_a, file_hash(&b_same).unwrap(), "identical content must hash equal across paths");
        assert_ne!(hash_a, file_hash(&c_different).unwrap(), "different content must hash different");
        assert_eq!(file_hash(&dir.join("does-not-exist.bin")), None, "a missing file is None, not an error");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An endpoint with the effect chain switched off needs fixing even when the APO is
    /// already attached to it — attaching alone achieves nothing there.
    #[test]
    fn an_endpoint_with_effects_disabled_still_needs_work() {
        let ep = "{6cafe423-cde5-4ec1-a1e2-e3fcec778349}";
        let status = SetupStatus {
            registered_dll: Some(PathBuf::from("C:/x/CAGEqApo.dll")),
            dll_present: true,
            dll_current: true,
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
        let s = status(None);
        assert!(s.attached.windows(2).all(|w| w[0] <= w[1]), "attached should be sorted");
    }
}
