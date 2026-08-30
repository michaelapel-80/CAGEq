# cageq-apo — CAGEq's own Audio Processing Object

Replaces the Equalizer APO runtime dependency (filter.md §5.3c). A COM DLL loaded into
`audiodg.exe`, implementing only the subset CAGEq actually uses: four RBJ biquad types
plus a preamp, driven by a live control channel instead of EqAPO's file-reload model.

## Why it is half C++

An APO has to survive audiodg's load handshake, and what reliably does is
`CBaseAudioProcessingObject` from the Windows SDK — format negotiation, connection
validation, buffer bookkeeping. It is a C++ class, and **Rust cannot inherit one**. An
earlier spike hand-wrote the `IAudioProcessingObject` vtables instead: it CoCreated fine,
but audiodg silently discarded it mid-handshake.

So `shim/cageq_apo.cpp` inherits that base class and does COM plumbing *and nothing else*,
forwarding across a C ABI to `src/lib.rs`, where all the DSP, filter state and control-channel
logic lives. If audio logic starts appearing in the shim, the split has stopped paying for itself.

> **The base class is in the plain Windows SDK**, under
> `Include/<ver>/um/baseaudioprocessingobject.h` and
> `Lib/<ver>/um/x64/AudioBaseProcessingObjectV140.lib`. **No WDK is required** — the earlier
> spike assumed it was a driver-kit component and abandoned the base class over it.

## Build

```
build.bat
```

Needs VS2022 with the C++ toolset, the Windows SDK, and cargo. Building is harmless
anywhere: it neither registers the COM server nor touches any audio endpoint.

Produces a **self-contained** `build/CAGEqApo.dll` (x64) — both halves link the CRT
statically, so it imports only OS libraries. That is deliberate: inside audiodg
(LocalService, session 0) a missing `VCRUNTIME140.dll` is a silent load failure that looks
exactly like "our APO is broken".

`LNK4217` about `__stdio_common_vswprintf` during the link is expected and benign — the
SDK's base-class lib was built against the dynamic CRT and imports a symbol we now provide
statically. The linker resolves it correctly.

## ⚠ Registering — snapshotted VM only

`scripts/register.ps1` edits an audio endpoint's effect chain and restarts the audio
service. The APO then runs **inside the process that owns audio for the whole machine**. A
broken one can silence an endpoint.

Take a VM snapshot first. `unregister.ps1` restores the endpoint from a backup this makes,
but the snapshot is the thing that always works.

## Stage B procedure

Stage B answers one question: **does our APO load into audiodg and pass audio?** The DSP is
deliberately an identity passthrough — adding filters before that is answered would only
make a failure harder to localise.

On the VM, elevated:

1. `scripts\register.ps1` — lists endpoints. Pick a **scratch** one with `fx=True`.
2. `scripts\register.ps1 -EndpointId <GUID>`
3. Play audio on that endpoint. audiodg only loads APOs once a stream is running.
4. Process Explorer → `audiodg.exe` → lower pane (Ctrl+D, DLL view) → look for `CAGEqApo.dll`.

| Result | Meaning |
|---|---|
| Loaded **and** audio still plays | **Stage B passes** — it loads and passes audio through |
| Not loaded | Run `scripts\sign.ps1`, re-register. Then try `-Slot LFX`. Then check Event Viewer → System |
| Loaded but silence | It loads but the RT path or format negotiation is wrong — the more informative failure |

5. Soak ~10 minutes: start/stop playback, change the endpoint's sample format, switch the
   default device. Watch for glitches, audiodg CPU spikes, crashes.
6. `scripts\unregister.ps1 -EndpointId <GUID>`, then revert the snapshot.

Record two things, because stage D has to ship them: **whether signing was needed**, and
**whether `DisableProtectedAudioDG=1` was needed**.

## Files

| Path | |
|---|---|
| `src/lib.rs` | Rust core — DSP, RT contract, C ABI. Everything with judgement in it |
| `shim/cageq_apo.cpp` | C++ COM shim — base class, class factory, registration |
| `shim/cageq_apo.def` | DLL exports (the four COM entry points) |
| `build.bat` | Builds both halves into `build/CAGEqApo.dll` |
| `scripts/register.ps1` | Register + attach to one endpoint (**VM only**) |
| `scripts/unregister.ps1` | Detach + unregister, restoring from backup |
| `scripts/sign.ps1` | Self-signed Authenticode, in-box tooling only (**VM only**) |

CLSID `{530052E1-2CD4-400A-AC2B-0D19273AD5B7}` is declared in `shim/cageq_apo.cpp` and
`scripts/register.ps1` — keep them in sync.
