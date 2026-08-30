//! Stamps each build with the time it was compiled.
//!
//! Exists because "is audiodg running the DLL I just built?" has been genuinely ambiguous
//! more than once, and guessing wrong sends you looking for DSP bugs that were fixed hours
//! ago. The stamp is published in the control block, so `push.exe --watch` can report the
//! build actually loaded rather than the one on disk.
fn main() {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=CAGEQ_APO_BUILD={secs}");
    // No `rerun-if-changed`: this must re-run whenever anything in the crate does, or the
    // stamp would go stale and defeat its own purpose.
}
