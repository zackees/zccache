//! Owner-only security descriptor for the Windows daemon pipe (#1272).
//!
//! `ServerOptions::create` passes null `SECURITY_ATTRIBUTES`, so the pipe
//! inherits the platform default descriptor. Probed on Windows 10 (19045):
//!
//! ```text
//! D:(A;;FR;;;WD)(A;;FR;;;AN)(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;S-1-5-21-…-1001)
//! ```
//!
//! **Everyone** (`WD`) and **ANONYMOUS LOGON** (`AN`) get `FR` —
//! `FILE_GENERIC_READ`. That is read, not write, so it does not grant the
//! cross-user request injection #1171 describes on unix: sending a `Compile`
//! or `GenericToolExec` needs write access. What it does grant is *reading the
//! traffic*, and every compile request carries `args`, `env` and `cwd` — build
//! paths, environment variables, and any secret passed to a compile through
//! the environment, readable by any other local account.
//!
//! The replacement is `D:P(A;;GA;;;OW)(A;;GA;;;SY)`: full control for the
//! object owner and SYSTEM, nobody else. The `P` (PROTECTED) flag is
//! load-bearing — without it, inheritance can re-add the `WD`/`AN` aces this
//! exists to remove.
//!
//! `BUILTIN\Administrators` losing its entry is not a regression worth
//! worrying about: an administrator can take ownership of the object anyway,
//! so the ACE bought nothing that an attacker at that privilege level lacked.
//!
//! FFI is declared inline rather than pulling `windows-sys` into this crate —
//! the same house pattern used elsewhere for small Win32 surfaces, and it
//! keeps the dependency graph unchanged.

use std::ffi::c_void;

/// `SECURITY_ATTRIBUTES` plus the descriptor allocation it points at.
///
/// The descriptor must outlive every `CreateNamedPipe` call that references
/// it, so the two are owned together and freed together. Dropping this while
/// a pipe creation is still in flight would hand the kernel a dangling
/// pointer, which is why it is never constructed as a temporary.
pub(super) struct OwnerOnlySecurity {
    attributes: SecurityAttributes,
    descriptor: *mut c_void,
}

#[repr(C)]
struct SecurityAttributes {
    n_length: u32,
    lp_security_descriptor: *mut c_void,
    b_inherit_handle: i32,
}

/// Full control for the object owner (`OW`) and SYSTEM (`SY`), protected from
/// inheritance so nothing can re-add Everyone or ANONYMOUS LOGON.
const OWNER_ONLY_SDDL: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)";

const SDDL_REVISION_1: u32 = 1;

extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string_security_descriptor: *const u16,
        string_sd_revision: u32,
        security_descriptor: *mut *mut c_void,
        security_descriptor_size: *mut u32,
    ) -> i32;
    fn LocalFree(mem: *mut c_void) -> *mut c_void;
}

impl OwnerOnlySecurity {
    /// Build the descriptor. Returns `None` if Windows rejects the SDDL, in
    /// which case the caller falls back to the platform default rather than
    /// failing to start — a daemon that runs with a readable pipe is worse
    /// than one that runs with a private pipe, but far better than one that
    /// refuses to serve at all.
    pub(super) fn new() -> Option<Self> {
        let wide: Vec<u16> = OWNER_ONLY_SDDL
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut descriptor: *mut c_void = std::ptr::null_mut();

        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the
        // call. `descriptor` is a valid out-pointer. On success Windows
        // allocates the descriptor with LocalAlloc and we own it; `Drop`
        // releases it with the matching LocalFree.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || descriptor.is_null() {
            return None;
        }

        Some(Self {
            attributes: SecurityAttributes {
                n_length: std::mem::size_of::<SecurityAttributes>() as u32,
                lp_security_descriptor: descriptor,
                b_inherit_handle: 0,
            },
            descriptor,
        })
    }

    /// Pointer to pass to `create_with_security_attributes_raw`.
    ///
    /// Borrows `self`, so the descriptor cannot be dropped while the returned
    /// pointer is still in use.
    pub(super) fn as_ptr(&self) -> *mut c_void {
        std::ptr::addr_of!(self.attributes) as *mut c_void
    }
}

impl Drop for OwnerOnlySecurity {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            // SAFETY: `descriptor` came from
            // `ConvertStringSecurityDescriptorToSecurityDescriptorW`, which
            // documents LocalFree as the matching release, and is freed
            // exactly once because `Drop` runs once and we null nothing else.
            unsafe { LocalFree(self.descriptor) };
        }
    }
}

/// Create one pipe instance carrying the owner-only descriptor (#1272).
///
/// Falls back to `ServerOptions::create` if the descriptor cannot be built.
/// A daemon that starts with the platform-default pipe is a confidentiality
/// regression; a daemon that refuses to start is an outage. The former is
/// recoverable and visible in the DACL, so it is the better failure.
pub(super) fn create_pipe_instance(
    endpoint: &str,
    first: bool,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let mut options = ServerOptions::new();
    options.first_pipe_instance(first);

    let Some(security) = OwnerOnlySecurity::new() else {
        tracing::warn!(
            "could not build the owner-only pipe descriptor; falling back to the \
             platform default, which grants Everyone and ANONYMOUS LOGON read \
             access to request traffic"
        );
        return options.create(endpoint);
    };

    // SAFETY: `security` lives until the end of this function, so the pointer
    // handed to Windows is valid for the whole `create_with_security_attributes_raw`
    // call, which is the only thing that dereferences it. The descriptor is
    // copied into the kernel object during creation and is not referenced
    // afterwards, so releasing it on drop is sound.
    let created =
        unsafe { options.create_with_security_attributes_raw(endpoint, security.as_ptr()) };
    drop(security);
    created
}

#[cfg(test)]
mod tests {
    use super::*;

    const SE_KERNEL_OBJECT: i32 = 6;
    const DACL_SECURITY_INFORMATION: u32 = 4;

    extern "system" {
        fn GetSecurityInfo(
            handle: isize,
            object_type: i32,
            security_info: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            security_descriptor: *mut *mut c_void,
        ) -> u32;
        fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
            security_descriptor: *mut c_void,
            request_revision: u32,
            security_information: u32,
            string_security_descriptor: *mut *mut u16,
            string_len: *mut u32,
        ) -> i32;
    }

    /// Read the live DACL off a kernel object as SDDL.
    ///
    /// Reading it back from the bound handle — rather than asserting on what
    /// we passed in — is what makes this a test of the pipe's actual security
    /// rather than a test of our own constant.
    fn dacl_sddl(handle: isize) -> String {
        let mut descriptor: *mut c_void = std::ptr::null_mut();
        let mut dacl: *mut c_void = std::ptr::null_mut();

        // SAFETY: `handle` is a live pipe handle owned by the caller for the
        // duration of this call. All out-pointers are valid; on success
        // Windows allocates `descriptor` and we release it below.
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(status, 0, "GetSecurityInfo failed with {status}");

        let mut text: *mut u16 = std::ptr::null_mut();
        let mut len: u32 = 0;
        // SAFETY: `descriptor` is the descriptor just returned above, still
        // live. `text`/`len` are valid out-pointers.
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                &mut len,
            )
        };
        assert_ne!(ok, 0, "ConvertSecurityDescriptorToString failed");

        // SAFETY: `text` is a NUL-terminated UTF-16 string of `len` units,
        // allocated by Windows and valid until the LocalFree below.
        let sddl = unsafe {
            let slice = std::slice::from_raw_parts(text, len as usize);
            String::from_utf16_lossy(slice)
        };

        // SAFETY: both allocations came from Win32 LocalAlloc-family calls and
        // are each freed exactly once here.
        unsafe {
            LocalFree(text as *mut c_void);
            LocalFree(descriptor);
        }
        sddl
    }

    /// #1272: the default descriptor grants Everyone (`WD`) and ANONYMOUS
    /// LOGON (`AN`) `FR`, so any local account could read the pipe traffic —
    /// which carries `args`, `env` and `cwd` for every compile.
    ///
    /// This drives the real `IpcListener::bind` path and reads the DACL back
    /// off the bound handle, so it fails against the pre-fix code rather than
    /// merely restating the constant we pass in.
    // Tokio's named-pipe types register with the reactor at creation, so this
    // needs a runtime even though nothing here awaits.
    #[tokio::test]
    async fn a_bound_pipe_is_not_readable_by_everyone_or_anonymous() {
        use std::os::windows::io::AsRawHandle;

        let endpoint = crate::transport::unique_test_endpoint();
        let listener = crate::transport::IpcListener::bind(&endpoint).expect("bind");
        let handle = listener
            .inner
            .pool
            .front()
            .expect("bind leaves at least one pipe instance in the pool")
            .as_raw_handle() as isize;

        let sddl = dacl_sddl(handle);

        assert!(
            !sddl.contains(";WD)"),
            "Everyone must not appear in the pipe DACL: {sddl}"
        );
        assert!(
            !sddl.contains(";AN)"),
            "ANONYMOUS LOGON must not appear in the pipe DACL: {sddl}"
        );
    }

    /// The SDDL must actually parse — a typo would otherwise degrade silently
    /// to the platform default via the `None` fallback.
    #[test]
    fn the_owner_only_descriptor_builds() {
        assert!(
            OwnerOnlySecurity::new().is_some(),
            "OWNER_ONLY_SDDL must be accepted by Windows"
        );
    }
}
