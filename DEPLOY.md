# Deploying CAGEq

**Platform:** Windows x64.

The installer is **self-contained**: it bundles the Python DSP sidecar frozen with
PyInstaller (numpy/scipy/matplotlib/autoeq), so the target machine needs **no Python**.

## For end users (one-click)

1. **Install Equalizer APO**, run its Configurator (`DeviceSelector.exe`), tick the
   output device you want to correct, and **reboot**. (CAGEq warns if a device isn't
   EqAPO-enabled.)
2. **Run `CAGEq_<version>_x64-setup.exe`.** It installs CAGEq and bootstraps the
   Microsoft Edge WebView2 runtime if missing (Windows 11 already has it).
3. Launch CAGEq. The status line should read `sidecar: AutoEq DSP, bundled (…)`. Pick a
   device, a headphone + measurement, a target, and **Apply** — audio changes.

That's it. First launch fetches the ~6800-headphone AutoEq index once (~9 s, needs
internet); measurement CSVs are fetched on demand and cached under `%TEMP%\cageq-cache`.

**Note:** CAGEq writes to Equalizer APO's real config dir, so **Apply** changes playback.
Set `CAGEQ_CONFIG_DIR=<folder>` to write to a scratch folder instead (the status line
shows where it writes). Launching CAGEq twice focuses the existing window (single
instance).

## For maintainers (building the installer)

The frozen sidecar (~177 MB) is a build artifact — gitignored, not committed. Rebuild it
whenever `sidecar_dsp.py` or its deps change, then build the app:

```powershell
# 1. One-time: the Python 3.10 venv (autoeq needs cp310 wheels; 3.11+ fail)
cd cageq-sidecar
py -3.10 -m venv .venv
.\.venv\Scripts\python -m pip install -r python\requirements.txt

# 2. Freeze the sidecar into cageq-app\src-tauri\sidecar\ (bundled as a Tauri resource)
cd ..
.\scripts\build-sidecar.ps1


# 2b. Build CAGEq's own APO + its setup helper into cageq-app\src-tauri\apo\
cd cageq-app
.\scripts\build-apo.ps1
cd ..
# 3. Build the installer
cd cageq-app
npm run tauri build -- --bundles nsis
#   -> src-tauri\target\release\bundle\nsis\CAGEq_<version>_x64-setup.exe  (~39 MB)
```

How it resolves at runtime (`resolve_sidecar`): `CAGEQ_PYTHON`/`CAGEQ_SIDECAR_SCRIPT`
env override → the **bundled frozen exe** next to the app → the dev `.venv` +
`sidecar_dsp.py` → the dependency-free stub. So a dev checkout uses the venv, a released
install uses the bundle, and either can be overridden with the env vars.

## CAGEq's own APO (filter.md §5.3c)

`build-apo.ps1` stages two files into `src-tauri\apo\`, bundled as a Tauri resource and
therefore installed together:

| file | why it ships |
|---|---|
| `CAGEqApo.dll` | the APO itself — the thing audiodg loads |
| `cageq-apo-setup.exe` | the elevated helper the in-app setup panel drives |

They must stay in the **same** directory: the helper looks for the DLL beside itself, which is
what lets `register` install it without being told a path.

**The DLL is not loaded from where it is installed.** `register` copies it to
`%ProgramFiles%\CAGEq\CAGEqApo.dll` and registers *that*. Two independent reasons, either
sufficient on its own:

* `audiodg` runs as **LocalService**, and Tauri's NSIS default is a **per-user** install into
  `%LOCALAPPDATA%` — which a service account cannot read. The DLL would never load, and
  audiodg says nothing when it skips an APO, so the failure would be silent.
* A DLL loaded into a service process **must not be writable by unprivileged users**, or
  replacing it is a privilege escalation. `%ProgramFiles%` is administrator-write,
  everyone-read, which is the shape required.

It also means updating or moving CAGEq cannot leave a registration pointing at a file that has
gone. `register` re-copies each time, so an app update refreshes the installed DLL while the
registry entry keeps pointing at one fixed path.

Nothing about this is automatic on install: setup is driven from the app's own panel, because
**attaching is a per-endpoint choice** and at install time nobody knows which device the user
wants (and the answer changes when they buy a DAC). An installer *may* call
`cageq-apo-setup.exe register` and `open-gate` for the machine-wide half; the per-device
`attach` belongs to the app.
