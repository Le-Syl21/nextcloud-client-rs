// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! The system instance `ncsyncd@USER`: one daemon per user, running as that
//! user (`User=%i` in the unit), never as root.
//!
//! The configuration commands (`ncsync ... --instance USER`) are usually run
//! as root, before the service has ever started. Everything they write in
//! the instance's state directory (configuration, lock file, password files)
//! and a local folder they create are then given to the instance user, so
//! that the daemon can read and update them. systemd does the same for the
//! state directory when the service starts (`StateDirectory=` fixes the
//! ownership of a directory that belongs to someone else), but not for files
//! written while the service already runs.

use std::path::Path;

use crate::config_file::{ConfigLocation, ServiceMode};

/// Errors about the instance user.
#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    #[error(
        "the system instance ncsyncd@{0} runs as the user {0}, but there is no such user (the instance name is the user name)"
    )]
    NoSuchUser(String),
    #[error("could not look up the user {user}: {source}")]
    Lookup { user: String, source: nix::Error },
    #[error("could not give {path} to the user {user}: {source}")]
    Chown {
        path: String,
        user: String,
        source: std::io::Error,
    },
    #[error(
        "the system instance ncsyncd@{0} must not run as root: the unit runs it as the user {0} (User=%i)"
    )]
    RunningAsRoot(String),
}

/// The uid and gid of the instance user (its primary group).
pub fn instance_user(instance: &str) -> Result<(u32, u32), InstanceError> {
    match nix::unistd::User::from_name(instance) {
        Ok(Some(u)) => Ok((u.uid.as_raw(), u.gid.as_raw())),
        Ok(None) => Err(InstanceError::NoSuchUser(instance.to_owned())),
        Err(source) => Err(InstanceError::Lookup {
            user: instance.to_owned(),
            source,
        }),
    }
}

/// `true` when this process runs as root.
pub fn running_as_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// The daemon side: a system instance refuses to run as root.
pub fn check_daemon_user(location: &ConfigLocation) -> Result<(), InstanceError> {
    match &location.mode {
        ServiceMode::System { instance } if running_as_root() => {
            Err(InstanceError::RunningAsRoot(instance.clone()))
        }
        _ => Ok(()),
    }
}

fn chown_one(path: &Path, owner: (u32, u32), user: &str) -> Result<(), InstanceError> {
    std::os::unix::fs::lchown(path, Some(owner.0), Some(owner.1)).map_err(|source| {
        InstanceError::Chown {
            path: path.display().to_string(),
            user: user.to_owned(),
            source,
        }
    })
}

fn chown_tree(path: &Path, owner: (u32, u32), user: &str) -> Result<(), InstanceError> {
    chown_one(path, owner, user)?;
    let meta = std::fs::symlink_metadata(path).map_err(|source| InstanceError::Chown {
        path: path.display().to_string(),
        user: user.to_owned(),
        source,
    })?;
    if meta.is_dir() {
        let entries = std::fs::read_dir(path).map_err(|source| InstanceError::Chown {
            path: path.display().to_string(),
            user: user.to_owned(),
            source,
        })?;
        for entry in entries.flatten() {
            chown_tree(&entry.path(), owner, user)?;
        }
    }
    Ok(())
}

/// After a configuration command: when it ran as root on a system instance,
/// gives the instance's state directory (recursively) and `extra` (not
/// recursively: a local folder the command created) to the instance user.
/// Does nothing for the user daemon or when not running as root.
pub fn hand_to_instance_user(
    location: &ConfigLocation,
    extra: Option<&Path>,
) -> Result<(), InstanceError> {
    let ServiceMode::System { instance } = &location.mode else {
        return Ok(());
    };
    if !running_as_root() {
        return Ok(());
    }
    let owner = instance_user(instance)?;
    if location.state_dir.exists() {
        chown_tree(&location.state_dir, owner, instance)?;
    }
    if let Some(p) = extra {
        chown_one(p, owner, instance)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_unknown_instance_user_is_an_error() {
        assert!(matches!(
            instance_user("ncsyncd-no-such-user-xyz"),
            Err(InstanceError::NoSuchUser(_))
        ));
    }

    #[test]
    fn derived_root_user_lookup() {
        assert_eq!(instance_user("root").unwrap(), (0, 0));
    }

    #[test]
    fn derived_user_mode_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let loc = ConfigLocation {
            mode: ServiceMode::User,
            config_file: dir.path().join("x.cfg"),
            state_dir: dir.path().to_owned(),
        };
        hand_to_instance_user(&loc, None).unwrap();
        check_daemon_user(&loc).unwrap();
    }
}
