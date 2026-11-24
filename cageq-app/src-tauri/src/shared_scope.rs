//! The scope stream through shared memory instead of a `Channel`: each webview that shows the
//! scopes gets a WebView2 shared buffer (`ICoreWebView2Environment12::CreateSharedBuffer`, handed
//! to the page with `ICoreWebView2_17::PostSharedBufferToScript`), the capture thread writes every
//! scope window straight into a ring in it, and the page reads what's new each frame.
//!
//! Why: a `Channel` message isn't a raw pipe — Tauri delivers each one by evaluating a freshly
//! generated script in the webview, and one over 8 KiB (a scope window is ~10 KB of JSON) also
//! makes the page fetch it back over IPC. At 100 windows a second per webview, plus the meter and
//! spectrum, the renderer's garbage collector fell behind with the scope pop-out open (live: GC
//! hitches, the renderer past 1.9 GB) — the same churn as the `app.emit` out-of-memory class
//! `StreamSubs` documents. Here there is no message per window at all: no script, no JSON, no
//! garbage. And no delivery to hold up: a window drag's modal loop, which held `Channel` data back
//! by up to ~135 ms, can't touch memory the capture thread writes directly.
//!
//! The ring (little-endian, read-only to the page):
//!
//! | offset | header                                            |
//! |--------|---------------------------------------------------|
//! | 0      | magic `RING_MAGIC` (u32)                          |
//! | 4      | version `RING_VERSION` (u32)                      |
//! | 8      | slot count (u32)                                  |
//! | 12     | slot size in bytes (u32)                          |
//! | 16     | windows written so far (u32) — set after each one |
//!
//! Slots from `HEADER_BYTES`, window `k` in slot `k % SLOTS`:
//!
//! | offset | slot                                                          |
//! |--------|---------------------------------------------------------------|
//! | 0      | `k + 1` once complete, 0 while being written (u32)            |
//! | 4      | sample pairs `n` (u32)                                        |
//! | 8      | rate, Hz (u32)                                                |
//! | 12     | flags: bit 0 = signal (u32)                                   |
//! | 16     | `t0`, 100 ns performance-counter units (f64, exact below 2^53) |
//! | 24     | `sent`, same units (f64)                                      |
//! | 32     | `n` (l, r) pairs (f32)                                        |
//!
//! The page reads a window only once the count says it's complete, and only within the last
//! `SLOTS - SLOT_MARGIN` — a slot is rewritten `SLOTS` windows later, so a read would have to stall
//! for most of a second mid-copy to see a slot change under it, and the slot's own number says so
//! if it did. Writes are ordered (slot number zeroed, data, slot number, count) with release
//! stores; x86 keeps stores in order for the reader in the other process.

use std::collections::HashMap;
use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

pub const RING_MAGIC: u32 = 0x4253_4743; // "CGSB"
pub const RING_VERSION: u32 = 1;
/// Windows the ring holds: 640 ms at 100 a second.
pub const SLOTS: u32 = 64;
/// The most sample pairs a slot takes: what a window can carry at the capture rate cap (96 kHz ×
/// 16 ms × 3, cageq-monitor's `SCOPE_MAX_POINTS`); a longer one keeps its newest samples.
pub const SLOT_PAIRS: usize = 4608;
pub const HEADER_BYTES: usize = 64;
const SLOT_HEAD_BYTES: usize = 32;
/// Rounded up to 64 bytes, so every slot's f64s stay aligned.
pub const SLOT_BYTES: usize = (SLOT_HEAD_BYTES + SLOT_PAIRS * 8).div_ceil(64) * 64;
pub const RING_BYTES: usize = HEADER_BYTES + SLOTS as usize * SLOT_BYTES;

/// One webview's ring: the mapped memory, and the WebView2 object that keeps it mapped.
struct Ring {
    ptr: *mut u8,
    written: u32,
    // Dropped (released) only on the main thread: `drop_ring` and `subscribe` both run there.
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

/// Every webview's ring, by label.
#[derive(Default, Clone)]
pub struct SharedScope(Arc<Mutex<HashMap<String, Ring>>>);

impl SharedScope {
    /// Write one scope window into every ring. Called on the capture thread.
    pub fn write(&self, update: &cageq_monitor::ScopeUpdate) {
        let Ok(mut rings) = self.0.lock() else { return };
        for ring in rings.values_mut() {
            // SAFETY: the ring's memory is `RING_BYTES` long and mapped while it's in the map.
            unsafe { ring.write(update) };
        }
    }

    /// Drop a webview's ring (it was destroyed). Main thread.
    pub fn drop_ring(&self, label: &str) {
        if let Ok(mut rings) = self.0.lock() {
            rings.remove(label);
        }
    }
}

impl Ring {
    /// SAFETY: `self.ptr` must point at `RING_BYTES` of writable memory.
    unsafe fn write(&mut self, update: &cageq_monitor::ScopeUpdate) {
        let k = self.written;
        let slot = unsafe { self.ptr.add(HEADER_BYTES + (k % SLOTS) as usize * SLOT_BYTES) };
        let pairs = update.xy.len() / 2;
        let n = pairs.min(SLOT_PAIRS);
        let skipped = pairs - n;
        let t0 = if update.t0 == 0 || update.rate == 0 {
            0
        } else {
            update.t0 + (skipped as u64 * 10_000_000) / update.rate as u64
        };
        unsafe {
            let seq = AtomicU32::from_ptr(slot as *mut u32);
            seq.store(0, Ordering::Relaxed);
            fence(Ordering::Release); // the slot reads as busy before any of it changes
            (slot.add(4) as *mut u32).write(n as u32);
            (slot.add(8) as *mut u32).write(update.rate);
            (slot.add(12) as *mut u32).write(u32::from(update.signal));
            (slot.add(16) as *mut f64).write(t0 as f64);
            (slot.add(24) as *mut f64).write(update.sent as f64);
            std::ptr::copy_nonoverlapping(update.xy[skipped * 2..].as_ptr(), slot.add(SLOT_HEAD_BYTES) as *mut f32, n * 2);
            seq.store(k.wrapping_add(1), Ordering::Release);
            AtomicU32::from_ptr(self.ptr.add(16) as *mut u32).store(k.wrapping_add(1), Ordering::Release);
        }
        self.written = k.wrapping_add(1);
    }
}

/// Create `webview`'s ring and hand it to the page (replacing any it had: a reload). Must run on
/// the main thread, as WebView2's API does — from `with_webview`'s closure.
pub fn create(shared: &SharedScope, label: &str, pw: tauri::webview::PlatformWebview) -> Result<(), String> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2Environment12, ICoreWebView2_17, COREWEBVIEW2_SHARED_BUFFER_ACCESS_READ_ONLY,
    };
    use windows_core::Interface;
    let err = |what: &str, e: windows_core::Error| format!("shared scope buffer: {what}: {e}");
    unsafe {
        let env: ICoreWebView2Environment12 = pw.environment().cast().map_err(|e| err("environment", e))?;
        let core: ICoreWebView2_17 = pw
            .controller()
            .CoreWebView2()
            .and_then(|c| c.cast())
            .map_err(|e| err("webview", e))?;
        let buffer = env.CreateSharedBuffer(RING_BYTES as u64).map_err(|e| err("create", e))?;
        let mut ptr = std::ptr::null_mut();
        buffer.Buffer(&mut ptr).map_err(|e| err("map", e))?;
        if ptr.is_null() {
            return Err("shared scope buffer: no memory".into());
        }
        std::ptr::write_bytes(ptr, 0, RING_BYTES);
        (ptr as *mut u32).write(RING_MAGIC);
        (ptr.add(4) as *mut u32).write(RING_VERSION);
        (ptr.add(8) as *mut u32).write(SLOTS);
        (ptr.add(12) as *mut u32).write(SLOT_BYTES as u32);
        // In the map before the page sees it, so the first window after can't be missed.
        if let Ok(mut rings) = shared.0.lock() {
            rings.insert(label.to_string(), Ring { ptr, written: 0, _buffer: Keep(buffer.clone()) });
        }
        if let Err(e) = core.PostSharedBufferToScript(
            &buffer,
            COREWEBVIEW2_SHARED_BUFFER_ACCESS_READ_ONLY,
            windows_core::w!(r#"{"kind":"scope"}"#),
        ) {
            shared.drop_ring(label);
            return Err(err("post", e));
        }
    }
    Ok(())
}
