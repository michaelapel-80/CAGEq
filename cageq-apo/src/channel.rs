//! The control channel's **OS plumbing**: a named shared-memory section that CAGEq writes
//! and the APO reads. The protocol carried over it — layout, seqlock, validation — is
//! [`crate::control`], deliberately kept separate so the part that decides what bytes may
//! mean is testable without an operating system.
//!
//! ## Direction and privilege
//! The APO **creates** the section, from inside `audiodg` (LocalService, session 0); CAGEq
//! opens it from the user's session. That direction is forced: a `Global\` name is what
//! crosses the session-0 boundary, and creating one requires `SeCreateGlobalPrivilege`,
//! which service accounts hold and ordinary users do not.
//!
//! That asymmetry is also a useful protection: because a standard user cannot create a
//! `Global\` object at all, they cannot *squat* the name ahead of us and hand the APO a
//! section with a security descriptor of their choosing. Squatting here needs administrator
//! rights, at which point the game is already lost by other means.
//!
//! ## Access control
//! One SDDL string, below, rather than hand-assembled ACLs — a security decision worth being
//! able to read at a glance and re-audit later:
//!
//! * SYSTEM and Administrators: full control.
//! * Authenticated Users: read and write. CAGEq runs unelevated as the interactive user and
//!   must be able to push coefficients.
//! * A **Medium** mandatory label with no-write-up. This is the part that matters: objects
//!   without an explicit label are implicitly Medium anyway, but stating it makes the
//!   intent legible and pins the behaviour against a default changing. It excludes Low
//!   integrity — sandboxed browser content and AppContainer apps — which is precisely the
//!   class we do not want able to reach into audio processing.
//!
//! This bounds *who* can write. What they can achieve by writing is bounded separately, and
//! more importantly, by [`crate::control`]'s validation and the cascade's loudness ceiling:
//! a hostile writer can change what you hear within safe limits, and cannot crash audiodg or
//! execute anything.

#![cfg(windows)]

use std::ffi::c_void;

use crate::config::is_valid_endpoint_id;
use crate::control::ControlBlock;

type Handle = *mut c_void;

const PAGE_READWRITE: u32 = 0x04;
const FILE_MAP_ALL_ACCESS: u32 = 0x000F_001F;
const SDDL_REVISION_1: u32 = 1;

/// See the module doc. `GA` = generic all, `GRGW` = generic read + write, `SY` = SYSTEM,
/// `BA` = builtin administrators, `AU` = authenticated users; the `S:` clause is the
/// mandatory label — `ML`, no-write-up, `ME` = medium integrity.
const CHANNEL_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)S:(ML;;NW;;;ME)";

#[repr(C)]
struct SecurityAttributes {
    n_length: u32,
    lp_security_descriptor: *mut c_void,
    b_inherit_handle: i32,
}

// Split by defining DLL so the linker is told where each lives: the section APIs are
// kernel32 (linked by default), the SDDL helper is advapi32 (not).
#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string_security_descriptor: *const u16,
        string_sd_revision: u32,
        security_descriptor: *mut *mut c_void,
        security_descriptor_size: *mut u32,
    ) -> i32;
    fn LocalFree(h_mem: *mut c_void) -> *mut c_void;
}

unsafe extern "system" {
    fn CreateFileMappingW(
        h_file: Handle,
        lp_attributes: *const SecurityAttributes,
        fl_protect: u32,
        dw_maximum_size_high: u32,
        dw_maximum_size_low: u32,
        lp_name: *const u16,
    ) -> Handle;
    fn MapViewOfFile(
        h_file_mapping_object: Handle,
        dw_desired_access: u32,
        dw_file_offset_high: u32,
        dw_file_offset_low: u32,
        dw_number_of_bytes_to_map: usize,
    ) -> *mut c_void;
    fn UnmapViewOfFile(lp_base_address: *const c_void) -> i32;
    fn CloseHandle(h_object: Handle) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Owns a security descriptor allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`,
/// so it is released on every path out — including the error ones, which is exactly where a
/// hand-rolled `LocalFree` gets forgotten.
struct SecurityDescriptor(*mut c_void);

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> Option<SecurityDescriptor> {
        let text = wide(sddl);
        let mut psd: *mut c_void = std::ptr::null_mut();
        // SAFETY: `text` is NUL-terminated; the callee writes at most one pointer to `psd`.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut psd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || psd.is_null() { None } else { Some(SecurityDescriptor(psd)) }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

/// A mapped control block: the section handle plus the view, released together.
pub struct ControlChannel {
    handle: Handle,
    view: *mut ControlBlock,
}

// SAFETY: created on the configuration thread (`LockForProcess`) and thereafter read from
// audiodg's real-time thread. The pointed-to block is only ever touched through
// `crate::control`, whose reads are seqlock-guarded and whose only writes are atomics, so
// moving the owning handle between those two threads is sound.
unsafe impl Send for ControlChannel {}
unsafe impl Sync for ControlChannel {}

impl ControlChannel {
    /// Create (or re-attach to) this endpoint's control section.
    ///
    /// `None` on any failure — no channel simply means no live edits, and the APO keeps
    /// applying its persistent configuration. This must never be a reason to fail a lock and
    /// silence the endpoint.
    ///
    /// An already-existing section is reused rather than treated as an error: a format change
    /// re-locks the APO, and inheriting the coefficients CAGEq last pushed is exactly what
    /// should happen. (The security descriptor is only applied at creation, but see the
    /// module doc on why a standard user cannot get there first.)
    pub fn create(endpoint_id: &str) -> Option<ControlChannel> {
        if !is_valid_endpoint_id(endpoint_id) {
            return None;
        }
        let sd = SecurityDescriptor::from_sddl(CHANNEL_SDDL)?;
        let sa = SecurityAttributes {
            n_length: std::mem::size_of::<SecurityAttributes>() as u32,
            lp_security_descriptor: sd.0,
            b_inherit_handle: 0,
        };

        // Per endpoint: each has its own APO instance and its own correction.
        let name = wide(&format!("Global\\CAGEqApo_{endpoint_id}"));
        let size = std::mem::size_of::<ControlBlock>() as u32;

        // SAFETY: INVALID_HANDLE_VALUE (-1) requests a pagefile-backed section; `name` is
        // NUL-terminated; `sa` outlives the call.
        let handle = unsafe {
            CreateFileMappingW(
                usize::MAX as Handle,
                &sa,
                PAGE_READWRITE,
                0,
                size,
                name.as_ptr(),
            )
        };
        if handle.is_null() {
            return None;
        }

        // SAFETY: `handle` is a valid section of at least `size` bytes.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size as usize) };
        if view.is_null() {
            unsafe { CloseHandle(handle) };
            return None;
        }

        // A freshly created section is zero-filled, which `control::try_read` reports as
        // `Unrecognised` — deliberately, so an unwritten block is never mistaken for a valid
        // "no filters" instruction. Nothing is initialised here: initialising it would mean
        // this side inventing a state CAGEq never published.
        Some(ControlChannel { handle, view: view as *mut ControlBlock })
    }

    /// The mapped block.
    ///
    /// Shared with another process by construction, so every field is untrusted and must go
    /// through [`crate::control`]'s validation rather than being read directly.
    pub fn block(&self) -> &ControlBlock {
        // SAFETY: mapped for the lifetime of `self`, and at least `size_of::<ControlBlock>()`.
        unsafe { &*self.view }
    }
}

impl Drop for ControlChannel {
    fn drop(&mut self) {
        // Unmap before closing, and tolerate either failing: this runs during teardown inside
        // audiodg, where there is nobody to tell and nothing useful to do about it.
        unsafe {
            UnmapViewOfFile(self.view as *const c_void);
            CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{self, RawCoeffs, ReadOutcome, Snapshot};

    fn test_endpoint() -> String {
        // Unique per process so concurrent test runs cannot collide on the section name.
        format!("{{6cafe423-cde5-4ec1-a1e2-{:012x}}}", std::process::id())
    }

    /// The SDDL must actually parse — a typo here would silently become "no channel", and
    /// the fallback (no live edits) is quiet enough that it could go unnoticed for a long time.
    #[test]
    fn the_access_control_string_is_valid() {
        assert!(
            SecurityDescriptor::from_sddl(CHANNEL_SDDL).is_some(),
            "CHANNEL_SDDL failed to parse: {CHANNEL_SDDL}",
        );
    }

    /// Ids that could not name a real endpoint are refused before they reach an object name.
    #[test]
    fn implausible_endpoint_ids_do_not_become_section_names() {
        for bad in ["", "../escape", "a\\b", &"f".repeat(200)] {
            assert!(ControlChannel::create(bad).is_none(), "accepted {bad:?}");
        }
    }

    /// End-to-end through real shared memory: create the section, publish through the mapped
    /// view as CAGEq would, and read it back through the validating path.
    ///
    /// Creating a `Global\` section needs SeCreateGlobalPrivilege, which a normal developer
    /// account does not hold — so this skips rather than fails when it cannot create one.
    /// (In production the creator is audiodg, which does hold it.)
    #[test]
    fn a_real_section_round_trips_a_published_update() {
        let Some(ch) = ControlChannel::create(&test_endpoint()) else {
            eprintln!("skipping: could not create a Global\\ section (needs SeCreateGlobalPrivilege)");
            return;
        };

        // A fresh section is zeroed, and that must NOT read as a valid empty correction.
        let mut snap = Snapshot::default();
        assert_eq!(
            control::try_read(ch.block(), &mut snap),
            ReadOutcome::Unrecognised,
            "an unwritten section must not be mistaken for a published one",
        );

        // Publish as the writer would, through the shared view.
        let stable = RawCoeffs { b0: 1.02, b1: -1.9, b2: 0.89, a1: -1.9, a2: 0.91 };
        // SAFETY: single-threaded test; the view is ours and correctly sized.
        let block = unsafe { &mut *(ch.view) };
        assert!(control::publish(block, -6.0, &[stable, stable]));

        match control::try_read(ch.block(), &mut snap) {
            ReadOutcome::Updated(_) => {}
            other => panic!("expected Updated through shared memory, got {other:?}"),
        }
        assert_eq!(snap.band_count, 2);
        assert_eq!(snap.preamp_db, -6.0);
    }

    /// Re-attaching to an existing section keeps what was published — a format change
    /// re-locks the APO, and losing the live correction there would be a regression the user
    /// would hear.
    #[test]
    fn reattaching_preserves_the_published_state() {
        let id = test_endpoint();
        let Some(first) = ControlChannel::create(&id) else { return };
        let stable = RawCoeffs { b0: 1.02, b1: -1.9, b2: 0.89, a1: -1.9, a2: 0.91 };
        assert!(control::publish(unsafe { &mut *first.view }, -9.0, &[stable]));

        // A second attach while the first is still open maps the same section.
        let Some(second) = ControlChannel::create(&id) else { return };
        let mut snap = Snapshot::default();
        assert!(matches!(control::try_read(second.block(), &mut snap), ReadOutcome::Updated(_)));
        assert_eq!(snap.preamp_db, -9.0);
        assert_eq!(snap.band_count, 1);
    }
}
