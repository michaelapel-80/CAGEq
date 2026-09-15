//! Whether the account `audiodg` actually runs as can read the installed APO DLL.
//!
//! [`setup::SetupStatus::dll_present`](crate::setup::SetupStatus) is `path.exists()` from
//! whichever account is running the *caller* — proof the interactive user can see the file,
//! not that LocalService (audiodg's real account, well-known SID `S-1-5-19`) can. Those are
//! different security principals and can diverge without anything else noticing: AV/EDR
//! hardening or a restrictive Group Policy can pull LocalService's access without touching
//! the interactive user's.
//!
//! This checks the file's real DACL against the SIDs that matter, rather than trying to
//! impersonate the account — that would need `SeDebugPrivilege` and a handle to the running
//! `audiodg` process to duplicate its token, the same cost as the module-enumeration
//! approach already tried and abandoned for a different diagnostic. `GetNamedSecurityInfoW`
//! only needs `READ_CONTROL`, which any user normally has on a Program Files file, so this
//! needs no elevation.

#![cfg(windows)]

use std::ffi::c_void;
use std::path::Path;

const SE_FILE_OBJECT: u32 = 1;
const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
const ERROR_SUCCESS: u32 = 0;

// NTFS "Read & Execute", spelled out from its known bit values rather than pulled from a
// header — this crate deliberately has no `windows`/`windows-sys` dependency (see the
// module doc; `cageq-apo/src/channel.rs` sets the same precedent).
const FILE_GENERIC_READ: u32 = 0x0012_0089;
const FILE_GENERIC_EXECUTE: u32 = 0x0012_00A0;
const REQUIRED_ACCESS: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;

/// LOCAL SERVICE itself, and `BUILTIN\Users` — the group that grants it access on a normal
/// install (LOCAL SERVICE is an implicit member by default; confirmed against a real
/// install's ACL, which grants `BUILTIN\Users` `ReadAndExecute` and has no separate LOCAL
/// SERVICE entry at all). Either SID having the required access is enough.
const CANDIDATE_SIDS: [&str; 2] = ["S-1-5-19", "S-1-5-32-545"];

#[repr(C)]
struct TrusteeW {
    p_multiple_trustee: *mut c_void,
    multiple_trustee_operation: i32,
    trustee_form: i32,
    trustee_type: i32,
    name: *mut c_void,
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSidToSidW(string_sid: *const u16, sid: *mut *mut c_void) -> i32;
    fn GetNamedSecurityInfoW(
        object_name: *const u16,
        object_type: u32,
        security_info: u32,
        owner: *mut *mut c_void,
        group: *mut *mut c_void,
        dacl: *mut *mut c_void,
        sacl: *mut *mut c_void,
        security_descriptor: *mut *mut c_void,
    ) -> u32;
    fn BuildTrusteeWithSidW(trustee: *mut TrusteeW, sid: *mut c_void);
    fn GetEffectiveRightsFromAclW(
        acl: *mut c_void,
        trustee: *const TrusteeW,
        access_rights: *mut u32,
    ) -> u32;
    fn LocalFree(mem: *mut c_void) -> *mut c_void;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Owns a `LocalAlloc`-family pointer (a security descriptor or a SID), freed on every path
/// out — including the error ones, which is exactly where a hand-rolled `LocalFree` gets
/// forgotten. Mirrors `cageq-apo/src/channel.rs`'s `SecurityDescriptor` guard.
struct LocalAlloc(*mut c_void);

impl Drop for LocalAlloc {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

/// Whether audiodg (LOCAL SERVICE) can read and execute `path` — `None` if the check itself
/// could not run (file not found, an API failure). Treated as "not blocking" by every
/// caller: this is a diagnostic hint, not a security decision, and a failed *check* must
/// never read the same as a confirmed *block*.
pub fn dll_readable_by_audiodg(path: &Path) -> Option<bool> {
    let path_w = wide(&path.to_string_lossy());

    let mut dacl: *mut c_void = std::ptr::null_mut();
    let mut sd: *mut c_void = std::ptr::null_mut();
    // SAFETY: `path_w` is NUL-terminated. The four `null_mut()` out-params are the documented
    // way to tell the API we don't want owner/group/SACL. `sd` is what actually owns the
    // returned security descriptor (including `dacl`, which points inside it) and must live
    // at least as long as `dacl` is used below — `_sd_guard` frees it on every return path.
    let err = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if err != ERROR_SUCCESS || sd.is_null() {
        return None;
    }
    let _sd_guard = LocalAlloc(sd);

    let any_ok = CANDIDATE_SIDS
        .iter()
        .any(|sid| effective_rights(dacl, sid).is_some_and(|r| r & REQUIRED_ACCESS == REQUIRED_ACCESS));
    Some(any_ok)
}

/// The effective access mask `sid_str` has against `dacl`, or `None` if the SID string
/// failed to parse or the API call itself failed — both silently skipped by the caller
/// rather than counted as a denial, since a well-known SID string failing to parse is an
/// environment problem, not evidence the DLL is unreadable.
fn effective_rights(dacl: *mut c_void, sid_str: &str) -> Option<u32> {
    let sid_w = wide(sid_str);
    let mut sid: *mut c_void = std::ptr::null_mut();
    // SAFETY: `sid_w` is NUL-terminated; on success `sid` is `LocalAlloc`'d and freed below.
    let ok = unsafe { ConvertStringSidToSidW(sid_w.as_ptr(), &mut sid) };
    if ok == 0 || sid.is_null() {
        return None;
    }
    let _sid_guard = LocalAlloc(sid);

    let mut trustee = TrusteeW {
        p_multiple_trustee: std::ptr::null_mut(),
        multiple_trustee_operation: 0,
        trustee_form: 0,
        trustee_type: 0,
        name: std::ptr::null_mut(),
    };
    // SAFETY: `trustee` is a fully-initialized out-param; `sid` outlives this call (freed
    // only when `_sid_guard` drops, after `GetEffectiveRightsFromAclW` returns below).
    unsafe { BuildTrusteeWithSidW(&mut trustee, sid) };

    let mut rights: u32 = 0;
    // SAFETY: `dacl` came from a successful `GetNamedSecurityInfoW` call in the caller and is
    // still alive (owned by its `_sd_guard`); `trustee` was just built above.
    let err = unsafe { GetEffectiveRightsFromAclW(dacl, &trustee, &mut rights) };
    if err != ERROR_SUCCESS { None } else { Some(rights) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The running test binary itself: guaranteed to exist with an absolute path, and
    /// guaranteed the current user (who just executed it) can read — a sanity check that the
    /// API plumbing itself works, not a test of any particular ACL.
    #[test]
    fn reads_the_test_binarys_own_acl_without_erroring() {
        let exe = std::env::current_exe().expect("test binary has a path");
        let result = dll_readable_by_audiodg(&exe);
        assert!(result.is_some(), "the ACL check should succeed against a file that exists");
    }

    #[test]
    fn missing_file_reports_none() {
        let missing = Path::new(r"C:\this\path\does\not\exist\CAGEqApoNoSuchFile.dll");
        assert_eq!(dll_readable_by_audiodg(missing), None);
    }
}
