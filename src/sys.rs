//! The platform layer: opening a file for Direct I/O, flushing it to stable
//! storage, and positioned reads and writes.
//!
//! Direct I/O is a different request on every platform — `O_DIRECT` on Linux,
//! `F_NOCACHE` on macOS, `FILE_FLAG_NO_BUFFERING` on Windows — and so is a
//! durable flush. Each is handled here behind one small, uniform interface so
//! the rest of the crate is platform-agnostic. On targets outside the supported
//! set (Linux, macOS, Windows) the cache-bypass hint is simply not applied;
//! correctness and durability do not depend on it, only the page-cache pressure
//! does.

use std::fs::File;
use std::io;
use std::path::Path;

/// Open `path` read-write for paged I/O.
///
/// When `direct` is set, the OS page cache is bypassed (Direct I/O). When
/// `create` is set, the file is created if it does not exist.
pub(crate) fn open(path: &Path, direct: bool, create: bool) -> io::Result<File> {
    imp::open(path, direct, create)
}

/// Flush all written data to stable storage.
///
/// This is the call that actually makes a write durable, and it is not the same
/// everywhere: Linux uses `fdatasync`, Windows `FlushFileBuffers`, and macOS
/// `F_FULLFSYNC` (a plain `fsync` on macOS leaves data in the drive's own write
/// cache).
pub(crate) fn sync_data(file: &File) -> io::Result<()> {
    imp::sync_data(file)
}

/// Read into `buf` starting at byte `offset`, returning the number of bytes
/// read. A return value short of `buf.len()` means end of file.
pub(crate) fn read_at_full(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut read = 0;
    while read < buf.len() {
        match pread(file, &mut buf[read..], offset + read as u64) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(read)
}

/// Write all of `buf` starting at byte `offset`.
pub(crate) fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    let mut written = 0;
    while written < buf.len() {
        match pwrite(file, &buf[written..], offset + written as u64) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "positioned write returned zero",
                ));
            }
            Ok(n) => written += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
#[inline]
fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(unix)]
#[inline]
fn pwrite(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

#[cfg(windows)]
#[inline]
fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

#[cfg(windows)]
#[inline]
fn pwrite(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    pub(super) fn open(path: &Path, direct: bool, create: bool) -> io::Result<File> {
        let mut opts = OpenOptions::new();
        let _ = opts.read(true).write(true).create(create);
        if direct {
            let _ = opts.custom_flags(libc::O_DIRECT);
        }
        opts.open(path)
    }

    pub(super) fn sync_data(file: &File) -> io::Result<()> {
        file.sync_data()
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    pub(super) fn open(path: &Path, direct: bool, create: bool) -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(path)?;
        if direct {
            let fd = file.as_raw_fd();
            // SAFETY: `fd` is a valid descriptor owned by `file` for the
            // duration of this call. `F_NOCACHE` takes an int arg and returns
            // -1 on error, which we convert to the last OS error.
            let rc = unsafe { libc::fcntl(fd, libc::F_NOCACHE, 1) };
            if rc == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(file)
    }

    pub(super) fn sync_data(file: &File) -> io::Result<()> {
        let fd = file.as_raw_fd();
        // SAFETY: `fd` is a valid descriptor owned by `file`. `F_FULLFSYNC`
        // takes no argument and returns -1 on error; it flushes the drive's
        // write cache, the only durable barrier on macOS.
        let rc = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;

    // From <winbase.h>. `NO_BUFFERING` is Direct I/O; `WRITE_THROUGH` pairs with
    // it so a write reaches the device, and `FlushFileBuffers` (via `sync_data`)
    // flushes the device cache.
    const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;

    pub(super) fn open(path: &Path, direct: bool, create: bool) -> io::Result<File> {
        let mut opts = OpenOptions::new();
        let _ = opts.read(true).write(true).create(create);
        if direct {
            let _ = opts.custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH);
        }
        opts.open(path)
    }

    pub(super) fn sync_data(file: &File) -> io::Result<()> {
        file.sync_data()
    }
}

#[cfg(all(unix, not(target_os = "linux"), not(target_os = "macos")))]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::Path;

    // Outside the supported Unix targets the cache-bypass hint is not applied:
    // there is no portable `O_DIRECT` equivalent to honor. Durability still
    // comes from `sync_data`; only page-cache behavior differs.
    pub(super) fn open(path: &Path, _direct: bool, create: bool) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(path)
    }

    pub(super) fn sync_data(file: &File) -> io::Result<()> {
        file.sync_data()
    }
}
