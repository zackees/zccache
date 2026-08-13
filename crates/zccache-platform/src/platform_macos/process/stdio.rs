//! macOS standard-I/O mechanics.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub fn detach() {
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null < 0 {
            return;
        }
        for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            let _ = libc::dup2(null, target);
        }
        if null > libc::STDERR_FILENO {
            let _ = libc::close(null);
        }
    }
}

pub fn redirect_to_log(path: &Path) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        if null < 0 {
            return false;
        }
        let _ = libc::dup2(null, libc::STDIN_FILENO);
        if null > libc::STDERR_FILENO {
            let _ = libc::close(null);
        }
        let log = libc::open(
            path.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
            0o644,
        );
        if log < 0 {
            return false;
        }
        let _ = libc::dup2(log, libc::STDOUT_FILENO);
        let _ = libc::dup2(log, libc::STDERR_FILENO);
        if log > libc::STDERR_FILENO {
            let _ = libc::close(log);
        }
    }
    true
}
