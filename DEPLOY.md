# Deploying CAGEq

**Platform:** Windows x64.

The installer is **self-contained**: it bundles the Python DSP sidecar frozen with
PyInstaller (numpy/scipy/matplotlib/autoeq), so the target machine needs **no Python**.

## For end users (one-click)

1. **Run `CAGEq_<version>_x64-setup.exe`.** It installs CAGEq and bootstraps the
   Microsoft Edge WebView2 runtime if missing (Windows 11 already has it). The installer isn't
   code-signed (no certificate), so Windows SmartScreen will show an "unknown publisher"
   warning the first time — "More info" → "Run anyway" to proceed.
2. Launch CAGEq and pick your output device. CAGEq needs an audio engine attached to that
   device before **Apply** does anything audible (filter.md §5.3c) — **installing Equalizer
   APO is no longer required**, there are two options:
   * **CAGEq's own engine** — no separate install. The header's **Engine** button reads red
     ("needs setup") until something is attached; click it and step through the setup panel —
     three elevated steps the first time (register, open the gate, then attach the device;
     each is its own UAC prompt), just one more Attach prompt per additional device after that.
   * **Equalizer APO** — if you already have it, or want its other features too. Install it,
     run its Configurator (`DeviceSelector.exe`), tick the output device, and reboot; CAGEq
     detects and drives it automatically once it's there.
3. Pick a headphone + measurement, a target, and **Apply** — audio changes.

That's it. First launch fetches the ~6800-headphone AutoEq index once (~9 s, needs
internet); measurement CSVs are fetched on demand and cached under `%TEMP%\cageq-cache`.
With neither engine attached yet, CAGEq still launches normally (it points Equalizer APO's
backend at a harmless scratch folder rather than erroring) and simply waits for setup.

**Note:** with Equalizer APO as the active engine, **Apply** writes to its real config dir,
changing playback there directly — set `CAGEQ_CONFIG_DIR=<folder>` to write to a scratch
folder instead (the status line shows where it writes). This override is specific to the
Equalizer APO path; CAGEq's own engine always keeps its per-endpoint configuration under the
fixed `%ProgramData%\CAGEq\apo` (not redirectable — `audiodg` needs to read it as
LocalService, the same constraint the DLL's own install location is under, see "CAGEq's own
APO" below). Launching CAGEq twice focuses the existing window (single instance).

## For maintainers (building the installer)

**Automated:** pushing a `v*` tag runs [`.github/workflows/release.yml`](.github/workflows/release.yml),
which does exactly the steps below on a `windows-2022` GitHub-hosted runner and publishes the
resulting installer to that tag's Release. `workflow_dispatch` runs the same pipeline manually
(installer available as a workflow artifact) without publishing anything, for testing the
pipeline itself. What follows is that same process by hand — useful for local testing, or if the
automated one ever needs debugging.

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

Step 3 will **refuse to run** (`beforeBuildCommand` → `scripts\check-apo-staleness.ps1 -Block`) if
anything under `cageq-apo`, `cageq-apo-backend`, or `cageq-backend` is newer than what's staged in
`src-tauri\apo\` — i.e. if step 2b was skipped, or those crates changed after the last time it ran.
Re-run `build-apo.ps1` and try again. (`npm run tauri dev` runs the same check but only warns, so a
frontend-only dev session isn't blocked by an unrelated stale APO build.)

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

**An app update alone does not re-run `register`.** Nothing does it automatically — the app
detects staleness (`dll_current`: `cageq_apo::APO_VERSION`, the number *this build* was compiled
with, against a plain-text marker `register` writes alongside the installed DLL at
`%ProgramFiles%\CAGEq\CAGEqApo.version`) and shows an in-app notice; clicking through it re-runs
the same `RegisterServer` action a first-time setup uses. **Not a hash of the DLL's bytes** —
that was the original mechanism, and it meant every dev rebuild looked like a new release
(compiler output isn't byte-reproducible build to build), nagging to re-run an elevated step for
zero actual behaviour change. The trade-off: bumping `APO_VERSION` by hand in `cageq-apo/src/
lib.rs` is now a **real release step**, not automatic — forgetting it means a genuine APO change
ships without the update prompt ever firing. Bump it for anything a user should actually be
prompted to update for (DSP math, the config format, `control::CONTROL_VERSION`, ramp timing);
don't for anything else. This is exactly why step 3 above refuses to build with a stale staged
DLL: shipping an update whose *bundled* copy is itself stale would make that whole detection
meaningless.

Nothing about this is automatic on install: setup is driven from the app's own panel, because
**attaching is a per-endpoint choice** and at install time nobody knows which device the user
wants (and the answer changes when they buy a DAC). An installer *may* call
`cageq-apo-setup.exe register` and `open-gate` for the machine-wide half; the per-device
`attach` belongs to the app.
