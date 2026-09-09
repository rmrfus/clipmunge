//! Watch config directories with inotify. Directory watches survive atomic
//! file replacements and detect changes to neighbouring Lua modules.

use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::fs::{StatVfsMountFlags, statvfs};

/// Debounce multiple events from a single save.
pub const SETTLE: Duration = Duration::from_millis(150);

pub struct Watcher {
    fd: OwnedFd,
    buf: Vec<MaybeUninit<u8>>,
    /// Last observed change; cleared when a settled change is consumed.
    dirty_since: Option<Instant>,
}

/// Watch both the given and resolved config directories: editors can replace
/// the target file, while config managers can replace the symlink.
/// These candidates are used at startup; reloads do not update the watches.
pub fn candidates(given: &Path, resolved: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for p in [given, resolved] {
        // `--config config.lua` has a parent of "", not None, and inotify does
        // not watch "".
        let dir = match p.parent() {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => PathBuf::from("."),
        };
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// Check whether the directory is on a read-only filesystem.
/// Skip /nix/store when mounted read-only to avoid unrelated reloads.
fn read_only(dir: &Path) -> bool {
    match statvfs(dir) {
        Ok(st) => st.f_flag.contains(StatVfsMountFlags::RDONLY),
        // Not being able to ask is not a reason to skip it.
        Err(_) => false,
    }
}

impl Watcher {
    /// Watch directories on writable filesystems. Fail if none can be watched.
    pub fn new(dirs: &[PathBuf]) -> Result<Self> {
        let fd = inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC)
            .map_err(|e| anyhow::anyhow!("inotify_init: {e}"))?;

        let mut watched = 0;
        for dir in dirs {
            if read_only(dir) {
                log::info!("not watching {}: read-only filesystem", dir.display());
                continue;
            }
            match inotify::add_watch(
                &fd,
                dir,
                WatchFlags::CLOSE_WRITE
                    | WatchFlags::MOVED_TO
                    | WatchFlags::CREATE
                    | WatchFlags::DELETE
                    | WatchFlags::MOVED_FROM,
            ) {
                Ok(_) => {
                    log::debug!("watching {}", dir.display());
                    watched += 1;
                }
                Err(e) => log::warn!("not watching {}: {e}", dir.display()),
            }
        }
        if watched == 0 {
            bail!("no watchable directory among {} candidate(s)", dirs.len());
        }

        Ok(Self {
            fd,
            buf: vec![MaybeUninit::uninit(); 4096],
            dirty_since: None,
        })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Drain the queue. Call whenever the fd polls readable.
    pub fn absorb(&mut self) {
        let mut reader = inotify::Reader::new(self.fd.as_fd(), &mut self.buf);
        // Stop after draining available events or encountering a read error.
        let mut lost = 0;
        while let Ok(event) = reader.next() {
            if event.events().contains(ReadFlags::IGNORED) {
                // A deleted or replaced directory loses its watch. Report the loss.
                lost += 1;
                continue;
            }
            self.dirty_since = Some(Instant::now());
        }
        if lost > 0 {
            log::warn!(
                "{lost} config watch(es) went away (directory replaced?);                  restart clipmunge to watch again"
            );
        }
    }

    /// How long to block before the pending change is old enough to act on.
    /// None means nothing is pending, so block indefinitely.
    pub fn timeout(&self) -> Option<Duration> {
        self.dirty_since.map(|t| SETTLE.saturating_sub(t.elapsed()))
    }

    /// True once, when a change has settled.
    pub fn take_settled(&mut self) -> bool {
        match self.dirty_since {
            Some(t) if t.elapsed() >= SETTLE => {
                self.dirty_since = None;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_filename_watches_the_working_directory() {
        // `--config config.lua` has a parent of "", not None, and inotify
        // does not watch "".
        assert_eq!(
            candidates(Path::new("config.lua"), Path::new("config.lua")),
            vec![PathBuf::from(".")]
        );
    }

    #[test]
    fn the_same_directory_twice_is_watched_once() {
        assert_eq!(
            candidates(
                Path::new("/home/u/.config/clipmunge/config.lua"),
                Path::new("/home/u/.config/clipmunge/config.lua"),
            ),
            vec![PathBuf::from("/home/u/.config/clipmunge")]
        );
    }

    #[test]
    fn given_and_resolved_directories_are_both_watched() {
        // Cover changes beside both the symlink and its target.
        assert_eq!(
            candidates(
                Path::new("/home/u/.config/clipmunge/config.lua"),
                Path::new("/home/u/dotfiles/clipmunge/config.lua"),
            ),
            vec![
                PathBuf::from("/home/u/.config/clipmunge"),
                PathBuf::from("/home/u/dotfiles/clipmunge"),
            ]
        );
    }
}
