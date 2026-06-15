use std::ffi::{CString, OsStr, OsString};
use std::mem;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Create an inotify instance watching the parent directory of `config_path`.
///
/// Returns the inotify `OwnedFd` and the bare filename to match against.
/// Watching the directory (rather than the file itself) handles atomic-rename
/// saves from editors like Vim, which write a temp file then rename it into
/// place — triggering `IN_MOVED_TO` rather than `IN_CLOSE_WRITE`.
pub fn make_config_watch_fd(config_path: &Path) -> Result<(OwnedFd, OsString), std::io::Error> {
    let dir = config_path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "config path has no parent directory",
        )
    })?;
    let filename = config_path
        .file_name()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "config path has no filename",
            )
        })?
        .to_os_string();

    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };

    let dir_cstr = CString::new(dir.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "config directory path contains a null byte",
        )
    })?;

    let wd = unsafe {
        libc::inotify_add_watch(
            fd,
            dir_cstr.as_ptr(),
            libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO,
        )
    };
    if wd == -1 {
        return Err(std::io::Error::last_os_error());
    }

    Ok((owned_fd, filename))
}

/// Drain all pending inotify events from `fd`.
///
/// Returns `true` if any event matches `config_filename`, meaning the config
/// file was written or atomically replaced. All events are consumed regardless
/// so rapid successive saves don't queue up multiple reloads.
pub fn drain_config_event(fd: BorrowedFd<'_>, config_filename: &OsStr) -> bool {
    // Aligned buffer: inotify_event requires 4-byte alignment.
    #[repr(align(4))]
    struct AlignedBuf([u8; 4096]);
    let mut buf = AlignedBuf([0u8; 4096]);

    let mut found = false;

    loop {
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.0.as_mut_ptr().cast(), buf.0.len()) };

        if n <= 0 {
            // EAGAIN / EOF: no more events right now.
            break;
        }

        let n = n as usize;
        let mut offset = 0;

        while offset + mem::size_of::<libc::inotify_event>() <= n {
            let event = unsafe { &*(buf.0.as_ptr().add(offset) as *const libc::inotify_event) };
            let name_len = event.len as usize;
            let event_size = mem::size_of::<libc::inotify_event>() + name_len;

            // Guard against a malformed event with zero size to avoid a loop.
            if event_size == 0 {
                break;
            }

            if event.mask & (libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO) != 0
                && name_len > 0
                && offset + event_size <= n
            {
                let name_ptr = unsafe {
                    buf.0
                        .as_ptr()
                        .add(offset + mem::size_of::<libc::inotify_event>())
                };
                let name_bytes = unsafe { std::slice::from_raw_parts(name_ptr, name_len) };
                // The name field is null-terminated and may include padding bytes.
                let name_end = name_bytes.iter().position(|&b| b == 0).unwrap_or(name_len);
                let name = OsStr::from_bytes(&name_bytes[..name_end]);
                if name == config_filename {
                    found = true;
                }
            }

            offset += event_size;
        }
    }

    found
}
