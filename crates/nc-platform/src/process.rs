// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! What a lock file needs to know about the machine and its processes
//! (`QLockFile`: the host name and whether the owner is still running).

/// The host name of the machine (empty when it cannot be read).
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        rustix::system::uname()
            .nodename()
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(windows)]
    {
        crate::windows::computer_name()
    }
}

/// Whether the process `pid` exists (it may be another user's).
pub fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Some(pid) = i32::try_from(pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        else {
            return false;
        };
        // Signal 0 only checks: "no permission" still means it exists.
        match rustix::process::test_kill_process(pid) {
            Ok(()) => true,
            Err(e) => e == rustix::io::Errno::PERM,
        }
    }
    #[cfg(windows)]
    {
        crate::windows::process_is_alive(pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive() {
        assert!(process_is_alive(std::process::id()));
        assert!(!hostname().is_empty());
    }

    #[test]
    fn finished_child_is_gone() {
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
            .args(if cfg!(windows) {
                vec!["/C", "exit"]
            } else {
                vec![]
            })
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!process_is_alive(pid));
    }
}
