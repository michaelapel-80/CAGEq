# CAGEq — Caged Auto-Gain EQ

A Windows desktop app for creating, managing, and **loudness-neutrally comparing** parametric
headphone EQ corrections, built on real headphone measurement data.

CAGEq computes the correction (via [AutoEq](https://github.com/jaakkopasanen/AutoEq)'s fitting
framework, plus its own custom-filter editor) and drives the actual audio processing either
through [Equalizer APO](https://sourceforge.net/projects/equalizerapo/) or through its own
purpose-built Windows Audio Processing Object — the A/B/Dry comparison, the safety mechanisms,
and the click-free live editing around it are what make either one actually usable.

Loosely inspired by [AQUA](https://github.com/h39s/AQUA), but an independent implementation with
a deliberately different stack — not a fork, no shared code.

![The main correction view: an EQ curve against a live post-EQ spectrum backdrop, level/LUFS
meters, filter-band editor, and A/B/Dry comparison controls.](docs/screenshot.png)

## The problem

Getting a good headphone correction curve is a solved problem (thanks, AutoEq). Comparing curves
*meaningfully by ear* isn't: any EQ change also shifts perceived loudness, so a naive A/B swap
answers "does A just sound louder?" instead of "does A sound better?". And hand-tuning your own
filters without guardrails risks digital clipping or jarring level jumps.

## What CAGEq does about it

* **Loudness-neutral comparison (auto-LUFS).** Every correction gets an automatic compensation
  gain, computed from human hearing-weighted loudness (ITU-R BS.1770-4) — A/B/Dry then differ
  only in tone, not level.
* **Click-free real-time switching** between two independent filter slots (A/B) and the
  unmodified original (Dry) — including one-key switching (`A`/`S`/`D`) so you don't have to look
  at the screen while listening.
* **Cascaded clipping protection.** Several independent safety stages guard against digital
  overs, including when multiple filters stack in ways that look harmless individually.
* **Custom filters for everyone, not just experts.** Set parametric bands by hand (graphical EQ,
  chart-drag, or precise numeric entry) if you want to; use curated or your own saved presets if
  you don't.
* **Auto-fit and hand-tuned filters stay separate but merge cleanly at runtime** — adjusting your
  own bands never loses or fights the underlying AutoEq correction.

## Built-in instrumentation

CAGEq ships its own oscilloscope, stereo vectorscope, spectrum analyzer, and level/LUFS meters,
all fed by a live WASAPI loopback capture of the actual (post-EQ) output — not mockups.

By default, the oscilloscope, vectorscope, and spectrum analyzer don't show that raw capture —
they inverse-filter it back to the pre-EQ source image ("undistort"), so e.g. steady pink noise
still reads flat no matter how aggressive the correction is; each has its own toggle to switch to
the raw post-EQ signal instead. The meters are the one exception and always show the real post-EQ
output, since that's what actually needs measuring for safety.

The level meter (phosphor-beam rendered, like the scopes) carries true-peak (BS.1770 oversampled —
catches inter-sample overs a plain sample-peak read misses) and true-RMS marks, and a BS.1770
momentary/short-term LUFS meter sits beside it, making the auto-loudness compensation this app is
built around actually visible rather than just trusted to work.

The scopes share a CRT-phosphor-style persistence/bloom renderer — a trailing glow that decays at
a real, tunable rate, closer to a real analog scope's look than a plain clear-and-redraw.

A few things about the spectrum analyzer specifically:

* **A fast update rate from heavy window overlap** — each analysis window advances by only a
  quarter of its own length (75% overlap), so a new result lands roughly every 43 ms instead of
  waiting out a full window per update.
* **Interpolated on top of that**, both in frequency (zero-padding resolves the same window's
  transform more finely, not adding fake information) and between successive updates on the
  frontend, so the display reads as continuous motion rather than a stepped, sample-and-hold look.
* **Still readable on fast-moving signals**, since the CRT-phosphor persistence above integrates
  rapid change into a legible trail instead of flickering into noise.

## Safety first

CAGEq follows "no sound beats wrong sound": any inconsistency drops the system into a defined,
silent safe state immediately — including a watchdog independent of the main calculation engine,
which can still intervene even if that engine has crashed. Digital clipping (above 0 dBFS) is
guarded against by several independent checks, not a single calculation anyone could get wrong.

## How it works

```
Frontend (React/TypeScript) ──Tauri commands──▶ Rust core ──JSON-RPC over stdio──▶ Python sidecar
                                                    │                                     │
                                            process/watchdog                      AutoEq framework,
                                               management                         NumPy/SciPy, DSP
                                                    │                                     │
                                                    └──────── biquad coefficients ────────┘
                                                                       │
                                           ┌───────────────────────────┴───────────────────────────┐
                                           ▼                                                       ▼
                          Equalizer APO (external, optional)                           CAGEq's own Windows Audio
                         — click-free config-reload crossfade                          Processing Object — live
                                                                                      coefficient ramping over a
                                                                                     shared-memory control channel
```

* **Why Python in the sidecar:** reuse the established AutoEq framework (fitting algorithms plus
  its curated target-curve database) instead of reimplementing it in Rust/JS. Not latency-critical
  — the actual real-time audio path lives entirely in the two engines above.
* **Why Rust/Tauri:** a lean native WebView2 shell instead of a bundled Chromium (Electron), plus
  a fail-safe watchdog independent of Python. The orchestrator itself doesn't need Rust's
  performance to do its job — but two other components in this same Rust codebase have their own
  reasons: the spectrum analyzer's FFT runs on [`rustfft`](https://github.com/ejmahler/RustFFT),
  which benchmarks itself against FFTW and claims to match or beat it; and CAGEq's own APO runs
  inside `audiodg.exe`'s real-time audio callback, where missing a deadline means an audible
  glitch rather than a slow UI — though it leans on a fair amount of `unsafe` to interop with its
  C++ COM shim, so it isn't a clean memory-safety win either.
* **Why a second, custom audio engine alongside Equalizer APO:** the whole point of this app is a
  meaningful A/B. Equalizer APO's config-reload crossfade puts a bloom on every switch, not just
  every edit — not a hard click, but measurable, and audible with real program material. Read
  naively, it can pass for an actual difference between A and B when there is none, undermining
  the exact comparison CAGEq exists to make trustworthy. CAGEq's own APO keeps filter state across
  switches and ramps coefficients live over a control channel instead, making A/B/Dry switching
  close to fully transparent. A cleaner single-edit transition comes along for free, but
  transparent switching is the actual reason for the one-time elevated setup step it costs.
  Equalizer APO remains fully supported for anyone who already uses it or wants its other
  features.

Windows-only today (via Equalizer APO / a custom Windows Audio Processing Object), though the
data model, DSP math, and most of the UI are platform-agnostic — a port would mean swapping the
Windows-specific audio engine, not restructuring the rest.

Windows-on-Arm isn't natively built or tested, but should already work today via the Equalizer
APO backend and its own ARM64 build: CAGEq there only ever writes `config.txt`, never loads
anything into `audiodg.exe` itself, and the rest of the app (Tauri shell, Python sidecar) isn't
real-time-critical code, so running under Windows' x64 emulation should be a non-issue. CAGEq's
own custom APO is the one piece that doesn't work there — it loads in-process into `audiodg.exe`,
which is genuinely architecture-matched on Arm, so it would need an actual native ARM64 build
(and, unverified either way: neither path has been run on real Arm hardware).

Built with heavy AI assistance (Claude) across the whole stack, not just the frontend — worth
saying plainly rather than leaving it to be inferred. In practice that means the DSP math is
checked against an analytic ground truth rather than just listened to, corners of the Windows
audio APIs got verified against source/documentation rather than assumed, and behavior that
matters (loudness matching, clipping protection, the fail-safe path) was tested directly, not
taken on faith.

## Status

Built and working: both audio engines, the fitting pipeline, the custom-filter editor, A/B/Dry
comparison with loudness matching, the fail-safe watchdog, and the oscilloscope/vectorscope/
spectrum-analyzer/meter instrument views. Actively developed — expect rough edges.

## Getting started

Grab the installer from [Releases](../../releases/latest), or see [DEPLOY.md](DEPLOY.md) for the
full installation walkthrough and building from source (maintainers).

## License

[GPL-3.0-or-later](LICENSE). Third-party dependencies bundled into the built application (Rust
crates, the frontend's npm packages, the frozen Python sidecar) are all permissively licensed
(MIT/BSD/Apache-2.0 and similar) — see [THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt) for
the full list and their license texts.

## Acknowledgments

* [AutoEq](https://github.com/jaakkopasanen/AutoEq) — the fitting framework and measurement/target
  curve database this app builds corrections from.
* [Equalizer APO](https://sourceforge.net/projects/equalizerapo/) — one of the two audio engines
  CAGEq can drive.
* [AQUA](https://github.com/h39s/AQUA) — the project that first suggested this space was worth
  building a real UI for.
