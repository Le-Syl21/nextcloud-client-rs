// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The guard that stops two programs (`ncsyncd`, the GUI, or two of either)
//! from running on the same configuration file, and the control endpoint
//! named after it.
//!
//! No upstream counterpart: the official client has one configuration per
//! user and KDSingleApplication. Here every configuration file has an
//! exclusive OS file lock, `<state_dir>/instance-<hash>.lock`, taken before
//! anything reads the configuration and held for the life of the process.
//! `<hash>` is the md5 of the canonical configuration path, cut like the
//! journal names of `SyncJournalDb::makeDbName` (6 bytes, 12 hex digits).
//! The OS releases the lock when the process dies, even on `kill -9`, so
//! there is no stale lock to detect. The holder writes its pid and program
//! name in the file, so a refusal can say who holds it.
//!
//! The control endpoint carries the same hash
//! ([`default_endpoint`]): `$XDG_RUNTIME_DIR/ncsyncd/<hash>.sock` for a
//! user, `/run/ncsyncd/<instance>/<hash>.sock` for a system instance.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};

use crate::config_file::ConfigLocation;
#[cfg(unix)]
use crate::config_file::ServiceMode;

/// The configuration path the hash is made of: canonical when it exists,
/// else its canonical parent joined with its name (a GUI's first run
/// creates the file later), else absolute.
pub fn canonical_config_path(config_file: &Path) -> PathBuf {
    if let Ok(p) = std::fs::canonicalize(config_file) {
        return p;
    }
    if let (Some(parent), Some(name)) = (config_file.parent(), config_file.file_name())
        && let Ok(p) = std::fs::canonicalize(if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        })
    {
        return p.join(name);
    }
    std::path::absolute(config_file).unwrap_or_else(|_| config_file.to_owned())
}

/// The hash naming the lock and the control endpoint of a configuration.
pub fn config_hash(config_file: &Path) -> String {
    let path = canonical_config_path(config_file);
    let digest = Md5::digest(path.as_os_str().as_encoded_bytes());
    digest[..6].iter().map(|b| format!("{b:02x}")).collect()
}

/// The lock file of a configuration.
pub fn lock_path(location: &ConfigLocation) -> PathBuf {
    location.state_dir.join(format!(
        "instance-{}.lock",
        config_hash(&location.config_file)
    ))
}

/// Where the holder is also written on Windows: a byte-range lock makes the
/// lock file itself unreadable to the process that is refused.
#[cfg(windows)]
fn holder_path(lock: &Path) -> PathBuf {
    let mut p = lock.as_os_str().to_owned();
    p.push(".holder");
    PathBuf::from(p)
}

/// The default control endpoint of a configuration:
/// `$XDG_RUNTIME_DIR/ncsyncd/<hash>.sock` for a user (without
/// `XDG_RUNTIME_DIR`: `/tmp/ncsyncd-<uid>/ncsyncd/<hash>.sock`),
/// `/run/ncsyncd/<instance>/<hash>.sock` for an instance of the system
/// service (its `RuntimeDirectory=`).
#[cfg(windows)]
pub fn default_endpoint(location: &ConfigLocation) -> PathBuf {
    // Windows: a named pipe of the user (the official client's socket API
    // uses `\\.\pipe\<exe>-<USERNAME>`).
    let user = std::env::var("USERNAME").unwrap_or_default();
    PathBuf::from(format!(
        r"\\.\pipe\ncsyncd-{user}-{}",
        config_hash(&location.config_file)
    ))
}

/// See the Windows variant.
#[cfg(unix)]
pub fn default_endpoint(location: &ConfigLocation) -> PathBuf {
    let name = format!("{}.sock", config_hash(&location.config_file));
    if let ServiceMode::System { instance } = &location.mode {
        return PathBuf::from(format!("/run/ncsyncd/{instance}")).join(name);
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| format!("/tmp/ncsyncd-{}", rustix::process::getuid().as_raw()));
    PathBuf::from(base).join("ncsyncd").join(name)
}

/// The process holding a configuration, as written in its lock file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    pub program: String,
}

impl Holder {
    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let pid = lines.next()?.trim().parse().ok()?;
        let program = lines.next().unwrap_or("").trim().to_owned();
        Some(Self { pid, program })
    }
}

/// Why the guard could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("{} is already in use by {} (lock {})", config.display(), holder_text(holder.as_ref()), lock.display())]
    Held {
        config: PathBuf,
        lock: PathBuf,
        holder: Option<Holder>,
    },
    #[error("cannot lock {}: {source}", lock.display())]
    Io {
        lock: PathBuf,
        source: std::io::Error,
    },
}

fn holder_text(holder: Option<&Holder>) -> String {
    match holder {
        Some(h) if !h.program.is_empty() => format!("{} (pid {})", h.program, h.pid),
        Some(h) => format!("pid {}", h.pid),
        None => "another program".to_owned(),
    }
}

/// The exclusive lock of a configuration, held until it is dropped (or the
/// process ends).
#[derive(Debug)]
pub struct InstanceGuard {
    _file: File,
    lock: PathBuf,
    hash: String,
}

impl InstanceGuard {
    /// Takes the lock of `location`'s configuration for `program` (written
    /// in the lock file with the pid). Creates the state directory (mode
    /// 0700) when needed.
    pub fn acquire(location: &ConfigLocation, program: &str) -> Result<Self, GuardError> {
        let lock = lock_path(location);
        let io = |source| GuardError::Io {
            lock: lock.clone(),
            source,
        };
        if !location.state_dir.exists() {
            std::fs::create_dir_all(&location.state_dir).map_err(io)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &location.state_dir,
                    std::fs::Permissions::from_mode(0o700),
                );
            }
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock)
            .map_err(io)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let mut text = String::new();
                let _ = file.read_to_string(&mut text);
                // Windows: the lock covers the file, nobody else can read it.
                #[cfg(windows)]
                if text.is_empty() {
                    text = std::fs::read_to_string(holder_path(&lock)).unwrap_or_default();
                }
                return Err(GuardError::Held {
                    config: location.config_file.clone(),
                    lock,
                    holder: Holder::parse(&text),
                });
            }
            Err(TryLockError::Error(e)) => return Err(io(e)),
        }
        let write = |file: &mut File| -> std::io::Result<()> {
            file.set_len(0)?;
            file.rewind()?;
            writeln!(file, "{}\n{program}", std::process::id())?;
            file.flush()
        };
        write(&mut file).map_err(io)?;
        #[cfg(windows)]
        let _ = std::fs::write(
            holder_path(&lock),
            format!("{}\n{program}\n", std::process::id()),
        );
        Ok(Self {
            _file: file,
            hash: config_hash(&location.config_file),
            lock,
        })
    }

    /// The hash of the configuration (see [`config_hash`]).
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// The lock file.
    pub fn lock_path(&self) -> &Path {
        &self.lock
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_file::ServiceMode;

    fn location(dir: &Path) -> ConfigLocation {
        ConfigLocation {
            mode: ServiceMode::User,
            config_file: dir.join("ncsyncd.cfg"),
            state_dir: dir.join("state"),
        }
    }

    #[test]
    fn derived_guard_refuses_a_second_holder_and_names_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let loc = location(dir.path());
        let first = InstanceGuard::acquire(&loc, "ncsyncd").unwrap();
        // flock locks belong to the open file: a second open conflicts even
        // in the same process.
        match InstanceGuard::acquire(&loc, "nc-client") {
            Err(GuardError::Held { holder, .. }) => assert_eq!(
                holder,
                Some(Holder {
                    pid: std::process::id(),
                    program: "ncsyncd".into()
                })
            ),
            other => panic!("not refused: {other:?}"),
        }
        drop(first);
        let again = InstanceGuard::acquire(&loc, "nc-client").unwrap();
        assert_eq!(again.hash(), config_hash(&loc.config_file));
    }

    #[test]
    fn derived_hash_is_of_the_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("ncsyncd.cfg");
        // Not created yet: the canonical parent joined with the name.
        let before = config_hash(&cfg);
        std::fs::write(&cfg, "").unwrap();
        assert_eq!(config_hash(&cfg), before);
        let link = dir.path().join("link.cfg");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&cfg, &link).unwrap();
            assert_eq!(config_hash(&link), before);
        }
        let _ = &link;
        let dotted = dir.path().join(".").join("ncsyncd.cfg");
        assert_eq!(config_hash(&dotted), before);
        assert_eq!(before.len(), 12);
        assert_ne!(config_hash(&dir.path().join("other.cfg")), before);
    }

    // The Windows endpoint is a named pipe.
    #[cfg(unix)]
    #[test]
    fn derived_endpoint_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut loc = location(dir.path());
        let hash = config_hash(&loc.config_file);
        loc.mode = ServiceMode::System {
            instance: "alice".into(),
        };
        assert_eq!(
            default_endpoint(&loc),
            PathBuf::from(format!("/run/ncsyncd/alice/{hash}.sock"))
        );
        loc.mode = ServiceMode::User;
        assert!(
            default_endpoint(&loc)
                .to_string_lossy()
                .ends_with(&format!("/ncsyncd/{hash}.sock"))
        );
    }
}
