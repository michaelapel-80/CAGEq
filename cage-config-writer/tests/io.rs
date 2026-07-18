//! Integration tests for the on-disk paths of `cage-config-writer`.
//!
//! Unlike the in-crate `#[cfg(test)] mod tests` (which drive the pure string
//! helpers with no I/O), a file under `tests/` is compiled as a *separate crate*
//! that sees only the public API — exactly how the real consumer (the Tauri
//! core) will use it. So these exercise `write_managed_block` / `read_block_state`
//! against real files, covering the fs::read/write/rename glue the unit tests
//! can't reach.

use std::fs;
use std::path::{Path, PathBuf};

use cage_config_writer::{
    read_block_state, write_managed_block, BlockState, Filter, FilterType, WriteError,
};

/// RAII temp path: a unique file in the OS temp dir that deletes itself — and any
/// leftover sibling temp file — when it goes out of scope. Demonstrates Rust's
/// `Drop`: the destructor runs automatically at end of scope, even on panic, so
/// a failing test can't leave junk behind. Each test uses a distinct `tag`
/// because `cargo test` runs tests in parallel threads.
struct TempPath(PathBuf);

impl TempPath {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("cage-it-{}-{tag}.txt", std::process::id()));
        let _ = fs::remove_file(&p); // clear any stale leftover from a prior run
        TempPath(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn sample() -> Vec<Filter> {
    vec![
        Filter { kind: FilterType::LowShelf, freq_hz: 105.0, gain_db: 3.0, q: 0.7 },
        Filter { kind: FilterType::Peaking, freq_hz: 2500.0, gain_db: -2.4, q: 1.4 },
    ]
}

#[test]
fn writes_a_fresh_file_then_reads_back_a_matching_hash() {
    let tmp = TempPath::new("roundtrip");
    // The file does not exist yet: the write must create it (NotFound path).
    let hash = write_managed_block(tmp.path(), "USB DAC", -9.0, &sample()).unwrap();

    let text = fs::read_to_string(tmp.path()).unwrap();
    assert!(text.contains("Device: USB DAC"));
    assert!(text.contains("#CAGE:BEGIN"));
    // Exercises the AutoEq-verified line format end-to-end (number, token, units).
    assert!(text.contains("Filter 1: ON LSC Fc 105 Hz Gain 3.0 dB Q 0.70"));
    assert!(text.contains("Filter 2: ON PK Fc 2500 Hz Gain -2.4 dB Q 1.40"));

    // read_block_state must recompute the exact hash the write returned.
    match read_block_state(tmp.path(), "USB DAC").unwrap() {
        BlockState::Present { stored_hash, actual_hash } => {
            assert_eq!(stored_hash.as_deref(), Some(hash.as_str()));
            assert_eq!(actual_hash, hash);
        }
        BlockState::Absent => panic!("expected a present block right after writing it"),
    }
}

#[test]
fn a_foreign_device_block_survives_a_real_write() {
    let tmp = TempPath::new("foreign");
    let foreign = "Device: Other\nFilter: ON PK Fc 1000 Hz Gain 2 dB Q 1\n";
    fs::write(tmp.path(), foreign).unwrap();

    write_managed_block(tmp.path(), "USB DAC", -9.0, &sample()).unwrap();

    let text = fs::read_to_string(tmp.path()).unwrap();
    assert!(text.contains(foreign), "foreign device block was not preserved verbatim");
    assert!(text.contains("Device: USB DAC"));
}

#[test]
fn a_second_write_replaces_in_place_on_disk() {
    let tmp = TempPath::new("replace");
    write_managed_block(tmp.path(), "USB DAC", -9.0, &sample()).unwrap();
    write_managed_block(tmp.path(), "USB DAC", -6.0, &sample()).unwrap();

    let text = fs::read_to_string(tmp.path()).unwrap();
    assert_eq!(text.matches("#CAGE:BEGIN").count(), 1, "must not accumulate blocks");
    assert!(text.contains("Preamp: -6.0 dB"));
    assert!(!text.contains("Preamp: -9.0 dB"));
}

#[test]
fn state_is_absent_for_a_device_never_written() {
    let tmp = TempPath::new("absent");
    fs::write(tmp.path(), "Device: Someone Else\nPreamp: -3.0 dB\n").unwrap();
    assert!(matches!(
        read_block_state(tmp.path(), "USB DAC").unwrap(),
        BlockState::Absent
    ));
}

#[test]
fn a_non_utf8_config_fails_loudly_with_notutf8() {
    let tmp = TempPath::new("ansi");
    // 0xFF is not a valid UTF-8 byte — stands in for a legacy ANSI-saved config.
    let bytes: [u8; 5] = [b'D', b'e', b'v', 0xFF, b'\n'];
    fs::write(tmp.path(), bytes).unwrap();

    let err = write_managed_block(tmp.path(), "USB DAC", -9.0, &sample()).unwrap_err();
    assert!(matches!(err, WriteError::NotUtf8), "expected NotUtf8, got {err:?}");
}
