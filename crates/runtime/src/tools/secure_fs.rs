//! Descriptor-relative filesystem access for runtime tools.
//!
//! Scope classification alone cannot close a check/use race: an attacker can
//! replace an ancestor or leaf with a symlink after canonicalization. These
//! helpers anchor traversal at an already-authorized root directory and open
//! every component with `O_NOFOLLOW`, so the object acted on is the one reached
//! through the authorized directory handles.

#[cfg(unix)]
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use codypendent_daemon::policy::{PathScope, ScopeVerdict};

use super::ToolError;

pub(crate) struct ScopedFile {
    pub path: PathBuf,
    pub file: std::fs::File,
    pub created: bool,
    /// The authorized directory handle the leaf was opened through, kept open so
    /// a caller can act on the leaf's *directory entry* — creating a sibling,
    /// renaming over it — with the same anchoring the open had.
    /// [`replace_contents`] is the only such caller.
    #[cfg(unix)]
    dir: rustix::fd::OwnedFd,
    /// The leaf's own name within [`Self::dir`].
    #[cfg(unix)]
    leaf: OsString,
}

pub(crate) fn open_read(path: &Path, scope: &PathScope) -> Result<ScopedFile, ToolError> {
    open_scoped(path, scope, Access::Read, false)
}

pub(crate) fn open_edit(path: &Path, scope: &PathScope) -> Result<ScopedFile, ToolError> {
    open_scoped(path, scope, Access::ReadWrite, false)
}

pub(crate) fn open_write(path: &Path, scope: &PathScope) -> Result<ScopedFile, ToolError> {
    open_scoped(path, scope, Access::Write, true)
}

#[derive(Clone, Copy)]
enum Access {
    Read,
    Write,
    ReadWrite,
}

#[cfg(unix)]
fn open_scoped(
    path: &Path,
    scope: &PathScope,
    access: Access,
    create_parents: bool,
) -> Result<ScopedFile, ToolError> {
    use rustix::fs::{fcntl_getfl, fcntl_setfl, mkdirat, open, openat, Mode, OFlags};
    use rustix::io::Errno;

    let (resolved, verdict) = scope.resolve(path);
    match verdict {
        ScopeVerdict::Allowed => {}
        ScopeVerdict::Denied => return Err(ToolError::PathDenied(resolved)),
        ScopeVerdict::OutsideRoots => return Err(ToolError::PathOutOfScope(resolved)),
    }
    let root = scope
        .roots
        .iter()
        .filter(|root| resolved.starts_with(root))
        .max_by_key(|root| root.components().count())
        .ok_or_else(|| ToolError::PathOutOfScope(resolved.clone()))?;
    let relative = resolved
        .strip_prefix(root)
        .map_err(|_| ToolError::PathOutOfScope(resolved.clone()))?;
    let components = relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value.to_os_string()),
            _ => Err(ToolError::PathOutOfScope(resolved.clone())),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let Some((leaf, parents)) = components.split_last() else {
        return Err(ToolError::NotRegularFile(resolved));
    };

    let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let mut dir = open(root, dir_flags, Mode::empty()).map_err(errno_to_io)?;
    for component in parents {
        match openat(&dir, component, dir_flags, Mode::empty()) {
            Ok(next) => dir = next,
            Err(Errno::NOENT) if create_parents => {
                match mkdirat(&dir, component, Mode::from_raw_mode(0o755)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => return Err(ToolError::Io(errno_to_io(error))),
                }
                dir = openat(&dir, component, dir_flags, Mode::empty()).map_err(errno_to_io)?;
            }
            Err(error) => return Err(ToolError::Io(errno_to_io(error))),
        }
    }

    // `O_NONBLOCK` on the leaf open, always. The refusal below rejects anything
    // that is not a regular file, but it can only run once `openat` has
    // *returned* — and on a FIFO the open itself is what blocks: `O_RDONLY`
    // waits for a writer, `O_WRONLY` waits for a reader, both without bound. A
    // `mkfifo` dropped into the worktree by any allowed build command therefore
    // wedged the tool past the step's wall clock (which is only checked between
    // steps) and leaked the thread that asked. With the flag set the open
    // returns immediately — with a descriptor for a FIFO that has a peer, or
    // `ENXIO` for one that does not — and the refusal gets to run either way.
    // Character devices that block on open (a tty, a modem line) are closed off
    // by the same flag.
    let leaf_flags = OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let (fd, created) = match access {
        Access::Read => (
            openat(&dir, leaf, OFlags::RDONLY | leaf_flags, Mode::empty())
                .map_err(|error| leaf_open_error(error, &resolved))?,
            false,
        ),
        Access::ReadWrite => (
            openat(&dir, leaf, OFlags::RDWR | leaf_flags, Mode::empty())
                .map_err(|error| leaf_open_error(error, &resolved))?,
            false,
        ),
        Access::Write => match openat(&dir, leaf, OFlags::WRONLY | leaf_flags, Mode::empty()) {
            Ok(fd) => (fd, false),
            Err(Errno::NOENT) => (
                openat(
                    &dir,
                    leaf,
                    OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | leaf_flags,
                    // 0o666 and let the kernel apply the umask, exactly as an
                    // editor or `touch` would (0o644 under the usual 022). This was
                    // 0o600, so every file the agent created came out owner-only
                    // while its neighbours were not — invisible to a build running
                    // as another uid (a container, a CI runner), and a surprise in
                    // review. An existing file's mode is still preserved on
                    // overwrite; only creation changes.
                    Mode::from_raw_mode(0o666),
                )
                .map_err(errno_to_io)?,
                true,
            ),
            Err(error) => return Err(leaf_open_error(error, &resolved)),
        },
    };
    let file = std::fs::File::from(fd);
    if !file.metadata()?.is_file() {
        return Err(ToolError::NotRegularFile(resolved));
    }

    // Regular file confirmed, so drop `O_NONBLOCK` again: it was only ever
    // needed to survive the open. Leaving it set would hand every caller a
    // descriptor whose `read`/`write` may legally come back short or with
    // `EAGAIN` — and the callers here are ordinary blocking readers and
    // writers. On Linux a regular file ignores the flag, which is exactly why
    // leaving it would be a latent trap rather than an obvious one.
    let flags = fcntl_getfl(&file).map_err(errno_to_io)?;
    fcntl_setfl(&file, flags - OFlags::NONBLOCK).map_err(errno_to_io)?;
    Ok(ScopedFile {
        path: resolved,
        file,
        created,
        dir,
        leaf: leaf.clone(),
    })
}

/// Replace `scoped`'s contents with `bytes` **atomically**: write a fresh
/// sibling file, fsync it, and rename it over the leaf.
///
/// Truncating in place (`set_len(0)` then `write_all`) destroys the user's file
/// for any failure in between — a full disk, an I/O error, a killed daemon —
/// leaving a half-written or empty file and no copy of what was there. A rename
/// over the leaf is atomic for every reader: the file is either entirely the old
/// contents or entirely the new one, and a failure at any point before the
/// rename leaves the original untouched.
///
/// The temporary is created, renamed, and (on failure) removed **through the
/// same authorized directory handle** the leaf was opened through, so this adds
/// no path-resolution step an attacker could win by swapping an ancestor for a
/// symlink — the property `open_scoped` exists to provide. The original file's
/// permission bits are carried onto the replacement, since a rename installs a
/// new inode.
#[cfg(unix)]
pub(crate) fn replace_contents(scoped: &ScopedFile, bytes: &[u8]) -> Result<(), ToolError> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    use rustix::fs::{fsync, openat, renameat, unlinkat, AtFlags, Mode, OFlags};

    let mode = scoped.file.metadata()?.permissions().mode() & 0o7777;
    // `.` prefixed and uuid-suffixed: hidden from listings, and unique enough
    // that `O_EXCL` never collides with a live temporary of a concurrent edit.
    let temp_name = OsString::from(format!(
        ".codypendent-edit-{}.tmp",
        uuid::Uuid::now_v7().simple()
    ));
    let temp = openat(
        &scoped.dir,
        &temp_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(0o600),
    )
    .map_err(errno_to_io)?;
    let mut temp = std::fs::File::from(temp);

    // Anything that fails from here on removes the temporary and leaves the
    // original file exactly as it was.
    let write = (|| -> Result<(), ToolError> {
        temp.write_all(bytes)?;
        temp.set_permissions(std::fs::Permissions::from_mode(mode))?;
        // Durable BEFORE the rename: a rename that lands ahead of the data
        // would, after a crash, publish an empty file over the real one.
        temp.sync_all()?;
        renameat(&scoped.dir, &temp_name, &scoped.dir, &scoped.leaf).map_err(errno_to_io)?;
        Ok(())
    })();
    if let Err(error) = write {
        let _ = unlinkat(&scoped.dir, &temp_name, AtFlags::empty());
        return Err(error);
    }
    // The rename itself is only durable once the directory entry is synced. A
    // failure here means the new contents may not survive a power loss, but the
    // file is one of the two whole versions either way, so it is not fatal.
    let _ = fsync(&scoped.dir);
    Ok(())
}

/// The non-Unix stub. Unreachable in practice: [`open_scoped`] refuses on any
/// platform without descriptor-relative traversal, so no [`ScopedFile`] exists
/// here to replace.
#[cfg(not(unix))]
pub(crate) fn replace_contents(_scoped: &ScopedFile, _bytes: &[u8]) -> Result<(), ToolError> {
    Err(ToolError::Other(anyhow::anyhow!(
        "secure descriptor-relative filesystem tools are unsupported on this platform"
    )))
}

#[cfg(unix)]
fn leaf_open_error(error: rustix::io::Errno, path: &Path) -> ToolError {
    use rustix::io::Errno;

    if matches!(error, Errno::LOOP | Errno::ISDIR) {
        ToolError::NotRegularFile(path.to_path_buf())
    } else {
        ToolError::Io(errno_to_io(error))
    }
}

#[cfg(unix)]
fn errno_to_io(error: rustix::io::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(error.raw_os_error())
}

#[cfg(not(unix))]
fn open_scoped(
    path: &Path,
    scope: &PathScope,
    _access: Access,
    _create_parents: bool,
) -> Result<ScopedFile, ToolError> {
    let (resolved, verdict) = scope.resolve(path);
    match verdict {
        ScopeVerdict::Denied => Err(ToolError::PathDenied(resolved)),
        ScopeVerdict::OutsideRoots => Err(ToolError::PathOutOfScope(resolved)),
        ScopeVerdict::Allowed => Err(ToolError::Other(anyhow::anyhow!(
            "secure descriptor-relative filesystem tools are unsupported on this platform"
        ))),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn scope_for(root: &Path) -> PathScope {
        PathScope::new(vec![root.to_path_buf()], vec![])
    }

    /// Every leaf open must return, even when the leaf is a FIFO with no peer.
    ///
    /// The non-regular-file refusal runs only once `openat` has returned, and
    /// without `O_NONBLOCK` the open is itself the blocking call: `O_RDONLY`
    /// waits for a writer that never comes, `O_WRONLY` for a reader. A
    /// `mkfifo` left in the worktree by any allowed build command wedged the
    /// tool for the life of the process — past the step wall clock, which is
    /// only checked between steps — and leaked the thread that asked.
    ///
    /// Each open runs on its own thread with a deadline, so a regression fails
    /// this test instead of hanging the suite forever.
    #[test]
    fn a_fifo_leaf_is_refused_instead_of_blocking_forever() {
        // The fixture uses the POSIX `mkfifo(1)` utility rather than rustix:
        // `mknodat` is Linux-only and `mkfifoat` is `cfg(not(apple))`, so
        // rustix offers no FIFO-creation call on macOS and this test did not
        // COMPILE there. No CI job builds this crate's tests on macOS —
        // `test-macos` runs `-p codypendent-sandbox` only, and the
        // full-workspace `test` job runs on ubuntu — so `cargo test
        // --workspace` was broken on macOS with every gate green.

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let fifo = root.join("planted.fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("test fixture: run mkfifo");
        assert!(made.success(), "test fixture: mkfifo failed: {made:?}");

        // Read, write and read-write all reach the same leaf open, and each one
        // blocks on a peerless FIFO in its own way.
        for (label, open) in [
            (
                "open_read",
                open_read as fn(&Path, &PathScope) -> Result<ScopedFile, ToolError>,
            ),
            ("open_write", open_write),
            ("open_edit", open_edit),
        ] {
            let (tx, rx) = mpsc::channel();
            let fifo = fifo.clone();
            let root = root.clone();
            let worker = std::thread::spawn(move || {
                let result = open(&fifo, &scope_for(&root));
                // A receiver that has already timed out makes this a no-op.
                let _ = tx.send(matches!(
                    result,
                    Err(ToolError::NotRegularFile(_)) | Err(ToolError::Io(_))
                ));
            });

            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(refused) => {
                    assert!(refused, "{label} must refuse a FIFO, not accept it");
                    worker.join().expect("worker must not panic");
                }
                Err(_) => panic!(
                    "{label} blocked on a peerless FIFO: the leaf open needs O_NONBLOCK, \
                     because the non-regular-file refusal cannot run until it returns"
                ),
            }
        }
    }

    /// The flag exists only to survive the open. A regular file must come back
    /// with `O_NONBLOCK` cleared, so callers get the ordinary blocking
    /// `read`/`write` they are written against rather than a descriptor that
    /// may legally answer `EAGAIN`.
    #[test]
    fn a_regular_file_is_not_left_in_nonblocking_mode() {
        use rustix::fs::{fcntl_getfl, OFlags};

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("f.txt"), b"hello").unwrap();

        let scoped = open_read(&root.join("f.txt"), &scope_for(&root)).expect("regular file opens");
        let flags = fcntl_getfl(&scoped.file).expect("flags readable");
        assert!(
            !flags.contains(OFlags::NONBLOCK),
            "O_NONBLOCK must be cleared once the leaf is known to be a regular file"
        );
    }
}
