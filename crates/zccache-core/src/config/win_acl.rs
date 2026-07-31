//! Owner-only DACL for the daemon deploy directory on Windows (#1172 F1e).
//!
//! [`ensure_dir_private`](super::paths::ensure_dir_private) expressed "private"
//! as unix mode bits and did nothing at all on Windows, so the directory the
//! CLI copies the daemon binary into — and then *executes* — was left with
//! whatever the profile tree inherits. That is normally
//!
//! ```text
//! D:AI(A;OICIID;FA;;;SY)(A;OICIID;FA;;;BA)(A;OICIID;FA;;;S-1-5-21-…-1001)
//! ```
//!
//! under `%USERPROFILE%`, which is already narrow — but nothing *enforced* it.
//! A cache root relocated by `ZCCACHE_CACHE_DIR` to `C:\ProgramData\…`,
//! `C:\zccache`, or any directory created off the volume root inherits
//! `BUILTIN\Users:(OI)(CI)(M)` or `CREATOR OWNER` grants instead, and then any
//! local account can replace `zccache-daemon.exe` between the CLI's integrity
//! check and the spawn. Whoever can write that directory chooses what the CLI
//! runs as the daemon.
//!
//! The replacement mirrors the named-pipe descriptor from #1272
//! (`transport/pipe_security.rs`): `D:P(A;OICI;FA;;;<user>)(A;OICI;FA;;;SY)`.
//! The `P` (PROTECTED) flag is load-bearing — without it the inherited `Users`
//! and `CREATOR OWNER` aces this exists to remove come straight back. `OICI`
//! makes the deployed binary itself inherit the same pair, so the file is
//! covered and not just its directory.
//!
//! Two deliberate differences from the pipe:
//!
//! * The trustee is the **current token user's SID**, not `OW` (OWNER RIGHTS).
//!   `OW` resolves through object ownership, and a directory created by an
//!   elevated process is owned by `BUILTIN\Administrators`, not by the user —
//!   which would leave the CLI unable to write its own deploy directory. The
//!   explicit SID is the account that actually deploys and spawns.
//! * `SYSTEM` and `BUILTIN\Administrators` are *accepted* when already present
//!   on a directory, and never treated as a finding. An administrator can take
//!   ownership regardless, so an ACE for them grants nothing an attacker at
//!   that privilege level did not already have.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HLOCAL};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
    GetNamedSecurityInfoW, SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, ACL,
    DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Inheritance flags on every ACE we write: `OBJECT_INHERIT` +
/// `CONTAINER_INHERIT`, so the deployed `zccache-daemon.exe` carries the same
/// owner-only pair rather than relying on the directory alone.
const ACE_INHERIT_FLAGS: &str = "OICI";

/// `FILE_ALL_ACCESS`. Spelled as the file-object right rather than the generic
/// `GA` the pipe uses, because `icacls` and the round-tripped SDDL report a
/// file-object DACL in mapped form and an unmapped `GA` would make the
/// already-private comparison below never match its own output.
const ACE_RIGHTS: &str = "FA";

/// Trustees that may appear in a deploy-directory DACL without it counting as
/// exposed. Both the two-letter SDDL alias and the raw SID are listed because
/// which one comes back from
/// `ConvertSecurityDescriptorToStringSecurityDescriptorW` is not contractual.
const ALLOWED_TRUSTEES: &[&str] = &[
    "SY",           // NT AUTHORITY\SYSTEM
    "S-1-5-18",     //   "
    "BA",           // BUILTIN\Administrators
    "S-1-5-32-544", //   "
];

/// Number of `;`-separated fields in an SDDL ACE; the trustee is the last one.
const ACE_FIELDS: usize = 6;

/// Ensure `path` is writable only by the current user (plus SYSTEM and
/// Administrators), applying an explicit protected DACL when it is not.
///
/// Returns `Ok(false)` when the directory was already private, `Ok(true)` when
/// it was tightened, and `Err` when it is still exposed afterwards — the same
/// three outcomes the unix arm reports, so the caller's "tightened" /
/// "refused" lifecycle contract is unchanged.
pub(super) fn ensure_dir_private(path: &Path) -> io::Result<bool> {
    // Matches the unix arm's `metadata()` probe: a missing directory is an
    // error, not a silent pass — the caller is about to deploy a binary here.
    let _ = std::fs::metadata(path)?;

    let user_sid = current_user_sid()?;
    let current = dacl_sddl(path)?;
    if is_owner_only(&current, &user_sid) {
        return Ok(false);
    }

    apply_dacl(path, &owner_only_sddl(&user_sid))?;

    // Read back rather than trusting the write. `SetNamedSecurityInfoW` can
    // report success on filesystems that do not carry ACLs at all (FAT32, some
    // network redirectors), and this is exactly the case where a false negative
    // is expensive.
    let after = dacl_sddl(path)?;
    if !is_owner_only(&after, &user_sid) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is writable by other local users (DACL {after}) and could not be tightened",
                path.display()
            ),
        ));
    }
    Ok(true)
}

/// The protected owner-only DACL written to an exposed directory.
fn owner_only_sddl(user_sid: &str) -> String {
    format!("D:P(A;{ACE_INHERIT_FLAGS};{ACE_RIGHTS};;;{user_sid})(A;{ACE_INHERIT_FLAGS};{ACE_RIGHTS};;;SY)")
}

/// Is this DACL protected from inheritance *and* free of any trustee other
/// than the deploying user, SYSTEM, and Administrators?
///
/// Both halves matter. An unprotected DACL whose current aces happen to be
/// narrow is one `icacls` reset — or one move to a differently-permissioned
/// parent — away from re-acquiring `BUILTIN\Users`, so it is reported as
/// needing repair rather than accepted.
pub(super) fn is_owner_only(sddl: &str, user_sid: &str) -> bool {
    let Some(rest) = sddl.strip_prefix("D:") else {
        return false;
    };
    let flags = rest.split('(').next().unwrap_or_default();
    if !flags.contains('P') {
        return false;
    }
    // A NULL DACL grants everyone everything and renders as a flag word, with
    // no aces to iterate — the `P` check above already rejects it, but the
    // ace loop must not be read as "no aces means private".
    if rest.contains("NO_ACCESS_CONTROL") {
        return false;
    }
    aces(rest).all(|trustee| trustee_is_self_or_admin(trustee, user_sid))
}

/// Is this ACE trustee the running user, SYSTEM, or Administrators?
///
/// Compares **SIDs, not strings**. `ConvertSecurityDescriptorToStringSecurityDescriptorW`
/// substitutes a two-letter alias for well-known SIDs, and which SIDs count as
/// "well-known" is not something the caller controls: a process running as the
/// built-in Administrator gets its own account rendered as `LA`, so a raw-SID
/// string comparison reports the directory as exposed *after we just tightened
/// it*, and the read-back check then fails a DACL that is in fact correct.
///
/// CI found this — the Windows runner runs as that account and my dev host
/// does not.
fn trustee_is_self_or_admin(trustee: &str, user_sid: &str) -> bool {
    if trustee == user_sid || ALLOWED_TRUSTEES.contains(&trustee) {
        return true;
    }
    // Resolve both sides; `ConvertStringSidToSidW` accepts an alias as happily
    // as a raw SID, which is exactly the normalization the string compare
    // above lacks.
    match (sid_bytes(trustee), sid_bytes(user_sid)) {
        (Some(mut lhs), Some(mut rhs)) => {
            // SAFETY: both buffers hold a valid SID copied out under
            // `GetLengthSid`, and `EqualSid` only reads them.
            unsafe { EqualSid(lhs.as_mut_ptr().cast(), rhs.as_mut_ptr().cast()) != 0 }
        }
        _ => false,
    }
}

/// Copy the binary SID an SDDL trustee denotes, alias or raw.
fn sid_bytes(trustee: &str) -> Option<Vec<u8>> {
    let wide: Vec<u16> = trustee.encode_utf16().chain(std::iter::once(0)).collect();
    let mut psid: windows_sys::Win32::Security::PSID = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated; on success the SID is LocalAlloc'd and
    // freed below on every path.
    if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut psid) } == 0 || psid.is_null() {
        return None;
    }
    // SAFETY: `psid` is a valid SID from the call above.
    let len = unsafe { windows_sys::Win32::Security::GetLengthSid(psid) } as usize;
    // SAFETY: `psid` points at `len` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(psid.cast::<u8>(), len) }.to_vec();
    // SAFETY: `psid` came from `ConvertStringSidToSidW`, which LocalAllocs.
    unsafe { LocalFree(psid.cast::<std::ffi::c_void>()) };
    Some(bytes)
}

/// Yield the trustee field of every ACE in an SDDL DACL body.
fn aces(body: &str) -> impl Iterator<Item = &str> {
    body.split('(').skip(1).filter_map(|ace| {
        let ace = ace.split(')').next()?;
        let fields: Vec<&str> = ace.split(';').collect();
        (fields.len() >= ACE_FIELDS).then(|| fields[ACE_FIELDS - 1])
    })
}

/// UTF-16, NUL-terminated — what every `*W` entry point below expects.
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// String SID of the user this process runs as.
fn current_user_sid() -> io::Result<String> {
    let mut token: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no
    // release; `token` is a valid out-pointer we close below on every path.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = token_user_sid(token);
    // SAFETY: `token` was opened above and is closed exactly once here.
    unsafe { windows_sys::Win32::Foundation::CloseHandle(token) };
    result
}

fn token_user_sid(token: windows_sys::Win32::Foundation::HANDLE) -> io::Result<String> {
    let mut needed: u32 = 0;
    // SAFETY: the first call is the documented size probe — a null buffer with
    // zero length is expected to fail with ERROR_INSUFFICIENT_BUFFER and fill
    // `needed`.
    unsafe {
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0u8; needed as usize];
    // SAFETY: `buffer` is at least `needed` bytes and outlives the call.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success Windows wrote a TOKEN_USER at the head of `buffer`,
    // whose `Sid` points into that same allocation and stays valid while
    // `buffer` lives.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };

    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sid` is the live SID above; `text` is a valid out-pointer that
    // Windows fills with a LocalAlloc'd string we free below.
    let ok = unsafe { ConvertSidToStringSidW(sid, &mut text) };
    if ok == 0 || text.is_null() {
        return Err(io::Error::last_os_error());
    }
    let sid_string = wide_to_string(text);
    // SAFETY: `text` came from ConvertSidToStringSidW, whose documented
    // release is LocalFree, and is freed exactly once.
    unsafe { LocalFree(text as HLOCAL) };
    Ok(sid_string)
}

/// Read `path`'s DACL back as SDDL. This is the honest observation the
/// tightening decision and the tests are both made from — never the constant
/// we passed in.
pub(super) fn dacl_sddl(path: &Path) -> io::Result<String> {
    let wide_path = wide(path);
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();

    // SAFETY: `wide_path` is NUL-terminated and outlives the call; all
    // out-pointers are valid. On success Windows allocates `descriptor` and we
    // release it below with the documented LocalFree.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }

    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: `descriptor` is the descriptor just returned and still live.
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || text.is_null() {
        let err = io::Error::last_os_error();
        // SAFETY: `descriptor` is live and freed exactly once on this path.
        unsafe { LocalFree(descriptor as HLOCAL) };
        return Err(err);
    }
    let sddl = wide_to_string(text);
    // SAFETY: both allocations came from Win32 LocalAlloc-family calls and are
    // each freed exactly once here.
    unsafe {
        LocalFree(text as HLOCAL);
        LocalFree(descriptor as HLOCAL);
    }
    Ok(sddl)
}

/// Install `sddl`'s DACL on `path`, protected from inheritance.
fn apply_dacl(path: &Path, sddl: &str) -> io::Result<()> {
    let wide_sddl: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `wide_sddl` is NUL-terminated and outlives the call; on success
    // Windows allocates `descriptor`, released below.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide_sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || descriptor.is_null() {
        return Err(io::Error::last_os_error());
    }

    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut present: i32 = 0;
    let mut defaulted: i32 = 0;
    // SAFETY: `descriptor` is the live descriptor above; `dacl` borrows from
    // it and stays valid until the LocalFree below.
    let ok =
        unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) };
    let result = if ok == 0 || present == 0 {
        Err(io::Error::last_os_error())
    } else {
        let mut wide_path = wide(path);
        // SAFETY: `wide_path` is a NUL-terminated mutable buffer and `dacl`
        // points into the descriptor that is still live for this call. The
        // DACL is copied into the object's security descriptor by the kernel,
        // so releasing ours afterwards is sound.
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide_path.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    };

    // SAFETY: `descriptor` came from the SDDL conversion above and is freed
    // exactly once, after the last use of the `dacl` pointer into it.
    unsafe { LocalFree(descriptor as HLOCAL) };
    result
}

/// Copy a NUL-terminated Windows string out of a Win32 allocation.
fn wide_to_string(text: *const u16) -> String {
    let mut len = 0usize;
    // SAFETY: `text` is a NUL-terminated buffer produced by Win32; the walk
    // stops at that terminator and never reads past it.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units precede the terminator found above.
    let slice = unsafe { std::slice::from_raw_parts(text, len) };
    String::from_utf16_lossy(slice)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1172 F1e: the deploy directory holds a binary the CLI executes, so
    /// anything that can write it chooses what runs as the daemon. This drives
    /// the real code path and reads the DACL back off the created directory,
    /// so it fails against the pre-fix no-op rather than restating a constant.
    #[test]
    fn a_users_writable_deploy_dir_is_tightened_to_owner_only() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("deploy");
        std::fs::create_dir(&dir).unwrap();

        // Give BUILTIN\Users modify, inherited-style — the grant a cache root
        // under C:\ or C:\ProgramData picks up, and the one that makes this a
        // finding rather than a hardening nicety.
        let user_sid = current_user_sid().unwrap();
        apply_dacl(
            &dir,
            &format!("D:(A;OICI;FA;;;{user_sid})(A;OICI;0x1301bf;;;BU)"),
        )
        .unwrap();
        let before = dacl_sddl(&dir).unwrap();
        println!("DACL before: {before}");
        assert!(
            !is_owner_only(&before, &user_sid),
            "the test fixture must start exposed, got {before}"
        );

        let changed = super::ensure_dir_private(&dir).unwrap();

        let after = dacl_sddl(&dir).unwrap();
        println!("DACL after:  {after}");
        assert!(changed, "an exposed dir must report that it was repaired");
        assert!(
            is_owner_only(&after, &user_sid),
            "the repair must leave the deploy dir owner-only, got {after}"
        );
        assert!(
            !after.contains(";BU)"),
            "BUILTIN\\Users must not retain write access: {after}"
        );

        // The point of the DACL is that the CLI can still deploy into it.
        std::fs::write(dir.join("zccache-daemon.exe"), b"probe").unwrap();
    }

    /// Idempotence matters: this runs on every daemon spawn, and a directory
    /// that reported "tightened" forever would emit a security lifecycle event
    /// on every compile.
    #[test]
    fn an_already_private_dir_is_left_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("deploy");
        std::fs::create_dir(&dir).unwrap();

        super::ensure_dir_private(&dir).unwrap();
        let second = super::ensure_dir_private(&dir).unwrap();

        assert!(
            !second,
            "a dir already tightened must need no repair the second time"
        );
    }

    /// A missing directory is an error, not a silent pass — the caller is
    /// about to copy a binary into it.
    #[test]
    fn ensuring_a_missing_dir_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        super::ensure_dir_private(&temp.path().join("nope"))
            .expect_err("a missing deploy directory must not report success");
    }

    /// The SDDL we write must be accepted by Windows and must satisfy our own
    /// predicate — a typo would otherwise loop forever between "not private"
    /// and a set that never converges.
    #[test]
    fn the_owner_only_sddl_round_trips() {
        let user_sid = current_user_sid().unwrap();
        assert!(is_owner_only(&owner_only_sddl(&user_sid), &user_sid));
    }

    /// An unprotected DACL is one `icacls` reset away from re-inheriting
    /// `BUILTIN\Users`, so it must not be accepted as private.
    #[test]
    fn an_unprotected_dacl_is_not_owner_only() {
        assert!(!is_owner_only("D:AI(A;OICIID;FA;;;S-1-5-18)", "S-1-5-18"));
        assert!(!is_owner_only("D:NO_ACCESS_CONTROL", "S-1-5-18"));
    }

    /// The alias case CI caught and this host could not.
    ///
    /// `ConvertSecurityDescriptorToStringSecurityDescriptorW` renders
    /// well-known SIDs as two-letter aliases. A process running as the
    /// built-in Administrator gets its *own* account back as `LA`, so a raw
    /// string comparison declared the directory exposed immediately after
    /// tightening it — and the read-back check then rejected a DACL that was
    /// actually correct. The GitHub Windows runner runs as that account; my
    /// dev host does not, which is exactly why this needs a test rather than a
    /// manual probe.
    #[test]
    fn a_trustee_alias_matches_the_same_sid_written_longhand() {
        // SYSTEM, both spellings, in both argument positions.
        assert!(trustee_is_self_or_admin("SY", "S-1-5-18"));
        assert!(trustee_is_self_or_admin("S-1-5-18", "SY"));

        // The shape CI hit: the ACE for the running user comes back aliased
        // while the SID we compare against is spelled out longhand.
        assert!(
            is_owner_only("D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;S-1-5-18)", "S-1-5-18"),
            "an aliased trustee must resolve to its SID, not be read as a stranger"
        );

        // `LA` is the built-in Administrator *account*, not SYSTEM — a
        // different principal, and it must not be waved through just because
        // it happens to be an alias. (Getting this backwards is what made the
        // first version of this test fail.)
        assert!(!trustee_is_self_or_admin("LA", "S-1-5-18"));

        // And the comparison must still reject an actual stranger.
        assert!(!trustee_is_self_or_admin("WD", "S-1-5-18"));
        assert!(
            !is_owner_only("D:PAI(A;OICI;FA;;;WD)(A;OICI;FA;;;SY)", "S-1-5-18"),
            "Everyone must not be accepted just because SYSTEM is also listed"
        );
    }
}
