//! Descriptor-based executable inspection and approval checks.
use std::{fs::File, io::{self, Read}, os::unix::{fs::{MetadataExt, OpenOptionsExt}, io::AsRawFd}, path::Path};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity(String);
impl Identity {
    fn of(file: &File) -> io::Result<Self> {
        let m = file.metadata()?;
        Ok(Self(format!("{}:{}:{}:{}:{}:{}:{}:{}", m.dev(), m.ino(), m.size(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec(), m.mode())))
    }
    pub fn encode(&self) -> &str { &self.0 }
}

fn open(path: &Path) -> io::Result<File> {
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path)?;
    if !file.metadata()?.is_file() { return Err(io::Error::new(io::ErrorKind::IsADirectory, "Not a regular file")); }
    if unsafe { libc::faccessat(file.as_raw_fd(), c"".as_ptr(), libc::X_OK, libc::AT_EMPTY_PATH | libc::AT_EACCESS) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

/// Nonblocking open prevents a replaced FIFO from hanging the inspector.
pub fn inspect(path: &Path) -> io::Result<Option<Identity>> {
    let mut file = open(path)?;
    let before = Identity::of(&file)?;
    let mut magic = [0; 4];
    let n = file.read(&mut magic)?;
    if Identity::of(&file)? != before { return Err(io::Error::other("File changed during inspection")); }
    Ok((magic[..n].starts_with(b"\x7fELF") || magic[..n].starts_with(b"#!")).then_some(before))
}

/// Runs inside the terminal. No GUI or elevated privileges. Opening and executing
/// the same descriptor prevents a rename/symlink swap after the identity check.
#[cfg(test)]
pub fn execute(path: &Path, expected: &str) -> io::Result<()> {
    execute_notified(path, expected, None)
}

pub fn execute_notified(path: &Path, expected: &str, status: Option<&mut std::os::unix::net::UnixStream>) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let file = open(path)?;
    if Identity::of(&file)?.encode() != expected {
        return Err(io::Error::other("Program changed since confirmation. Search for it and confirm again."));
    }
    let fd = file.as_raw_fd();
    // Scripts need the descriptor to remain accessible to their interpreter.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } < 0 { return Err(io::Error::last_os_error()); }
    let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let args = [name.as_ptr(), std::ptr::null()];
    let env: Vec<_> = std::env::vars_os().map(|(k,v)| {
        let mut bytes = k.as_bytes().to_vec(); bytes.push(b'='); bytes.extend_from_slice(v.as_bytes());
        std::ffi::CString::new(bytes)
    }).collect::<Result<_,_>>()?;
    let mut envp: Vec<_> = env.iter().map(|s| s.as_ptr()).collect(); envp.push(std::ptr::null());
    if Identity::of(&file)?.encode() != expected {
        return Err(io::Error::other("Program changed before execution. Confirm again."));
    }
    if let Some(status) = status {
        use std::io::Write;
        status.write_all(b"ready\n")?;
    }
    // SAFETY: fd is live; argv/envp contain NUL-terminated strings and a null sentinel.
    unsafe { libc::fexecve(fd, args.as_ptr(), envp.as_ptr()); }
    Err(io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn dir() -> std::path::PathBuf { crate::util::private_test_dir("omni-program") }
    #[test]
    fn receipt_accepts_exec_and_rejects_failure() {
        use std::io::Write;
        for (message, success) in [("ready\n", true), ("ready\nexec failed", false), ("file changed", false)] {
            let receipt = Receipt::new().unwrap();
            assert_eq!(std::fs::metadata(&receipt.dir).unwrap().mode() & 0o777, 0o700);
            let mut client = std::os::unix::net::UnixStream::connect(receipt.path()).unwrap();
            client.write_all(message.as_bytes()).unwrap(); drop(client);
            assert_eq!(receipt.wait().is_ok(), success);
        }
    }

    #[test]
    fn rejects_fifo_without_waiting() {
        use std::os::unix::ffi::OsStrExt;
        let dir = dir(); let p = dir.join("fifo"); let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o700) }, 0);
        assert!(inspect(&p).is_err()); std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn refuses_replacement_and_in_place_change() {
        let dir = dir(); let p = dir.join("program");
        std::fs::write(&p, b"#!/bin/sh\nexit 0\n").unwrap(); std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = inspect(&p).unwrap().unwrap();
        std::fs::write(&p, b"#!/bin/sh\nexit 123\n").unwrap();
        assert!(execute(&p, id.encode()).unwrap_err().to_string().contains("changed"));
        let other = dir.join("other"); std::fs::copy(&p, &other).unwrap(); std::fs::rename(other, &p).unwrap();
        assert!(execute(&p, id.encode()).unwrap_err().to_string().contains("changed"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// A private per-launch socket acknowledges exec, rather than terminal process exit.
pub struct Receipt {
    dir: std::path::PathBuf,
    listener: std::os::unix::net::UnixListener,
}
impl Receipt {
    pub fn new() -> io::Result<Self> {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let template = std::env::temp_dir().join("omni-launch-XXXXXX");
        let mut bytes = std::ffi::CString::new(template.as_os_str().as_bytes())?.into_bytes_with_nul();
        if unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) }.is_null() { return Err(io::Error::last_os_error()); }
        bytes.pop();
        let dir = std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes));
        let listener = match std::os::unix::net::UnixListener::bind(dir.join("status")) {
            Ok(listener) => listener,
            Err(e) => { let _ = std::fs::remove_dir(&dir); return Err(e); }
        };
        listener.set_nonblocking(true)?;
        Ok(Self { dir, listener })
    }
    pub fn path(&self) -> std::path::PathBuf { self.dir.join("status") }
    pub fn wait(&self) -> io::Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(std::time::Duration::from_secs(15)))?;
                    let mut message = String::new();
                    (&mut stream).take(4096).read_to_string(&mut message)?;
                    return if message == "ready\n" { Ok(()) } else { Err(io::Error::other(message.trim().strip_prefix("ready\n").unwrap_or(message.trim()).to_owned())) };
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Err(io::Error::new(io::ErrorKind::TimedOut, "The terminal did not acknowledge program startup")),
                Err(e) => return Err(e),
            }
        }
    }
}
impl Drop for Receipt {
    fn drop(&mut self) { let _ = std::fs::remove_file(self.path()); let _ = std::fs::remove_dir(&self.dir); }
}
