//! Owner-only security descriptor for local named pipes.

use std::ffi::c_void;
use std::io;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

const OWNER_ONLY_SDDL: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)";
const SDDL_REVISION_1: u32 = 1;

#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut c_void,
    inherit_handle: i32,
}

struct OwnerOnlySecurity {
    attributes: SecurityAttributes,
    descriptor: *mut c_void,
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
}

impl OwnerOnlySecurity {
    fn new() -> io::Result<Self> {
        let wide: Vec<u16> = OWNER_ONLY_SDDL
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated and `descriptor` is a valid out pointer.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || descriptor.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            attributes: SecurityAttributes {
                length: size_of::<SecurityAttributes>() as u32,
                descriptor,
                inherit_handle: 0,
            },
            descriptor,
        })
    }

    fn as_ptr(&self) -> *mut c_void {
        std::ptr::from_ref(&self.attributes).cast_mut().cast()
    }
}

impl Drop for OwnerOnlySecurity {
    fn drop(&mut self) {
        // SAFETY: this allocation came from the matching Windows conversion API.
        unsafe { LocalFree(self.descriptor) };
    }
}

pub(super) fn create(endpoint: &str, first: bool) -> io::Result<NamedPipeServer> {
    let security = OwnerOnlySecurity::new()?;
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    // SAFETY: the descriptor remains live for creation and Windows copies it.
    unsafe { options.create_with_security_attributes_raw(endpoint, security.as_ptr()) }
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::AsRawHandle;

    use super::*;

    const SE_KERNEL_OBJECT: i32 = 6;
    const DACL_SECURITY_INFORMATION: u32 = 4;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn GetSecurityInfo(
            handle: isize,
            object_type: i32,
            security_info: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            descriptor: *mut *mut c_void,
        ) -> u32;
        fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor: *mut c_void,
            revision: u32,
            security_info: u32,
            text: *mut *mut u16,
            length: *mut u32,
        ) -> i32;
    }

    fn dacl_sddl(handle: isize) -> String {
        let mut descriptor = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        // SAFETY: the handle remains live and all output pointers are valid.
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

        let mut text = std::ptr::null_mut();
        let mut length = 0;
        // SAFETY: `descriptor` is live and both output pointers are valid.
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                &mut length,
            )
        };
        assert_ne!(ok, 0, "security descriptor string conversion failed");
        // SAFETY: Windows returned a UTF-16 allocation containing `length` units.
        let sddl = unsafe {
            String::from_utf16_lossy(std::slice::from_raw_parts(text, length as usize))
        };
        // SAFETY: both buffers are Win32 local allocations, each released once.
        unsafe {
            LocalFree(text.cast());
            LocalFree(descriptor);
        }
        sddl
    }

    #[tokio::test]
    async fn bound_pipe_excludes_everyone_and_anonymous() {
        let endpoint = super::super::Endpoint::unique_test("dacl");
        let pipe = create(endpoint.as_str(), true).expect("secure pipe create");
        let sddl = dacl_sddl(pipe.as_raw_handle() as isize);
        assert!(!sddl.contains(";WD)"), "Everyone present in DACL: {sddl}");
        assert!(!sddl.contains(";AN)"), "Anonymous present in DACL: {sddl}");
    }

    #[test]
    fn owner_only_descriptor_builds() {
        OwnerOnlySecurity::new().expect("OWNER_ONLY_SDDL must parse");
    }
}
