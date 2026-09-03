//! Watch the config directory and say when something in it changed.
//!
//! The *directory*, not the file. Editors do not write in place - vim writes a
//! temp file and renames it over the target - so a watch on the config inode
//! sees the first save and nothing afterwards, because the inode it is holding
//! is no longer the file you are editing. Watching the directory also picks up
//! anything `require`d from a neighbouring file for free.

use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::fs::{StatVfsMountFlags, statvfs};

/// Editors emit several events for one save (a rename, a chmod, an attrib
/// change). Wait for the dust to settle rather than reloading three times.
pub const SETTLE: Duration = Duration::from_millis(150);

pub struct Watcher {
    fd: OwnedFd,
    buf: Vec<MaybeUninit<u8>>,
    /// Set when something changed; cleared once the reload has happened.
    dirty_since: Option<Instant>,
}

/// Directories a config at `given`, resolving to `resolved`, can change in.
///
/// Two layouts, two answers. When the config is a symlink into a dotfiles
/// repo the editor writes next to the *resolved* file, so that is the
/// directory the event lands in. When a config manager publishes it,
/// `~/.config/clipmunge` is a real directory whose `config.lua` symlink gets
/// swapped, and the event is next to the *given* name instead. Watch both;
/// they are usually the same directory anyway.
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

/// True for a directory whose contents cannot change without a remount.
///
/// This is what keeps clipmunge off `/nix/store`. A config published by
/// home-manager resolves to a store path, whose parent is the store itself:
/// watching it means every unrelated build on the box wakes the daemon to
/// reload a file that is immutable by construction. The general statement -
/// do not watch a directory the config cannot be edited in - covers it
/// without the daemon having to know what nix is.
fn read_only(dir: &Path) -> bool {
    match statvfs(dir) {
        Ok(st) => st.f_flag.contains(StatVfsMountFlags::RDONLY),
        // Not being able to ask is not a reason to skip it.
        Err(_) => false,
    }
}

impl Watcher {
    /// Watch every directory in `dirs` that can actually change.
    ///
    /// Fails when none can, so the caller says so once instead of the daemon
    /// pretending to reload for the rest of the session.
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
        // The loop ends on the first Err: queue empty, or the buffer ran out
        // mid-read; either way we have learned what we needed - that
        // something moved.
        let mut lost = 0;
        while let Ok(event) = reader.next() {
            if event.events().contains(ReadFlags::IGNORED) {
                // The watch is gone - the directory was deleted or replaced.
                // Not a change to reload on, but not nothing either: reload
                // is now partly or wholly deaf and has to say so rather than
                // going quiet for the rest of the session.
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
