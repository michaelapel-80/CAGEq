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

Getting a good *starting point* for a headphone correction curve is a largely solved problem
(thanks, AutoEq) — but a measurement-to-target fit is a generally good match, not a perfect one
for *your* ears and *your* pair: individual anatomy shapes your own frequency response, headphones
bypass most of the head-related transfer function your brain is calibrated for, the measurement
rig can't reproduce your anatomy unless it was measured with in-ear mics in your own ears, and
there's sample variation between individual headphone units. So the curve still needs hand-tuning
to close that gap. Comparing curves *meaningfully by ear* while you do that isn't solved either:
any EQ change also shifts perceived loudness, so a naive A/B swap answers "does A just sound
louder?" instead of "does A sound better?". And hand-tuning your own filters without guardrails
risks digital clipping or jarring level jumps.

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

The scopes share a CRT-phosphor-style persistence/bloom renderer — a trailing glow that decays at
a real, tunable rate, closer to a real analog scope's look than a plain clear-and-redraw.

![The oscilloscope (with trigger/mix controls) and stereo vectorscope side by side, both rendered
with the CRT-phosphor persistence trail.](docs/Scope.png)

A few things about the spectrum analyzer specifically:

* **A fast, constant update rate whatever the window size** — heavy window overlap means a new
  result lands every ~43 ms (a quarter of the shortest window) instead of waiting out a full window
  per update, and that rate doesn't change with the window length: a longer window buys finer
  frequency resolution without slowing the display, it just overlaps more (75% at the shortest,
  ~94% at the longest). The window length itself is tuned smoothly (~171-683 ms at 48 kHz, in ~5 ms
  steps) rather than in the usual power-of-two jumps — every window is zero-padded into the same
  fixed-size FFT, so an arbitrary length costs nothing extra.
* **Interpolated on top of that**, both in frequency (zero-padding resolves the same window's
  transform more finely, not adding fake information) and between successive updates on the
  frontend, so the display reads as continuous motion rather than a stepped, sample-and-hold look.
* **Still readable on fast-moving signals**, since the CRT-phosphor persistence above integrates
  rapid change into a legible trail instead of flickering into noise.
* **A "Distribution" render-tuning preset for a denser read than a single trace** — a much longer
  setting of the same phosphor decay above, so a bin that keeps recurring builds up brighter than
  one that only flickered through once. Behaves like an analog CRT spectrum analyzer's persistence,
  not a modern digital one's boxcar/linear-decay density display — a good qualitative read, not an
  exact density measurement.

![The spectrum analyzer, with detected peaks marked and read out below the
chart.](docs/SpectrumScope.png)

The level meter (phosphor-beam rendered, like the scopes) carries true-peak (BS.1770 oversampled —
catches inter-sample overs a plain sample-peak read misses) and true-RMS marks, and a full BS.1770
loudness readout sits beside it — momentary, short-term, integrated, and loudness range (EBU Tech
3342 LRA), plus a peak-max high-water mark, all restartable on demand — making the auto-loudness
compensation this app is built around actually visible rather than just trusted to work.

![The level/LUFS meter: peak and RMS bars, and the full BS.1770 readout (momentary, short-term,
integrated, loudness range, peak max).](docs/Meter.png)

## Safety first

CAGEq follows "no sound beats wrong sound": any inconsistency drops the system into a defined,
silent safe state immediately. Digital clipping (above 0 dBFS) is guarded against by several
independent checks, not a single calculation anyone could get wrong.

## How it works

```
Frontend (React/TypeScript) ──Tauri commands──▶ Rust core ──biquad coefficients──┐
                                                    │                            │
                                            AutoEq's fitting                     │
                                          algorithm, ported to                   │
                                             Rust (SLSQP)                        │
                                                    │                            │
                              ┌─────────────────────┴──────┐                     │
                              ▼                            ▼                     │
             Equalizer APO (external, optional)        CAGEq's own APO ◀─────────┘
            — click-free config-reload crossfade     — live coefficient ramping over a
                                                       shared-memory control channel
```

* **Why Rust, not Python:** CAGEq originally called out to a Python sidecar (NumPy/SciPy/AutoEq)
  over JSON-RPC for the fit itself, to reuse AutoEq's established fitting algorithm rather than
  reimplementing it. That algorithm — the FR-prep, the SLSQP parametric-EQ solver, the
  measurement/target catalogue fetch — now runs natively in Rust (`cageq-peq-solver`,
  `cageq-catalog`), validated against the original Python/AutoEq implementation on the full
  measurement corpus rather than assumed equivalent. The app no longer starts a Python process at
  all; the reference implementation still lives in the repo purely as the comparison target those
  validation tests run against.
* **Why Rust/Tauri:** a lean native WebView2 shell instead of a bundled Chromium (Electron). The
  orchestrator itself doesn't need Rust's performance to do its job — but two other components in
  this same Rust codebase have their own reasons: the spectrum analyzer's FFT runs on
  [`rustfft`](https://github.com/ejmahler/RustFFT), which benchmarks itself against FFTW and
  claims to match or beat it. A GPU-accelerated path was evaluated and measured *worse*, not
  neutral — at this workload's batch size of one FFT per ~43 ms hop, the fixed per-dispatch
  upload/sync/readback cost outweighs the entire CPU transform, costing 2-3x more CPU than just
  doing it on the CPU outright, and even async pipelining only reaches break-even at 96 kHz. On an
  AMD Ryzen 9 7900X, a release build's single-core load for the FFT alone is ~0.3% at 96 kHz (the
  spectrum analyzer's own capped ceiling — the actual correction filters always run at the
  device's real sample rate, capped or not). CAGEq's own APO runs inside `audiodg.exe`'s real-time
  audio callback, where missing a deadline means an audible glitch rather than a slow UI — though
  it leans on a fair amount of `unsafe` to interop with its C++ COM shim, so it isn't a clean
  memory-safety win either.
* **Why the plain RBJ cookbook filter formulas, not a warping-corrected design** (Massberg/
  Vicanek/Muranov): AutoEq's own reference implementation and Equalizer APO both compute — and
  expect — coefficients from exactly these formulas, so matching them bit-for-bit keeps a
  CAGEq-fitted curve numerically identical to what either tool would produce from the same
  parameters; a "more correct" warping-corrected design would quietly diverge from the very target
  it's meant to match. These designs matter most as the target frequency approaches Nyquist — the
  regime these papers' own full-spectrum magnitude-response comparisons demonstrate it in — but
  AutoEq's own error signal is heavily smoothed above ~6-8kHz (a 2-octave smoothing window there,
  versus 1/12-octave everywhere else it fits against), by explicit design: its own changelog states
  it "treats +10kHz range as average value instead of trying to fix it precisely." A fit
  that never asks for a precise, narrow correction anywhere near Nyquist in the first place has
  nothing left for a warping-corrected design to actually improve.
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
anything into `audiodg.exe` itself, and the rest of the app (the Tauri shell) isn't
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

Built and working: both audio engines, the fitting pipeline (now pure Rust, no Python at
runtime), the custom-filter editor, A/B/Dry comparison with loudness matching, and the
oscilloscope/vectorscope/spectrum-analyzer/meter instrument views. Actively developed — expect
rough edges.

## Getting started

Grab the installer from [Releases](../../releases/latest), or see [DEPLOY.md](DEPLOY.md) for the
full installation walkthrough and building from source (maintainers).

## License

[GPL-3.0-or-later](LICENSE). Third-party dependencies bundled into the built application (Rust
crates, the frontend's npm packages) are all permissively licensed (MIT/BSD/Apache-2.0 and
similar) — see [THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt) for the full list and their
license texts.

## Acknowledgments

* [AutoEq](https://github.com/jaakkopasanen/AutoEq) — the fitting framework and measurement/target
  curve database this app builds corrections from.
* [Equalizer APO](https://sourceforge.net/projects/equalizerapo/) — one of the two audio engines
  CAGEq can drive.
* [AQUA](https://github.com/h39s/AQUA) — the project that first suggested this space was worth
  building a real UI for.
* [Peace](https://sourceforge.net/projects/peace-equalizer-apo-extension/) — another Equalizer APO GUI,
  with a much deeper feature set than AQUA's, if a dated UI.
