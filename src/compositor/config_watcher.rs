use std::ffi::{CString, OsStr, OsString};
use std::mem;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// A watched directory (inotify watch descriptor) and the file name in it
/// whose writes mean "the config changed".
pub type ConfigWatchTarget = (libc::c_int, OsString);

/// Create an inotify instance watching the parent directory of each of `paths`
/// (the config and the tray's `state.conf` overlay).
///
/// Returns the inotify `OwnedFd` and the (directory watch, filename) pairs to
/// match against. Watching the directory (rather than the file itself) handles
/// atomic-rename saves from editors like Vim, which write a temp file then
/// rename it into place, triggering `IN_MOVED_TO` rather than `IN_CLOSE_WRITE`.
///
/// When the config is a symlink (dotfile managers like stow or chezmoi), edits
/// land in the target's directory, so that directory is watched too.
pub fn make_config_watch_fd(
    paths: &[&Path],
) -> Result<(OwnedFd, Vec<ConfigWatchTarget>), std::io::Error> {
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut targets = Vec::new();
    for path in paths {
        targets.push(add_dir_watch(fd, path)?);
        if let Ok(resolved) = std::fs::canonicalize(path)
            && resolved.parent() != path.parent()
        {
            match add_dir_watch(fd, &resolved) {
                Ok(target) => targets.push(target),
                Err(error) => tracing::warn!(
                    "Failed to watch symlinked config target {}: {}",
                    resolved.display(),
                    error
                ),
            }
        }
    }

    Ok((owned_fd, targets))
}

/// Watch the directory holding `path` for writes/renames; returns the watch
/// descriptor paired with `path`'s file name.
fn add_dir_watch(fd: libc::c_int, path: &Path) -> Result<ConfigWatchTarget, std::io::Error> {
    let invalid =
        |message: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, message.to_string());
    let dir = path
        .parent()
        .ok_or_else(|| invalid("config path has no parent directory"))?;
    let filename = path
        .file_name()
        .ok_or_else(|| invalid("config path has no filename"))?
        .to_os_string();
    let dir_cstr = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| invalid("config directory path contains a null byte"))?;

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
    Ok((wd, filename))
}

/// Drain all pending inotify events from `fd`.
///
/// Returns `true` if any event matches one of `targets` (same watched directory
/// and file name), meaning the config file was written or atomically replaced. All events are consumed regardless
/// so rapid successive saves don't queue up multiple reloads.
pub fn drain_config_event(fd: BorrowedFd<'_>, targets: &[ConfigWatchTarget]) -> bool {
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
                if targets
                    .iter()
                    .any(|(wd, filename)| *wd == event.wd && name == filename.as_os_str())
                {
                    found = true;
                }
            }

            offset += event_size;
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::{drain_config_event, make_config_watch_fd};
    use std::os::fd::AsFd;

    #[test]
    fn edits_to_a_symlinked_config_target_are_seen() {
        let root = std::env::temp_dir().join(format!("beewm-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let config_dir = root.join("config");
        let dotfiles = root.join("dotfiles");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&dotfiles).unwrap();
        let target = dotfiles.join("beewm.conf");
        std::fs::write(&target, "gap 4\n").unwrap();
        let link = config_dir.join("config");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let (fd, targets) = make_config_watch_fd(&[&link]).unwrap();
        assert!(!drain_config_event(fd.as_fd(), &targets));

        std::fs::write(&target, "gap 8\n").unwrap();
        assert!(drain_config_event(fd.as_fd(), &targets));

        // An unrelated file in the target directory is not a config change.
        std::fs::write(dotfiles.join("other"), "x").unwrap();
        assert!(!drain_config_event(fd.as_fd(), &targets));

        let _ = std::fs::remove_dir_all(&root);
    }
}
