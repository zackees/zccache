use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub fn is_supported() -> bool {
    true
}

#[derive(Debug)]
pub struct NativeJobserver {
    read: OwnedFd,
    write: OwnedFd,
}

impl NativeJobserver {
    pub fn create(capacity: usize) -> std::io::Result<Self> {
        validate_capacity(capacity)?;
        let mut fds = [0_i32; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        prime(&write, capacity)?;
        Ok(Self { read, write })
    }

    pub fn auth_string(&self) -> String {
        format!("{},{}", self.read.as_raw_fd(), self.write.as_raw_fd())
    }
}

fn validate_capacity(capacity: usize) -> std::io::Result<()> {
    if capacity == 0 {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "jobserver capacity must be greater than zero",
        ))
    } else {
        Ok(())
    }
}

fn prime(write: &OwnedFd, capacity: usize) -> std::io::Result<()> {
    let tokens = vec![b'+'; capacity];
    let written = unsafe { libc::write(write.as_raw_fd(), tokens.as_ptr().cast(), tokens.len()) };
    if written < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if written as usize != tokens.len() {
        return Err(std::io::Error::other(format!(
            "jobserver pipe priming wrote {written} of {} bytes",
            tokens.len()
        )));
    }
    Ok(())
}
