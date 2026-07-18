//! Integration tests for the on-disk paths of `cageq-config-writer`.
//!
//! A file under `tests/` is compiled as a separate crate seeing only the public
//! API — exactly how the Tauri core will use it. These drive `apply` /
//! `write_cageq_txt` / `ensure_include` / `read_cageq_state` / `write_safe_state`
//! against real files in a temp directory (which stands in for EqAPO's config dir).

use std::fs;
use std::path::{Path, PathBuf};

use cageq_config_writer::{
    apply, decide_startup, read_cageq_state, write_safe_state, BlockState, DeviceConfig, Filter,
    FilterType, StartupDecision, WriteError, CAGEQ_FILENAME,
};

/// RAII temp directory (a unique dir under the OS temp dir) that deletes itself —
/// contents and all — when it goes out of scope. Demonstrates `Drop`: the
/// destructor runs at end of scope even on panic. Each test uses a distinct `tag`
/// because `cargo test` runs tests in parallel threads.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("cageq-it-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn dir(&self) -> &Path {
        &self.0
    }
    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.0.join(name)).unwrap()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sample() -> Vec<DeviceConfig> {
    vec![DeviceConfig {
        device: "USB DAC".to_string(),
        preamp_db: -9.0,
        filters: vec![
            Filter { kind: FilterType::LowShelf, freq_hz: 105.0, gain_db: 3.0, q: 0.7 },
            Filter { kind: FilterType::Peaking, freq_hz: 2500.0, gain_db: -2.4, q: 1.4 },
        ],
    }]
}

#[test]
fn apply_creates_both_files_and_state_round_trips() {
    let tmp = TempDir::new("apply");
    // Fresh config dir: apply must create config.txt (with the Include block) and
    // cageq.txt (with the filters).
    let hash = apply(tmp.dir(), &sample()).unwrap();

    let config = tmp.read("config.txt");
    assert!(config.contains("#CAGEq:BEGIN"));
    assert!(config.contains("Include: cageq.txt"));

    let cageq = tmp.read(CAGEQ_FILENAME);
    assert!(cageq.contains("Device: USB DAC"));
    assert!(cageq.contains("Filter 1: ON LSC Fc 105 Hz Gain 3.0 dB Q 0.70"));

    // read_cageq_state must recompute the exact hash apply returned.
    match read_cageq_state(&tmp.dir().join(CAGEQ_FILENAME)).unwrap() {
        BlockState::Present { stored_hash, actual_hash } => {
            assert_eq!(stored_hash.as_deref(), Some(hash.as_str()));
            assert_eq!(actual_hash, hash);
        }
        BlockState::Absent => panic!("expected a present cageq.txt right after writing it"),
    }
}

#[test]
fn apply_preserves_foreign_config_txt_and_is_idempotent() {
    let tmp = TempDir::new("foreign");
    let foreign = "Device: Other\nFilter: ON PK Fc 1000 Hz Gain 2 dB Q 1\n";
    fs::write(tmp.dir().join("config.txt"), foreign).unwrap();

    apply(tmp.dir(), &sample()).unwrap();
    let after_first = tmp.read("config.txt");
    assert!(after_first.contains(foreign), "foreign content was not preserved");
    assert!(after_first.contains("Include: cageq.txt"));

    // A second apply must not add a duplicate Include block.
    apply(tmp.dir(), &sample()).unwrap();
    let after_second = tmp.read("config.txt");
    assert_eq!(after_second.matches("#CAGEq:BEGIN").count(), 1, "Include block was duplicated");
    // config.txt is unchanged the second time (Include already correct).
    assert_eq!(after_first, after_second);
}

#[test]
fn a_second_apply_rewrites_only_cageq_txt() {
    let tmp = TempDir::new("rewrite");
    apply(tmp.dir(), &sample()).unwrap();

    let mut changed = sample();
    changed[0].preamp_db = -6.0;
    apply(tmp.dir(), &changed).unwrap();

    let cageq = tmp.read(CAGEQ_FILENAME);
    assert!(cageq.contains("Preamp: -6.0 dB"));
    assert!(!cageq.contains("Preamp: -9.0 dB"));
}

#[test]
fn a_non_utf8_config_txt_fails_loudly_with_notutf8() {
    let tmp = TempDir::new("ansi");
    let bytes: [u8; 5] = [b'D', b'e', b'v', 0xFF, b'\n']; // 0xFF is not valid UTF-8
    fs::write(tmp.dir().join("config.txt"), bytes).unwrap();

    let err = apply(tmp.dir(), &sample()).unwrap_err();
    assert!(matches!(err, WriteError::NotUtf8), "expected NotUtf8, got {err:?}");
}

#[test]
fn safe_state_is_written_and_recognised_at_startup() {
    let tmp = TempDir::new("safestate");
    let cageq_path = tmp.dir().join(CAGEQ_FILENAME);
    write_safe_state(&cageq_path).unwrap();

    let state = read_cageq_state(&cageq_path).unwrap();
    // settings.json still remembers some earlier real-config hash.
    assert_eq!(decide_startup(&state, Some("deadbeef")), StartupDecision::SafeStateStillActive);
}
