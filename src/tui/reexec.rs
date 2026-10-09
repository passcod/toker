//! Re-exec on upgrade: the dashboard notices its own binary being
//! replaced and starts the new one in its place.
//!
//! A dashboard left open across `cargo install --path . && toker
//! restart` otherwise keeps rendering with the old code — the proxy is
//! upgraded, the view of it is not, and the change looks like it did
//! not work. So the loop stats the path it was started from once a tick
//! ([`ExeWatch::check`]), and when that path names a different file
//! than at startup, it restores the terminal and `exec`s the path with
//! the same arguments ([`exec`]).
//!
//! The file is identified by device, inode, size and mtime: an install
//! that renames a new file into place changes the inode, one that
//! rewrites in place changes the size or mtime. A changed file is
//! only taken once it has held still for a whole check, so a binary
//! still being written is never exec'd half-copied; one that is not
//! executable (yet) is waited on the same way. A path that stops
//! existing is no change: the install is mid-way, or the binary was
//! removed, and in both cases the running dashboard is the best one
//! there is.

use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Which file a path names, as far as an upgrade can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ident {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
}

impl Ident {
    /// The path's current file, or `None` while it is missing,
    /// unreadable, or not executable.
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            return None;
        }
        Some(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
        })
    }
}

/// The running binary's path and the file it named at startup.
#[derive(Debug)]
pub(crate) struct ExeWatch {
    path: PathBuf,
    started: Ident,
    /// A changed file seen at the last check, waiting to hold still.
    pending: Option<Ident>,
}

impl ExeWatch {
    /// Watch the running binary. `None` when its path cannot be read
    /// (no `/proc`, say): the dashboard then simply never re-execs.
    pub(crate) fn current() -> Option<Self> {
        Self::at(std::env::current_exe().ok()?)
    }

    /// Watch `path`, taking the file it names now as the running one.
    fn at(path: PathBuf) -> Option<Self> {
        let started = Ident::of(&path)?;
        Some(Self {
            path,
            started,
            pending: None,
        })
    }

    /// Whether the path now names a different, settled binary: changed
    /// since startup, and the same at this check as at the last one.
    pub(crate) fn check(&mut self) -> bool {
        let now = Ident::of(&self.path).filter(|now| *now != self.started);
        let settled = now.is_some() && now == self.pending;
        self.pending = now;
        settled
    }

    /// The watched path, for the exec.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Replace this process with `path`, passing on this process's own
/// arguments. Only returns on failure; the caller has already handed
/// the terminal back, so the new binary starts from a clean one.
pub(crate) fn exec(path: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut args = std::env::args_os();
    let arg0 = args.next().unwrap_or_else(|| OsString::from("toker"));
    std::process::Command::new(path)
        .arg0(arg0)
        .args(args)
        .exec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh scratch directory, unique per call.
    fn test_dir(name: &str) -> crate::test_support::TestDir {
        crate::test_support::tempdir(&format!("toker-reexec-{name}-"))
    }

    fn write_exe(path: &Path, body: &[u8]) {
        fs::write(path, body).expect("write");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[test]
    fn an_unchanged_binary_never_triggers() {
        let dir = test_dir("unchanged");
        let exe = dir.join("toker");
        write_exe(&exe, b"old");
        let mut watch = ExeWatch::at(exe).expect("watch");
        for _ in 0..5 {
            assert!(!watch.check());
        }
    }

    #[test]
    fn a_replaced_binary_triggers_once_it_holds_still() {
        let dir = test_dir("replaced");
        let exe = dir.join("toker");
        write_exe(&exe, b"old");
        let mut watch = ExeWatch::at(exe.clone()).expect("watch");
        // The install's rename: a new file, a new inode.
        let staged = dir.join("toker.new");
        write_exe(&staged, b"new binary");
        fs::rename(&staged, &exe).expect("rename");
        // Seen once: pending, not yet taken.
        assert!(!watch.check());
        // Seen again, unchanged: settled.
        assert!(watch.check());
    }

    #[test]
    fn a_binary_still_being_written_is_waited_on() {
        let dir = test_dir("writing");
        let exe = dir.join("toker");
        write_exe(&exe, b"old");
        let mut watch = ExeWatch::at(exe.clone()).expect("watch");
        write_exe(&exe, b"new, part");
        assert!(!watch.check());
        // Still growing at the next check: not settled.
        write_exe(&exe, b"new, part two of it");
        assert!(!watch.check());
        assert!(watch.check());
    }

    #[test]
    fn a_missing_or_unexecutable_path_is_no_change() {
        let dir = test_dir("missing");
        let exe = dir.join("toker");
        write_exe(&exe, b"old");
        let mut watch = ExeWatch::at(exe.clone()).expect("watch");
        fs::remove_file(&exe).expect("remove");
        assert!(!watch.check());
        assert!(!watch.check());
        // Back, but not executable yet: still waiting.
        fs::write(&exe, b"new").expect("write");
        assert!(!watch.check());
        assert!(!watch.check());
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(!watch.check());
        assert!(watch.check());
    }
}
