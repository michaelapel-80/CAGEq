# Deploying CAGEq on another machine

**Platform:** Windows x64.

> **Not turnkey yet.** The Python DSP sidecar is **not bundled** into the installer.
> The binary has this repo's dev path compiled in (`CARGO_MANIFEST_DIR`), so on another
> machine CAGEq can't find the sidecar unless you set two env vars pointing at it. Until
> the "bundle the sidecar" work is done (see the end), deployment needs the manual steps
> below. See [filter.md](filter.md) for the full design.

## What has to be on the target machine

1. **Equalizer APO** — install it, then run its Configurator (`DeviceSelector.exe`),
   tick the output device you want to correct, and **reboot**. CAGEq detects whether
   EqAPO is actually enabled on a device and warns if it isn't.
2. **Python 3.10 specifically** — *not* 3.11/3.12. AutoEq 4.1.2 pins 2022-era
   numpy/matplotlib whose wheels only exist through cp310; newer Pythons try to compile
   them and fail.
3. **The DSP sidecar** — `cageq-sidecar/python/sidecar_dsp.py` from this repo, plus a
   Python 3.10 venv with `autoeq` installed.
4. **CAGEq** — the `CAGEq_<version>_x64-setup.exe` installer. It bootstraps the
   Microsoft Edge WebView2 runtime if missing (Windows 11 already has it).

## Steps

Assume you copy `sidecar_dsp.py` to `C:\CAGEq`.

```powershell
# 1. Python 3.10 (or install from python.org)
winget install Python.Python.3.10

# 2. Sidecar venv + AutoEq
cd C:\CAGEq                                  # this folder contains sidecar_dsp.py
py -3.10 -m venv .venv
.\.venv\Scripts\python -m pip install autoeq==4.1.2

# 3. Point CAGEq at the sidecar (the compiled-in dev path won't exist here)
setx CAGEQ_PYTHON "C:\CAGEq\.venv\Scripts\python.exe"
setx CAGEQ_SIDECAR_SCRIPT "C:\CAGEq\sidecar_dsp.py"

# 4. Install CAGEq
#    run CAGEq_<version>_x64-setup.exe
```

Launch CAGEq **after** the `setx` calls (env vars only apply to processes started
afterwards).

## Verify it worked

In the app's status line at the top:

- `sidecar: AutoEq DSP (…)` → the real engine is wired up. ✅
- `sidecar: stub (…)` or an init-failed banner → the env vars aren't being seen; check
  the paths and that CAGEq was launched in a fresh session after `setx`.

Then pick an EqAPO-enabled output device, a headphone + measurement, a target, and
**Apply**. The written config appears in the window and audio changes.

## Notes

- **Env vars are mandatory here** because the binary's baked-in sidecar path
  (`resolve_sidecar()` → `CARGO_MANIFEST_DIR`) doesn't exist on another machine; the
  overrides redirect it to your copied sidecar + venv. `CAGEQ_PYTHON` also pins the
  correct 3.10 interpreter (the `py`-launcher fallback might otherwise pick 3.12).
- **No measurement data to copy** — the AutoEq catalogue and each headphone's CSV are
  fetched on demand from GitHub and cached under `%TEMP%\cageq-cache`. Needs internet on
  first use.
- **First launch is slower** — it builds the ~6800-headphone index once (~9 s), cached
  afterwards.
- **It affects real audio** — CAGEq resolves EqAPO's real config dir, so **Apply**
  writes to `…\EqualizerAPO\config` and changes playback. Override with
  `CAGEQ_CONFIG_DIR=<folder>` to write to a scratch folder instead (the status line
  shows where it writes).
- **Single instance** — launching CAGEq twice focuses the existing window instead of
  starting a second writer.

## Making it turnkey (deferred)

The goal is to reduce this to **"install Equalizer APO → run the CAGEq installer"** with
no Python steps. That means:

1. Ship an embeddable/frozen Python 3.10 + the venv (e.g. PyInstaller, or the
   embeddable Python distribution) alongside the app.
2. Bundle it as a Tauri resource and have `resolve_sidecar()` prefer a path **next to
   the executable** over the compiled-in dev path.

Bigger job, but it removes every manual step above except installing Equalizer APO.
