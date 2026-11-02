// SPDX-FileCopyrightText: 2017 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `ownCloudGui::slotComputeOverallSyncStatus`
// (`src/gui/owncloudgui.cpp`), `FolderMan::trayOverallStatus` and
// `FolderMan::trayTooltipStatusString` (`src/gui/folderman.cpp`),
// `Theme::syncStateIcon` (`src/libsync/theme.cpp`) and
// `Folder::shortGuiLocalPath` (`src/gui/folder.cpp`) (nextcloud/desktop
// v34.0.5). Not ported: the macOS File Provider branches.

//! The tray icon's state and tooltip, as a pure function of a
//! [`Snapshot`]. The GUI recomputes them when a folder's or an account's
//! state changes, on every progress update and 2 s after a sync is done
//! (upstream's connections), and draws its own icon for each
//! [`StateIcon`].

use crate::sync_result::SyncStatus;
use crate::ui_event::{FolderSnapshot, ProgressPhase, ProgressSummary, Snapshot};

/// The tray icons of `Theme::syncStateIcon` and `folderOfflineIcon`
/// (`state-ok`, `state-sync`, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateIcon {
    Ok,
    Sync,
    Pause,
    Warning,
    Error,
    Offline,
}

/// `Theme::syncStateIcon(status, sysTray)`.
pub fn sync_state_icon(status: SyncStatus) -> StateIcon {
    match status {
        // this can happen if no sync connections are configured.
        SyncStatus::Undefined => StateIcon::Warning,
        SyncStatus::NotYetStarted | SyncStatus::SyncRunning => StateIcon::Sync,
        SyncStatus::SyncAbortRequested | SyncStatus::Paused => StateIcon::Pause,
        SyncStatus::SyncPrepare | SyncStatus::Success => StateIcon::Ok,
        SyncStatus::Problem => StateIcon::Warning,
        SyncStatus::Error | SyncStatus::SetupError => StateIcon::Error,
    }
}

/// How the tooltip is written: Windows has a 128-character limit, so it
/// gets the short form (`Q_OS_WIN`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TooltipStyle {
    Windows,
    Other,
}

impl TooltipStyle {
    /// The style of the OS the program runs on.
    pub fn native() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

/// What the tray shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayState {
    pub icon: StateIcon,
    pub tooltip: String,
}

/// What `FolderMan::trayOverallStatus` computes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverallStatus<'a> {
    pub status: SyncStatus,
    pub unresolved_conflicts: bool,
    /// The folder's progress when there is exactly one folder.
    pub progress: Option<&'a ProgressSummary>,
}

/// `FolderMan::trayOverallStatus(folders, ...)`.
///
/// If one folder: show the state of the one folder along with the sync
/// status. If more folders: if one of them has an error, show error; if
/// one is paused, but others ok, show ok; do not show "problem" in the
/// tray and do not show sync status.
pub fn tray_overall_status<'a>(folders: &[&'a FolderSnapshot]) -> OverallStatus<'a> {
    let mut out = OverallStatus {
        status: SyncStatus::Undefined,
        unresolved_conflicts: false,
        progress: None,
    };
    if let [folder] = folders {
        out.status = if folder.paused {
            SyncStatus::Paused
        } else {
            match folder.status {
                SyncStatus::Undefined => SyncStatus::Error,
                // don't show the problem icon in tray.
                SyncStatus::Problem => SyncStatus::Success,
                s => s,
            }
        };
        out.unresolved_conflicts = folder.unresolved_conflicts;
        out.progress = folder.progress.as_ref();
        return out;
    }
    let mut errors_seen = false;
    let mut good_seen = false;
    let mut abort_or_paused_seen = false;
    let mut run_seen = false;
    let mut initial_state_seen = false;
    for folder in folders {
        // We've already seen an error, worst case met. No need to check the
        // remaining folders.
        if errors_seen {
            break;
        }
        if folder.paused {
            abort_or_paused_seen = true;
        } else {
            match folder.status {
                SyncStatus::Undefined | SyncStatus::NotYetStarted => initial_state_seen = true,
                SyncStatus::SyncPrepare | SyncStatus::SyncRunning => run_seen = true,
                // don't show the problem icon in tray.
                SyncStatus::Problem | SyncStatus::Success => good_seen = true,
                SyncStatus::Error | SyncStatus::SetupError => errors_seen = true,
                SyncStatus::SyncAbortRequested | SyncStatus::Paused => abort_or_paused_seen = true,
            }
        }
        if folder.unresolved_conflicts {
            out.unresolved_conflicts = true;
        }
    }
    out.status = if errors_seen {
        SyncStatus::Error
    } else if abort_or_paused_seen {
        // only if all folders are paused
        SyncStatus::Paused
    } else if run_seen {
        SyncStatus::SyncRunning
    } else if good_seen {
        SyncStatus::Success
    } else if initial_state_seen {
        SyncStatus::NotYetStarted
    } else {
        SyncStatus::Undefined
    };
    out
}

/// `FolderMan::trayTooltipStatusString(syncStatus, hasUnresolvedConflicts,
/// paused, progress)`.
pub fn tray_tooltip_status_string(
    sync_status: SyncStatus,
    has_unresolved_conflicts: bool,
    paused: bool,
    progress: Option<&ProgressSummary>,
) -> String {
    use nc_sync::utility::{duration_to_descriptive_string1, octets_to_string};
    let folder_message = match sync_status {
        SyncStatus::Undefined => "Undefined state.".to_owned(),
        SyncStatus::NotYetStarted => "Waiting to start syncing.".to_owned(),
        SyncStatus::SyncPrepare => "Preparing for sync.".to_owned(),
        SyncStatus::SyncRunning => match progress {
            Some(p) if p.status == ProgressPhase::Propagation => {
                // trustEta(): the estimate, when there is one to trust.
                let eta = p.eta_msecs;
                if p.total_size == 0 {
                    let current_file = p.current_file;
                    let total_file_count = p.total_files.max(current_file);
                    match eta {
                        Some(0) => format!(
                            "Syncing {current_file} of {total_file_count} (A few seconds left)"
                        ),
                        Some(ms) => format!(
                            "Syncing {current_file} of {total_file_count} ({} left)",
                            duration_to_descriptive_string1(ms)
                        ),
                        None => format!("Syncing {current_file} of {total_file_count}"),
                    }
                } else {
                    let total_size_str = octets_to_string(p.total_size);
                    match eta {
                        Some(0) => format!("Syncing {total_size_str} (A few seconds left)"),
                        Some(ms) => format!(
                            "Syncing {total_size_str} ({} left)",
                            duration_to_descriptive_string1(ms)
                        ),
                        None => format!("Syncing {total_size_str}"),
                    }
                }
            }
            _ => "Sync is running.".to_owned(),
        },
        SyncStatus::Success | SyncStatus::Problem => {
            if has_unresolved_conflicts {
                "Sync finished with unresolved conflicts.".to_owned()
            } else {
                "Last sync was successful.".to_owned()
            }
        }
        SyncStatus::Error => String::new(),
        SyncStatus::SetupError => "Setup error.".to_owned(),
        SyncStatus::SyncAbortRequested => "Sync request was cancelled.".to_owned(),
        SyncStatus::Paused => "Sync is paused.".to_owned(),
    };
    if paused {
        // sync is disabled.
        format!("{folder_message} (Sync is paused)")
    } else {
        folder_message
    }
}

/// `Folder::shortGuiLocalPath()`: the local path relative to the home
/// directory, without a trailing `/`.
pub fn short_gui_local_path(local_path: &str, home: &str) -> String {
    let home = format!("{}/", home.trim_end_matches('/'));
    let mut p = local_path.strip_prefix(&home).unwrap_or(local_path);
    if p.len() > 1
        && let Some(stripped) = p.strip_suffix('/')
    {
        p = stripped;
    }
    p.to_owned()
}

/// `ownCloudGui::slotComputeOverallSyncStatus()`: the tray icon and
/// tooltip for the accounts and folders of `snapshot`. `home` is the home
/// directory folder paths are shown relative to.
pub fn compute_overall_sync_status(
    snapshot: &Snapshot,
    style: TooltipStyle,
    home: &str,
) -> TrayState {
    let all_signed_out = snapshot.accounts.iter().all(|a| a.signed_out);
    let problem_accounts: Vec<_> = snapshot.accounts.iter().filter(|a| !a.connected).collect();
    let all_paused = snapshot.folders.iter().all(|f| f.paused);

    if !problem_accounts.is_empty() {
        let tooltip = match style {
            TooltipStyle::Windows => {
                let names: Vec<&str> = problem_accounts
                    .iter()
                    .map(|a| a.display_name.as_str())
                    .collect();
                format!("Disconnected from {}", names.join(", "))
            }
            TooltipStyle::Other => {
                let mut messages = vec!["Disconnected from accounts:".to_owned()];
                for a in &problem_accounts {
                    //: %1 is the account display name. %2 is the account connection status.
                    let mut message = format!("Account {}: {}", a.display_name, a.state_string);
                    if !a.errors.is_empty() {
                        message.push('\n');
                        message.push_str(&a.errors.join("\n"));
                    }
                    messages.push(message);
                }
                messages.join("\n\n")
            }
        };
        return TrayState {
            icon: StateIcon::Offline,
            tooltip,
        };
    }

    if all_signed_out {
        return TrayState {
            icon: StateIcon::Offline,
            tooltip: "Please sign in".to_owned(),
        };
    } else if all_paused {
        return TrayState {
            icon: sync_state_icon(SyncStatus::Paused),
            tooltip: "Account synchronization is disabled".to_owned(),
        };
    }

    // display the info of the least successful sync (eg. do not just
    // display the result of the latest sync)
    let folders: Vec<&FolderSnapshot> = snapshot.folders.iter().collect();
    let overall = tray_overall_status(&folders);

    // If the sync succeeded but there are unresolved conflicts, show the
    // problem icon!
    let mut icon_status = overall.status;
    if icon_status == SyncStatus::Success && overall.unresolved_conflicts {
        icon_status = SyncStatus::Problem;
    }
    // If we don't get a status for whatever reason, that's a Problem
    if icon_status == SyncStatus::Undefined {
        icon_status = SyncStatus::Problem;
    }

    let tooltip = if folders.is_empty() {
        "There are no sync folders configured.".to_owned()
    } else {
        match style {
            TooltipStyle::Windows => tray_tooltip_status_string(
                overall.status,
                overall.unresolved_conflicts,
                false,
                overall.progress,
            ),
            TooltipStyle::Other => folders
                .iter()
                .map(|f| {
                    let folder_message = tray_tooltip_status_string(
                        f.status,
                        f.unresolved_conflicts,
                        f.paused,
                        f.progress.as_ref(),
                    );
                    //: Example text: "Nextcloud: Syncing 25MB (3 minutes left)"
                    format!(
                        "{}: {folder_message}",
                        short_gui_local_path(&f.local_path, home)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    };
    TrayState {
        icon: sync_state_icon(icon_status),
        tooltip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_state::State;
    use crate::ui_event::AccountSnapshot;

    const HOME: &str = "/home/alice";

    fn account(id: &str, state: State) -> AccountSnapshot {
        AccountSnapshot {
            id: id.into(),
            url: "https://cloud.example.com".into(),
            user: format!("user{id}"),
            display_name: format!("user{id}@cloud.example.com"),
            state,
            state_string: state.state_string().into(),
            connected: state == State::Connected,
            signed_out: state == State::SignedOut,
            connection_status: "Undefined".into(),
            errors: Vec::new(),
            server_version: "34.0.0.0".into(),
        }
    }

    fn folder(alias: &str, status: SyncStatus) -> FolderSnapshot {
        FolderSnapshot {
            alias: alias.into(),
            account: "0".into(),
            local_path: format!("{HOME}/Nextcloud{alias}/"),
            remote_path: "/".into(),
            paused: false,
            syncing: false,
            scheduled: false,
            current: false,
            status,
            status_string: status.status_string().into(),
            last_status_time: None,
            errors: Vec::new(),
            consecutive_failing_syncs: 0,
            seconds_since_last_sync: 0,
            last_sync_duration_ms: 0,
            unresolved_conflicts: false,
            watcher: true,
            progress: None,
        }
    }

    fn paused(mut f: FolderSnapshot) -> FolderSnapshot {
        f.paused = true;
        f
    }

    fn conflicts(mut f: FolderSnapshot) -> FolderSnapshot {
        f.unresolved_conflicts = true;
        f
    }

    fn status_of(folders: &[FolderSnapshot]) -> (SyncStatus, bool) {
        let refs: Vec<&FolderSnapshot> = folders.iter().collect();
        let o = tray_overall_status(&refs);
        (o.status, o.unresolved_conflicts)
    }

    fn connected(folders: Vec<FolderSnapshot>) -> Snapshot {
        Snapshot {
            accounts: vec![account("0", State::Connected)],
            folders,
        }
    }

    // ---- trayOverallStatus, one folder ----

    #[test]
    fn derived_tray_overall_status_no_folder_is_undefined() {
        assert_eq!(status_of(&[]), (SyncStatus::Undefined, false));
    }

    #[test]
    fn derived_tray_overall_status_one_paused_folder_is_paused() {
        let f = paused(folder("1", SyncStatus::Success));
        assert_eq!(status_of(&[f]), (SyncStatus::Paused, false));
    }

    #[test]
    fn derived_tray_overall_status_one_undefined_folder_is_an_error() {
        assert_eq!(
            status_of(&[folder("1", SyncStatus::Undefined)]).0,
            SyncStatus::Error
        );
    }

    #[test]
    fn derived_tray_overall_status_one_problem_folder_shows_success() {
        assert_eq!(
            status_of(&[folder("1", SyncStatus::Problem)]).0,
            SyncStatus::Success
        );
    }

    #[test]
    fn derived_tray_overall_status_one_folder_keeps_its_other_states() {
        for s in [
            SyncStatus::NotYetStarted,
            SyncStatus::SyncPrepare,
            SyncStatus::SyncRunning,
            SyncStatus::SyncAbortRequested,
            SyncStatus::Success,
            SyncStatus::Error,
            SyncStatus::SetupError,
            SyncStatus::Paused,
        ] {
            assert_eq!(status_of(&[folder("1", s)]).0, s);
        }
    }

    #[test]
    fn derived_tray_overall_status_one_folder_reports_conflicts_and_progress() {
        let mut f = conflicts(folder("1", SyncStatus::SyncRunning));
        f.progress = Some(ProgressSummary::default());
        let o = tray_overall_status(&[&f]);
        assert!(o.unresolved_conflicts);
        assert!(o.progress.is_some());
    }

    // ---- trayOverallStatus, several folders, by priority ----

    #[test]
    fn derived_tray_overall_status_any_error_wins() {
        for e in [SyncStatus::Error, SyncStatus::SetupError] {
            let fs = [
                paused(folder("1", SyncStatus::Success)),
                folder("2", SyncStatus::SyncRunning),
                folder("3", e),
            ];
            assert_eq!(status_of(&fs).0, SyncStatus::Error);
        }
    }

    #[test]
    fn derived_tray_overall_status_stops_at_the_first_error() {
        // The folders after an error are not looked at: their conflicts
        // are not reported.
        let fs = [
            folder("1", SyncStatus::Error),
            conflicts(folder("2", SyncStatus::Success)),
        ];
        assert_eq!(status_of(&fs), (SyncStatus::Error, false));
        let fs = [
            conflicts(folder("1", SyncStatus::Success)),
            folder("2", SyncStatus::Error),
        ];
        assert_eq!(status_of(&fs), (SyncStatus::Error, true));
    }

    #[test]
    fn derived_tray_overall_status_paused_or_aborted_before_running() {
        for second in [
            paused(folder("2", SyncStatus::Success)),
            folder("2", SyncStatus::SyncAbortRequested),
            folder("2", SyncStatus::Paused),
        ] {
            let fs = [folder("1", SyncStatus::SyncRunning), second];
            assert_eq!(status_of(&fs).0, SyncStatus::Paused);
        }
    }

    #[test]
    fn derived_tray_overall_status_running_before_success() {
        for running in [SyncStatus::SyncRunning, SyncStatus::SyncPrepare] {
            let fs = [folder("1", SyncStatus::Success), folder("2", running)];
            assert_eq!(status_of(&fs).0, SyncStatus::SyncRunning);
        }
    }

    #[test]
    fn derived_tray_overall_status_success_and_problem_are_good() {
        let fs = [
            folder("1", SyncStatus::Success),
            folder("2", SyncStatus::Problem),
            folder("3", SyncStatus::NotYetStarted),
        ];
        assert_eq!(status_of(&fs).0, SyncStatus::Success);
    }

    #[test]
    fn derived_tray_overall_status_initial_states_are_not_yet_started() {
        let fs = [
            folder("1", SyncStatus::Undefined),
            folder("2", SyncStatus::NotYetStarted),
        ];
        assert_eq!(status_of(&fs).0, SyncStatus::NotYetStarted);
    }

    #[test]
    fn derived_tray_overall_status_collects_conflicts() {
        let fs = [
            folder("1", SyncStatus::Success),
            conflicts(paused(folder("2", SyncStatus::Success))),
        ];
        assert_eq!(status_of(&fs), (SyncStatus::Paused, true));
    }

    // ---- slotComputeOverallSyncStatus ----

    #[test]
    fn derived_overall_disconnected_accounts_are_listed() {
        let mut down = account("1", State::NetworkError);
        down.errors = vec!["Connection refused".into(), "Timeout".into()];
        let s = Snapshot {
            accounts: vec![
                account("0", State::Connected),
                down,
                account("2", State::SignedOut),
            ],
            folders: vec![folder("1", SyncStatus::Success)],
        };
        let t = compute_overall_sync_status(&s, TooltipStyle::Other, HOME);
        assert_eq!(t.icon, StateIcon::Offline);
        assert_eq!(
            t.tooltip,
            "Disconnected from accounts:\n\n\
             Account user1@cloud.example.com: Network error\nConnection refused\nTimeout\n\n\
             Account user2@cloud.example.com: Signed out"
        );
        let t = compute_overall_sync_status(&s, TooltipStyle::Windows, HOME);
        assert_eq!(t.icon, StateIcon::Offline);
        assert_eq!(
            t.tooltip,
            "Disconnected from user1@cloud.example.com, user2@cloud.example.com"
        );
    }

    #[test]
    fn derived_overall_no_account_asks_to_sign_in() {
        // Every signed-out account is also disconnected, so only an empty
        // account list reaches "Please sign in".
        let t = compute_overall_sync_status(&Snapshot::default(), TooltipStyle::Other, HOME);
        assert_eq!(
            t,
            TrayState {
                icon: StateIcon::Offline,
                tooltip: "Please sign in".into()
            }
        );
    }

    #[test]
    fn derived_overall_all_paused_disables_sync() {
        let s = connected(vec![
            paused(folder("1", SyncStatus::Success)),
            paused(folder("2", SyncStatus::Error)),
        ]);
        let t = compute_overall_sync_status(&s, TooltipStyle::Other, HOME);
        assert_eq!(t.icon, StateIcon::Pause);
        assert_eq!(t.tooltip, "Account synchronization is disabled");
    }

    #[test]
    fn derived_overall_no_folder_counts_as_all_paused() {
        // `allPaused` starts true and no folder clears it: "There are no
        // sync folders configured." is only reached by the File Provider
        // accounts upstream.
        let t = compute_overall_sync_status(&connected(Vec::new()), TooltipStyle::Other, HOME);
        assert_eq!(t.icon, StateIcon::Pause);
        assert_eq!(t.tooltip, "Account synchronization is disabled");
    }

    #[test]
    fn derived_overall_success_with_conflicts_shows_the_warning_icon() {
        let s = connected(vec![conflicts(folder("1", SyncStatus::Problem))]);
        let t = compute_overall_sync_status(&s, TooltipStyle::Other, HOME);
        assert_eq!(t.icon, StateIcon::Warning);
        assert_eq!(
            t.tooltip,
            "Nextcloud1: Sync finished with unresolved conflicts."
        );
        let t = compute_overall_sync_status(&s, TooltipStyle::Windows, HOME);
        assert_eq!(t.tooltip, "Sync finished with unresolved conflicts.");
    }

    #[test]
    fn derived_overall_undefined_shows_the_warning_icon() {
        // One Undefined folder is already an Error for trayOverallStatus,
        // and several folders always set one of its flags: without the
        // File Provider accounts the "Undefined is a Problem" rule only
        // applies to an empty folder list, which the all-paused branch
        // returns before. The rule's mapping is checked on its own.
        let s = connected(vec![folder("1", SyncStatus::Undefined)]);
        assert_eq!(
            compute_overall_sync_status(&s, TooltipStyle::Other, HOME).icon,
            StateIcon::Error
        );
        let refs: Vec<&FolderSnapshot> = Vec::new();
        let mut icon_status = tray_overall_status(&refs).status;
        if icon_status == SyncStatus::Undefined {
            icon_status = SyncStatus::Problem;
        }
        assert_eq!(sync_state_icon(icon_status), StateIcon::Warning);
    }

    #[test]
    fn derived_overall_tooltip_lists_every_folder() {
        let mut running = folder("2", SyncStatus::SyncRunning);
        running.progress = Some(ProgressSummary {
            status: ProgressPhase::Propagation,
            total_files: 7,
            current_file: 3,
            ..ProgressSummary::default()
        });
        let s = connected(vec![
            folder("1", SyncStatus::Success),
            running,
            paused(folder("3", SyncStatus::Success)),
        ]);
        let t = compute_overall_sync_status(&s, TooltipStyle::Other, HOME);
        // A paused folder among others: Paused, the pause icon.
        assert_eq!(t.icon, StateIcon::Pause);
        assert_eq!(
            t.tooltip,
            "Nextcloud1: Last sync was successful.\n\
             Nextcloud2: Syncing 3 of 7\n\
             Nextcloud3: Last sync was successful. (Sync is paused)"
        );
        // Windows: the overall status only, without the progress of
        // several folders.
        let t = compute_overall_sync_status(&s, TooltipStyle::Windows, HOME);
        assert_eq!(t.tooltip, "Sync is paused.");
    }

    #[test]
    fn derived_overall_one_running_folder() {
        let mut f = folder("1", SyncStatus::SyncRunning);
        f.progress = Some(ProgressSummary {
            status: ProgressPhase::Propagation,
            total_size: 3 * 1024 * 1024,
            ..ProgressSummary::default()
        });
        let s = connected(vec![f]);
        let t = compute_overall_sync_status(&s, TooltipStyle::Windows, HOME);
        assert_eq!(t.icon, StateIcon::Sync);
        assert_eq!(t.tooltip, "Syncing 3 MB");
    }

    // ---- trayTooltipStatusString ----

    #[test]
    fn derived_tooltip_status_strings() {
        let t = |s, c, p| tray_tooltip_status_string(s, c, p, None);
        assert_eq!(t(SyncStatus::Undefined, false, false), "Undefined state.");
        assert_eq!(
            t(SyncStatus::NotYetStarted, false, false),
            "Waiting to start syncing."
        );
        assert_eq!(
            t(SyncStatus::SyncPrepare, false, false),
            "Preparing for sync."
        );
        assert_eq!(t(SyncStatus::SyncRunning, false, false), "Sync is running.");
        assert_eq!(
            t(SyncStatus::Success, false, false),
            "Last sync was successful."
        );
        assert_eq!(
            t(SyncStatus::Problem, true, false),
            "Sync finished with unresolved conflicts."
        );
        assert_eq!(t(SyncStatus::Error, false, false), "");
        assert_eq!(t(SyncStatus::SetupError, false, false), "Setup error.");
        assert_eq!(
            t(SyncStatus::SyncAbortRequested, false, false),
            "Sync request was cancelled."
        );
        assert_eq!(t(SyncStatus::Paused, false, false), "Sync is paused.");
        assert_eq!(
            t(SyncStatus::Error, false, true),
            " (Sync is paused)",
            "an error has no message"
        );
    }

    #[test]
    fn derived_tooltip_progress_strings() {
        let p = |total_size, eta_msecs| ProgressSummary {
            status: ProgressPhase::Propagation,
            total_files: 2,
            current_file: 5,
            total_size,
            eta_msecs,
            ..ProgressSummary::default()
        };
        let t = |p: ProgressSummary| {
            tray_tooltip_status_string(SyncStatus::SyncRunning, false, false, Some(&p))
        };
        // totalFileCount = max(totalFiles, currentFile)
        assert_eq!(t(p(0, None)), "Syncing 5 of 5");
        assert_eq!(t(p(0, Some(0))), "Syncing 5 of 5 (A few seconds left)");
        assert_eq!(t(p(0, Some(120_000))), "Syncing 5 of 5 (2 minute(s) left)");
        assert_eq!(t(p(2048, None)), "Syncing 2 KB");
        assert_eq!(t(p(2048, Some(0))), "Syncing 2 KB (A few seconds left)");
        assert_eq!(t(p(2048, Some(3_000))), "Syncing 2 KB (3 second(s) left)");
        // Outside the propagation: no numbers.
        let mut discovery = p(0, None);
        discovery.status = ProgressPhase::Discovery;
        assert_eq!(t(discovery), "Sync is running.");
    }

    #[test]
    fn derived_sync_state_icons() {
        use SyncStatus as S;
        let expected = [
            (S::Undefined, StateIcon::Warning),
            (S::NotYetStarted, StateIcon::Sync),
            (S::SyncRunning, StateIcon::Sync),
            (S::SyncAbortRequested, StateIcon::Pause),
            (S::Paused, StateIcon::Pause),
            (S::SyncPrepare, StateIcon::Ok),
            (S::Success, StateIcon::Ok),
            (S::Problem, StateIcon::Warning),
            (S::Error, StateIcon::Error),
            (S::SetupError, StateIcon::Error),
        ];
        for (s, icon) in expected {
            assert_eq!(sync_state_icon(s), icon, "{s:?}");
        }
    }

    #[test]
    fn derived_short_gui_local_path() {
        assert_eq!(
            short_gui_local_path("/home/alice/Nextcloud/", HOME),
            "Nextcloud"
        );
        assert_eq!(short_gui_local_path("/srv/data/", HOME), "/srv/data");
        assert_eq!(short_gui_local_path("/", HOME), "/");
        assert_eq!(
            short_gui_local_path("/home/alicea/x/", HOME),
            "/home/alicea/x"
        );
    }
}
