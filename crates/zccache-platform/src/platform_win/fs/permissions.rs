//! Windows permission mechanics: owner-only DACLs for cache-owned
//! directories and read-only attribute handling.
//!
//! Ported from zccache-core's win_acl (#1172 F1e): `ensure_dir_private`
//! expressed "private" as unix mode bits and did nothing at all on Windows,
//! so directories the CLI populates and executes from were left with
//! whatever the parent tree inherits. This module applies an explicit
//! protected owner-only DACL — `D:P(A;OICI;FA;;;<user>)(A;OICI;FA;;;SY)` —
//! instead, with the same three-outcome contract as the Unix arm
//! (untouched / tightened / still-exposed error).

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
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
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
pub(crate) fn ensure_dir_private(path: &Path) -> io::Result<bool> {
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

/// Create `path` with the owner-only DACL already on it, creating any missing
/// parents the same way.
///
/// #1172 residual: `ensure_dir_private` can only tighten a directory that
/// already exists, so every caller had the shape "create with whatever the
/// parent hands down, then fix it". Between those two steps the directory is
/// live with the inherited ACL. Under `%USERPROFILE%` that inheritance is
/// already narrow and the window is harmless, but the relocated-root case this
/// module exists for (`ZCCACHE_CACHE_DIR` on `C:\ProgramData\…` or a volume
/// root) inherits `BUILTIN\Users:(OI)(CI)(M)` — and there another local user
/// can win the race and populate the directory the daemon binary is about to
/// be deployed into.
///
/// The unix arm never had this gap: `DirBuilder::mode(0o700)` passes the mode
/// to `mkdir(2)` itself, so no directory is ever briefly group-writable. This
/// is the Windows equivalent — the descriptor goes to `CreateDirectoryW` in
/// `SECURITY_ATTRIBUTES`, so the directory is never visible with any other
/// DACL.
///
/// Already-existing directories are left to `ensure_dir_private`: this only
/// closes the window for directories *it* creates. `Ok(())` when the path
/// exists as a directory already, so it composes like `create_dir_all`.
pub(crate) fn create_dir_all_private(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            create_dir_all_private(parent)?;
        }
    }
    let user_sid = current_user_sid()?;
    match create_dir_with_dacl(path, &owner_only_sddl(&user_sid)) {
        Ok(()) => Ok(()),
        // Lost a benign race with another zccache process creating the same
        // directory. The winner applied the same descriptor, so this is a
        // success, not a retry.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

/// `CreateDirectoryW` with `sddl` supplied at creation time.
fn create_dir_with_dacl(path: &Path, sddl: &str) -> io::Result<()> {
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

    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: descriptor.cast(),
        bInheritHandle: 0,
    };
    let wide_path = wide(path);
    // SAFETY: `wide_path` is NUL-terminated and `attributes` borrows the live
    // descriptor freed below. The kernel copies the descriptor into the new
    // object, so releasing ours afterwards is sound.
    let created = unsafe { CreateDirectoryW(wide_path.as_ptr(), &attributes) };
    let result = if created == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    };

    // SAFETY: `descriptor` came from the SDDL conversion above and is freed
    // exactly once, after the last use of `attributes`.
    unsafe { LocalFree(descriptor as HLOCAL) };
    result
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
pub(crate) fn is_owner_only(sddl: &str, user_sid: &str) -> bool {
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
pub(crate) fn dacl_sddl(path: &Path) -> io::Result<String> {
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

/// Sets or clears the read-only file attribute.
pub(crate) fn set_readonly(path: &Path, readonly: bool) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    if permissions.readonly() == readonly {
        return Ok(());
    }
    permissions.set_readonly(readonly);
    std::fs::set_permissions(path, permissions)
}

/// Clears the read-only attribute when set.
pub(crate) fn make_writable(path: &Path) -> std::io::Result<()> {
    if path.exists() && std::fs::metadata(path)?.permissions().readonly() {
        set_readonly(path, false)?;
    }
    Ok(())
}

/// Windows has no per-file executable bit; this is a no-op that keeps the
/// neutral facade portable.
pub(crate) fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// The host's mode representation on Windows is the `0`/`1` readonly
/// attribute.
pub(crate) fn mode(metadata: &std::fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}

/// Restores the readonly attribute previously read with [`mode`]; all other
/// permission bits are preserved.
pub(crate) fn apply_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    set_readonly(path, mode != 0)
}

#[cfg(test)]
mod attribute_tests {
    use super::*;

    #[test]
    fn readonly_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("f");
        std::fs::write(&file, b"data").unwrap();
        set_readonly(&file, true).unwrap();
        assert!(std::fs::metadata(&file).unwrap().permissions().readonly());
        make_writable(&file).unwrap();
        std::fs::write(&file, b"more").unwrap();
    }
}
