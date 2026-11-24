//! The audio data streams — scope, meter, spectrum — through shared memory instead of `Channel`s:
//! each webview that shows them gets a WebView2 shared buffer per stream
//! (`ICoreWebView2Environment12::CreateSharedBuffer`, handed to the page with
//! `ICoreWebView2_17::PostSharedBufferToScript`), the capture thread writes every update straight
//! into a ring in it, and the page reads what's new each frame (frontend `streams.ts`).
//!
//! Why: a `Channel` message isn't a raw pipe — Tauri delivers each one by evaluating a freshly
//! generated script in the webview, and one over 8 KiB (a scope window is ~10 KB of JSON, a
//! spectrum update ~30 KB) also makes the page fetch it back over IPC. At 100 scope windows a
//! second per webview, plus ~60 meter and ~23 spectrum updates, the renderer's garbage collector
//! fell behind with the scope pop-outs open (live: GC hitches, the renderer past 1.9 GB) — the
//! same churn as the `app.emit` out-of-memory class `StreamSubs` documents. Here there is no
//! message per update at all: no script, no JSON, no fetch. And no delivery to hold up: a window
//! drag's modal loop, which held `Channel` data back by up to ~135 ms, can't touch memory the
//! capture thread writes directly.
//!
//! Each ring (little-endian, read-only to the page):
//!
//! | offset | header                                            |
//! |--------|---------------------------------------------------|
//! | 0      | magic `RING_MAGIC` (u32)                          |
//! | 4      | version `RING_VERSION` (u32)                      |
//! | 8      | slot count (u32)                                  |
//! | 12     | slot size in bytes (u32)                          |
//! | 16     | updates written so far (u32) — set after each one |
//!
//! Slots from `HEADER_BYTES`, update `k` in slot `k % slots`; every slot starts with `k + 1` once
//! complete, 0 while being written (u32). After that, per stream:
//!
//! - **scope** — 4: sample pairs `n` (u32); 8: rate, Hz (u32); 12: flags, bit 0 = signal (u32);
//!   16: `t0`, 100 ns performance-counter units (f64, exact below 2^53); 24: `sent`, same (f64);
//!   32: `n` (l, r) pairs (f32).
//! - **meter**, **spectrum** — 4: array count (u32); 8: a time, 100 ns units (f64: the spectrum's
//!   `t_end`, else 0); 16: the arrays, each its length (u32) then its values (f32), in the order
//!   `meter_arrays` / `spectrum_arrays` give them.
//!
//! The page reads an update only once the count says it's complete, and only within the last
//! `slots - 8` — a slot is rewritten a full lap later, so a read would have to stall for most of
//! a lap mid-copy to see a slot change under it, and the slot's own number says so if it did.
//! Writes are ordered (slot number zeroed, data, slot number, count) with release stores; x86
//! keeps stores in order for the reader in the other process.

use std::collections::HashMap;
use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

pub const RING_MAGIC: u32 = 0x4253_4743; // "CGSB"
pub const RING_VERSION: u32 = 1;
pub const HEADER_BYTES: usize = 64;

/// The streams, each with its own ring shape.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Stream {
    Scope,
    Meter,
    Spectrum,
}

impl Stream {
    pub fn parse(name: &str) -> Option<Stream> {
        match name {
            "scope" => Some(Stream::Scope),
            "meter" => Some(Stream::Meter),
            "spectrum" => Some(Stream::Spectrum),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Stream::Scope => "scope",
            Stream::Meter => "meter",
            Stream::Spectrum => "spectrum",
        }
    }

    /// (slots, slot bytes). Scope: 640 ms of windows, each up to `SCOPE_PAIRS`. Meter: ~1 s of
    /// updates, a few hundred bytes each. Spectrum: ~0.7 s, each ~31 KB (8 arrays of 957 values,
    /// cageq-monitor's `N_LOG_POINTS`/`N_LIN_POINTS`, and the peaks), with room to spare.
    fn shape(self) -> (u32, usize) {
        match self {
            Stream::Scope => (64, round64(SCOPE_HEAD_BYTES + SCOPE_PAIRS * 8)),
            Stream::Meter => (64, 2048),
            Stream::Spectrum => (16, 64 * 1024),
        }
    }
}

/// The most sample pairs a scope slot takes: what a window can carry at the capture rate cap
/// (96 kHz × 16 ms × 3, cageq-monitor's `SCOPE_MAX_POINTS`); a longer one keeps its newest samples.
const SCOPE_PAIRS: usize = 4608;
const SCOPE_HEAD_BYTES: usize = 32;
const RECORD_HEAD_BYTES: usize = 16;

const fn round64(n: usize) -> usize {
    n.div_ceil(64) * 64
}

/// One webview's ring for one stream: the mapped memory, and the WebView2 object that keeps it
/// mapped.
struct Ring {
    ptr: *mut u8,
    slots: u32,
    slot_bytes: usize,
    written: u32,
    // Dropped (released) only on the main thread: `drop_webview` and `create` both run there.
    _buffer: Keep,
}

/// The shared buffer COM object, held only to keep the memory mapped. Its interface pointer isn't
/// `Send`, but nothing calls it from another thread: the capture thread only writes the memory.
struct Keep(#[allow(dead_code)] webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2SharedBuffer);
// SAFETY: see `Keep` — never used off the main thread, only kept alive.
unsafe impl Send for Keep {}
// SAFETY: `ptr` is the buffer's own mapping, valid while `_buffer` lives; only written under the
// map's mutex, by one thread at a time.
unsafe impl Send for Ring {}

/// Every webview's rings, by (label, stream).
#[derive(Default, Clone)]
pub struct SharedStreams(Arc<Mutex<HashMap<(String, Stream), Ring>>>);

impl SharedStreams {
    /// Write a scope window into every scope ring. Capture thread.
    pub fn write_scope(&self, u: &cageq_monitor::ScopeUpdate) {
        self.each(Stream::Scope, |ring| unsafe { ring.write_with(|slot, bytes| scope_slot(slot, bytes, u)) });
    }

    /// Write a meter update into every meter ring. Capture thread.
    pub fn write_meter(&self, u: &cageq_monitor::MeterUpdate) {
        let head = [
            u.peak_db,
            u.rms_db,
            u.momentary_lufs,
            u.short_term_lufs,
            u.integrated_lufs,
            u.loudness_range,
            u.true_peak_max_db,
            f32::from(u8::from(u.signal)),
            u.sample_rate as f32,
        ];
        let arrays: [&[f32]; 2] = [&head, &u.bins];
        self.each(Stream::Meter, |ring| unsafe { ring.write_with(|slot, bytes| record_slot(slot, bytes, 0, &arrays)) });
    }

    /// Write a spectrum update into every spectrum ring. Capture thread.
    pub fn write_spectrum(&self, u: &cageq_monitor::SpectrumUpdate) {
        let head = [f32::from(u8::from(u.signal)), u.f_min, u.f_max, u.peak_separation_octaves];
        let pairs = |p: &[cageq_monitor::SpectrumPeak]| p.iter().flat_map(|x| [x.hz, x.db]).collect::<Vec<f32>>();
        let (peaks, candidates) = (pairs(&u.peaks), pairs(&u.peak_candidates));
        let arrays: [&[f32]; 11] = [
            &head,
            &u.db,
            &u.peak_db,
            &u.peak_hz,
            &u.db_raw,
            &u.db_lin,
            &u.peak_db_lin,
            &u.peak_hz_lin,
            &u.db_lin_raw,
            &peaks,
            &candidates,
        ];
        self.each(Stream::Spectrum, |ring| unsafe {
            ring.write_with(|slot, bytes| record_slot(slot, bytes, u.t_end, &arrays))
        });
    }

    fn each(&self, stream: Stream, mut f: impl FnMut(&mut Ring)) {
        let Ok(mut rings) = self.0.lock() else { return };
        for ((_, s), ring) in rings.iter_mut() {
            if *s == stream {
                f(ring);
            }
        }
    }

    /// Drop a webview's rings (it was destroyed). Main thread.
    pub fn drop_webview(&self, label: &str) {
        if let Ok(mut rings) = self.0.lock() {
            rings.retain(|(l, _), _| l != label);
        }
    }

    /// Whether `label` has a ring for `stream` — its `Channel` is then left unsent.
    pub fn has(&self, label: &str, stream: Stream) -> bool {
        self.0.lock().is_ok_and(|r| r.contains_key(&(label.to_string(), stream)))
    }
}

impl Ring {
    /// Write the next update into its slot with `fill` (which gets the slot past its number and
    /// its size, and says whether it fit), then publish it.
    ///
    /// SAFETY: `self.ptr` must point at the whole ring, writable.
    unsafe fn write_with(&mut self, fill: impl FnOnce(*mut u8, usize) -> bool) {
        let k = self.written;
        let slot = unsafe { self.ptr.add(HEADER_BYTES + (k % self.slots) as usize * self.slot_bytes) };
        unsafe {
            let seq = AtomicU32::from_ptr(slot as *mut u32);
            seq.store(0, Ordering::Relaxed);
            fence(Ordering::Release); // the slot reads as busy before any of it changes
            if !fill(slot, self.slot_bytes) {
                return; // didn't fit: left busy, the page skips it (never with the sizes above)
            }
            seq.store(k.wrapping_add(1), Ordering::Release);
            AtomicU32::from_ptr(self.ptr.add(16) as *mut u32).store(k.wrapping_add(1), Ordering::Release);
        }
        self.written = k.wrapping_add(1);
    }
}

/// A scope window into `slot` (see the module doc). SAFETY: `slot` is `bytes` long, writable.
unsafe fn scope_slot(slot: *mut u8, bytes: usize, u: &cageq_monitor::ScopeUpdate) -> bool {
    let pairs = u.xy.len() / 2;
    let n = pairs.min((bytes - SCOPE_HEAD_BYTES) / 8);
    let skipped = pairs - n;
    let t0 = if u.t0 == 0 || u.rate == 0 { 0 } else { u.t0 + (skipped as u64 * 10_000_000) / u.rate as u64 };
    unsafe {
        (slot.add(4) as *mut u32).write(n as u32);
        (slot.add(8) as *mut u32).write(u.rate);
        (slot.add(12) as *mut u32).write(u32::from(u.signal));
        (slot.add(16) as *mut f64).write(t0 as f64);
        (slot.add(24) as *mut f64).write(u.sent as f64);
        std::ptr::copy_nonoverlapping(u.xy[skipped * 2..].as_ptr(), slot.add(SCOPE_HEAD_BYTES) as *mut f32, n * 2);
    }
    true
}

/// A meter or spectrum record into `slot` (see the module doc). SAFETY: `slot` is `bytes` long,
/// writable.
unsafe fn record_slot(slot: *mut u8, bytes: usize, time: u64, arrays: &[&[f32]]) -> bool {
    let need = RECORD_HEAD_BYTES + arrays.iter().map(|a| 4 + a.len() * 4).sum::<usize>();
    if need > bytes {
        return false;
    }
    unsafe {
        (slot.add(4) as *mut u32).write(arrays.len() as u32);
        (slot.add(8) as *mut f64).write(time as f64);
        let mut at = RECORD_HEAD_BYTES;
        for a in arrays {
            (slot.add(at) as *mut u32).write(a.len() as u32);
            std::ptr::copy_nonoverlapping(a.as_ptr(), slot.add(at + 4) as *mut f32, a.len());
            at += 4 + a.len() * 4;
        }
    }
    true
}

/// Create `label`'s ring for `stream` and hand it to the page (replacing any it had: a reload).
/// Must run on the main thread, as WebView2's API does — from `with_webview`'s closure.
pub fn create(shared: &SharedStreams, label: &str, stream: Stream, pw: tauri::webview::PlatformWebview) -> Result<(), String> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2Environment12, ICoreWebView2_17, COREWEBVIEW2_SHARED_BUFFER_ACCESS_READ_ONLY,
    };
    use windows_core::Interface;
    let err = |what: &str, e: windows_core::Error| format!("shared {} buffer: {what}: {e}", stream.name());
    let (slots, slot_bytes) = stream.shape();
    let size = HEADER_BYTES + slots as usize * slot_bytes;
    unsafe {
        let env: ICoreWebView2Environment12 = pw.environment().cast().map_err(|e| err("environment", e))?;
        let core: ICoreWebView2_17 = pw
            .controller()
            .CoreWebView2()
            .and_then(|c| c.cast())
            .map_err(|e| err("webview", e))?;
        let buffer = env.CreateSharedBuffer(size as u64).map_err(|e| err("create", e))?;
        let mut ptr = std::ptr::null_mut();
        buffer.Buffer(&mut ptr).map_err(|e| err("map", e))?;
        if ptr.is_null() {
            return Err(format!("shared {} buffer: no memory", stream.name()));
        }
        std::ptr::write_bytes(ptr, 0, size);
        (ptr as *mut u32).write(RING_MAGIC);
        (ptr.add(4) as *mut u32).write(RING_VERSION);
        (ptr.add(8) as *mut u32).write(slots);
        (ptr.add(12) as *mut u32).write(slot_bytes as u32);
        // In the map before the page sees it, so the first update after can't be missed.
        let key = (label.to_string(), stream);
        if let Ok(mut rings) = shared.0.lock() {
            rings.insert(key.clone(), Ring { ptr, slots, slot_bytes, written: 0, _buffer: Keep(buffer.clone()) });
        }
        let data = windows_core::HSTRING::from(format!(r#"{{"kind":"{}"}}"#, stream.name()));
        if let Err(e) = core.PostSharedBufferToScript(&buffer, COREWEBVIEW2_SHARED_BUFFER_ACCESS_READ_ONLY, &data) {
            if let Ok(mut rings) = shared.0.lock() {
                rings.remove(&key);
            }
            return Err(err("post", e));
        }
    }
    Ok(())
}
