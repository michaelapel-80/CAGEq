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

CAGEq ships its own oscilloscope, stereo vectorscope, and spectrum analyzer, all fed by a live
WASAPI loopback capture of the actual (post-EQ) output — not mockups, not a generic VU meter.

The spectrum analyzer in particular holds up against dedicated analyzer tools:

* **Log-frequency binned** — equal screen distance means equal perceived pitch, so an octave
  looks like an octave whether it's 55–110 Hz or 5.5–11 kHz.
* **Real peak detection, not "loudest bin wins."** Peaks are picked by prominence against their
  local valleys (so a shoulder bump riding on a bigger resonance doesn't count), gated against
  the frame's own noise floor, and folded by harmonic series — a fundamental's own overtone
  ladder doesn't clutter the readout competing for its own slot. Reported frequency is refined to
  sub-bin precision by parabolic interpolation, not just "whichever of ~240 fixed bins is tallest."
* **Peaks keep their identity from frame to frame** instead of being recomputed from nothing on
  every tick, so the numeric readout stays legible instead of flickering every time a peak briefly
  dips below a detection threshold.

All three views share a CRT-phosphor-style persistence/bloom renderer — a trailing glow that
decays at a real, tunable rate, closer to a real analog scope's look than a plain clear-and-redraw.

## Safety first

CAGEq follows "no sound beats wrong sound": any inconsistency drops the system into a defined,
silent safe state immediately — including a watchdog independent of the main calculation engine,
which can still intervene even if that engine has crashed. Digital clipping (above 0 dBFS) is
guarded against by several independent checks, not a single calculation anyone could get wrong.

## How it works

```
Frontend (React/TypeScript) ──Tauri commands──▶ Rust core ──JSON-RPC over stdio──▶ Python sidecar
                                                     │                                    │
                                              process/watchdog                    AutoEq framework,
                                                management                        NumPy/SciPy, DSP
                                                     │                                    │
                                                     └──────────── biquad coefficients ───┘
                                                                      │
                                          ┌───────────────────────────┴───────────────────────────┐
                                          ▼                                                         ▼
                              Equalizer APO (external, optional)                    CAGEq's own Windows Audio
                              — click-free config-reload crossfade                  Processing Object — live
                                                                                     coefficient ramping over a
                                                                                     shared-memory control channel
```

* **Why Python in the sidecar:** reuse the established AutoEq framework (fitting algorithms plus
  its curated target-curve database) instead of reimplementing it in Rust/JS. Not latency-critical
  — the actual real-time audio path lives entirely in the two engines above.
* **Why Rust/Tauri:** a lean native WebView2 shell instead of a bundled Chromium (Electron), plus
  a fail-safe watchdog independent of Python. Honestly: no component here strictly needs Rust's
  performance — it's also a deliberate learning project, in contrast to the mostly AI-assisted
  frontend.
* **Why a second, custom audio engine alongside Equalizer APO:** Equalizer APO works well but
  its config-reload crossfade has a measurable cold-start bloom on every edit. CAGEq's own APO
  keeps filter state across edits and ramps coefficients live over a control channel instead,
  trading a one-time elevated setup step for a cleaner edit-to-edit transition. Equalizer APO
  remains fully supported for anyone who already uses it or wants its other features.

Windows-only today (via Equalizer APO / a custom Windows Audio Processing Object), though the
data model, DSP math, and most of the UI are platform-agnostic — a port would mean swapping the
Windows-specific audio engine, not restructuring the rest.

## Status

Built and working: both audio engines, the fitting pipeline, the custom-filter editor, A/B/Dry
comparison with loudness matching, the fail-safe watchdog, and the oscilloscope/vectorscope/
spectrum-analyzer instrument views. Actively developed — expect rough edges.

## Getting started

See [DEPLOY.md](DEPLOY.md) for installation (end users) and building from source (maintainers).

## License

[GPL-3.0-or-later](LICENSE).

## Acknowledgments

* [AutoEq](https://github.com/jaakkopasanen/AutoEq) — the fitting framework and measurement/target
  curve database this app builds corrections from.
* [Equalizer APO](https://sourceforge.net/projects/equalizerapo/) — one of the two audio engines
  CAGEq can drive.
* [AQUA](https://github.com/h39s/AQUA) — the project that first suggested this space was worth
  building a real UI for.
