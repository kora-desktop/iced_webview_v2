//! Who owns a CEF profile directory, and clearing what a dead browser left in it.
//!
//! Chromium marks a profile in use with `SingletonLock`, a symlink to
//! `hostname-pid`. It clears a lock whose process on this host is gone, but a
//! lock naming another host stops it cold: it asks the user whether that other
//! computer is still using the profile, an embedded browser cannot show the
//! question, and `initialize()` fails, leaving every view blank. A lock names
//! another host whenever the machine was renamed after a run that did not shut
//! down cleanly. Inside a PID namespace the pid is no better a witness: the app
//! gets the same small pid every run.
//!
//! So every instance also holds a file lock of its own in the profile, which
//! the kernel drops however the process ends. Whoever gets it knows no other
//! instance is running there, and can clear Chromium's files.
//!
//! A profile held by a running browser is never handed to Chromium: a second
//! browser on it would pass its command line to the first and exit, and the
//! first would open that as a free-standing Chrome window. A second instance
//! of the app takes the next free profile instead (`<app>-2`, `<app>-3`, …).

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// The file a running instance keeps locked inside its profile.
const PROFILE_LOCK: &str = "iced_webview.lock";

/// Chromium's singleton files, all left behind together.
const SINGLETON_FILES: [&str; 3] = ["SingletonLock", "SingletonSocket", "SingletonCookie"];

/// How many instances of one app may run at once, each with its own profile.
const MAX_PROFILES: u32 = 64;

/// Take the profile at `dir` for as long as the returned file is open, or
/// `None` when a running instance already holds it.
fn claim(dir: &Path) -> Option<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(PROFILE_LOCK))
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}

/// Whether the `SingletonLock` pointing at `target` belongs to no running
/// browser, given that this process (`own_pid` on `host`) holds the profile's
/// [`PROFILE_LOCK`].
///
/// One exception keeps the lock: a live process on this host other than us,
/// which could be an older build that predates [`PROFILE_LOCK`].
fn singleton_is_stale(target: &str, host: &str, own_pid: u32, alive: impl Fn(u32) -> bool) -> bool {
    // `hostname-pid`, and a hostname may itself contain '-'.
    let Some((lock_host, pid)) = target.rsplit_once('-') else {
        return true;
    };
    let Ok(pid) = pid.parse::<u32>() else {
        return true;
    };
    !(lock_host == host && pid != own_pid && alive(pid))
}

/// The `n`th profile of the app whose first profile is `root`.
fn slot(root: &Path, n: u32) -> PathBuf {
    if n == 1 {
        return root.to_path_buf();
    }
    let mut name = root.file_name().unwrap_or_default().to_os_string();
    name.push(format!("-{n}"));
    root.with_file_name(name)
}

/// Claim the first profile from `root` on that no running instance holds,
/// with the singleton files a dead browser left there cleared.
fn claim_slot(root: &Path) -> Option<(PathBuf, File)> {
    let host = host_name();
    let alive = |pid: u32| Path::new(&format!("/proc/{pid}")).exists();
    (1..=MAX_PROFILES).find_map(|n| {
        let dir = slot(root, n);
        let _ = std::fs::create_dir_all(&dir);
        let Some(file) = claim(&dir) else {
            log::debug!(
                "iced_webview: profile {} is held by a running instance",
                dir.display()
            );
            return None;
        };
        if let Ok(target) = std::fs::read_link(dir.join("SingletonLock")) {
            let target = target.to_string_lossy();
            if !singleton_is_stale(&target, &host, std::process::id(), alive) {
                log::debug!(
                    "iced_webview: profile {} is locked by live pid {target}, trying the next",
                    dir.display()
                );
                return None;
            }
            log::info!(
                "iced_webview: clearing the profile lock {target} left in {} (this host is {host})",
                dir.display()
            );
        }
        for name in SINGLETON_FILES {
            let _ = std::fs::remove_file(dir.join(name));
        }
        Some((dir, file))
    })
}

/// Claim a profile for the rest of this process: `root` when no running
/// instance holds it, else the next free one. `None` when every one is taken.
pub(crate) fn claim_free(root: &Path) -> Option<PathBuf> {
    let (dir, file) = claim_slot(root)?;
    // Held until the process exits; the kernel releases it then.
    std::mem::forget(file);
    Some(dir)
}

/// This host's name as Chromium reads it, for comparing with a lock's.
fn host_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim_end().to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "iced_webview-profile-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("app")
    }

    #[test]
    fn a_second_instance_gets_a_profile_of_its_own() {
        let root = scratch("second");
        let (first, _held) = claim_slot(&root).unwrap();
        let (second, _held2) = claim_slot(&root).unwrap();
        assert_eq!(first, root);
        assert_ne!(first, second, "two running instances shared one profile");
        assert_eq!(second, root.with_file_name("app-2"));
    }

    #[test]
    fn a_profile_is_free_again_once_its_instance_ends() {
        let root = scratch("ended");
        let (first, held) = claim_slot(&root).unwrap();
        drop(held);
        let (again, _held) = claim_slot(&root).unwrap();
        assert_eq!(first, again);
    }

    #[test]
    fn a_live_browser_lock_is_never_shared() {
        let root = scratch("live");
        std::fs::create_dir_all(&root).unwrap();
        // pid 1 is always alive and never this test.
        std::os::unix::fs::symlink(format!("{}-1", host_name()), root.join("SingletonLock"))
            .unwrap();
        let (dir, _held) = claim_slot(&root).unwrap();
        assert_eq!(dir, root.with_file_name("app-2"));
        assert!(std::fs::read_link(root.join("SingletonLock")).is_ok());
    }

    #[test]
    fn a_dead_browser_lock_is_cleared() {
        let root = scratch("dead");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink("another-host-12345", root.join("SingletonLock")).unwrap();
        std::fs::write(root.join("SingletonCookie"), "").unwrap();
        let (dir, _held) = claim_slot(&root).unwrap();
        assert_eq!(dir, root);
        assert!(std::fs::symlink_metadata(root.join("SingletonLock")).is_err());
        assert!(!root.join("SingletonCookie").exists());
    }
}
