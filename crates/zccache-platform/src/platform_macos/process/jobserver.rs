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
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        set_close_on_exec(&read)?;
        set_close_on_exec(&write)?;
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

fn set_close_on_exec(fd: &OwnedFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    if flags == -1
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
    {
        Err(std::io::Error::last_os_error())
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
