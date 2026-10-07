// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! Takeover and hand-back of a folder between the official client's
//! `nextcloud.cfg` and ours.
//!
//! **Takeover** moves one folder definition (and its account, when ours has
//! no account for the same server and login name) from the official
//! configuration into ours. The local path and the journal path are kept
//! untouched: both clients use the same `.sync_*.db` journal, so the next
//! sync continues where the official client stopped. The folder group of
//! the official file is copied verbatim into a backup group of ours,
//! `[ncsyncdTakeover]`, then the folder is removed from the official file
//! (upstream has no "disabled" flag that keeps a folder from syncing other
//! than `paused`, and a paused folder still shows up and can be resumed by a
//! click: removal is the reliable way).
//!
//! **Hand-back** restores the backup verbatim in the official file (same
//! account id, group and alias when still free) and removes the folder from
//! ours, and our account too when the takeover created it and it has no
//! folder left.
//!
//! Both refuse while the official client runs: it rewrites `nextcloud.cfg`
//! from memory when it exits or saves, which would undo the change. It is
//! detected by scanning `/proc` for processes of the configuration owner
//! whose `comm`, executable name or `argv[0]` name is `nextcloud`, or whose
//! `argv[0]` is a `Nextcloud*.AppImage` (the AppImage runtime then runs
//! `usr/bin/nextcloud`, whose `comm` is `nextcloud` too; a Flatpak runs
//! `/app/bin/nextcloud`). Its single-instance socket (KDSingleApplication)
//! is not used: its name depends on the session and it can be stale.
//!
//! The functions over two [`Settings`] are pure; [`take_over_files`] and
//! [`hand_back_files`] add the files, locks and process check.

use std::path::{Path, PathBuf};

use crate::account_config::{
    ACCOUNTS_GROUP, AccountDefinition, MAX_ACCOUNTS_VERSION, generate_free_account_id,
    load_account, load_accounts,
};
use crate::folder_definition::{
    FolderDefinition, FolderGroup, all_aliases, allocate_alias, clean_path, escape_alias,
    folder_group, load_folders, remove_folder, unescape_alias,
};
use crate::settings::{LockFile, Settings, SettingsError, join_key};

/// Our group holding the verbatim backups of taken-over folders, one
/// subgroup per folder (our escaped alias).
pub const TAKEOVER_GROUP: &str = "ncsyncdTakeover";

/// Errors of takeover and hand-back.
#[derive(Debug, thiserror::Error)]
pub enum TakeoverError {
    #[error(
        "the official Nextcloud client is running (pid {}): quit it first (it rewrites its configuration when it exits)",
        .0.iter().map(|p| p.pid.to_string()).collect::<Vec<_>>().join(", ")
    )]
    OfficialClientRunning(Vec<RunningProcess>),
    #[error("no folder {0:?} in the official client's configuration")]
    NotFound(String),
    #[error("{0:?} matches several folders of the official client: {1}; give the alias")]
    Ambiguous(String, String),
    #[error("{0}")]
    Unsupported(String),
    #[error("our configuration already has a folder for {0}")]
    AlreadyOurs(String),
    #[error("no taken-over folder {0:?} in our configuration")]
    NoBackup(String),
    #[error(
        "the official client's account {0} (or one with the same server and user) no longer exists; add the account in the official client first"
    )]
    OfficialAccountGone(String),
    #[error("the official client already has a folder for {0}")]
    OfficialHasFolder(String),
    #[error(transparent)]
    Settings(#[from] SettingsError),
}

/// A running official client process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningProcess {
    pub pid: u32,
    pub name: String,
}

/// What the takeover did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TakeoverOutcome {
    /// The folder's alias in our configuration.
    pub alias: String,
    pub account_id: String,
    /// Our account was created from the official one (it needs credentials).
    pub created_account: bool,
    /// The official account (with its own id), to look its app password up.
    pub official_account: AccountDefinition,
    pub folder: FolderDefinition,
}

/// What the hand-back did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandbackOutcome {
    /// The folder's alias in the official configuration.
    pub official_alias: String,
    pub official_account_id: String,
    pub folder: FolderDefinition,
    /// Our account, removed because the takeover created it and it has no
    /// folder left (the caller forgets its credentials).
    pub removed_account: Option<AccountDefinition>,
}

/// A folder found in a configuration.
#[derive(Clone, Debug)]
struct Found {
    account_id: String,
    group: FolderGroup,
    escaped_alias: String,
    definition: FolderDefinition,
}

fn same_path(a: &str, b: &str) -> bool {
    let canon = |p: &str| {
        std::fs::canonicalize(p)
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|_| clean_path(p))
    };
    clean_path(a) == clean_path(b) || canon(a) == canon(b)
}

fn all_folders(settings: &Settings) -> Vec<Found> {
    let accounts = load_accounts(settings).accounts;
    load_folders(settings, &accounts)
        .folders
        .into_iter()
        .map(|f| Found {
            account_id: f.account_id,
            group: f.group,
            escaped_alias: f.escaped_alias,
            definition: f.definition,
        })
        .collect()
}

fn select(settings: &Settings, selector: &str) -> Result<Found, TakeoverError> {
    let folders = all_folders(settings);
    let by_alias: Vec<&Found> = folders
        .iter()
        .filter(|f| f.definition.alias == selector)
        .collect();
    let looks_like_path = selector.starts_with('/') || selector.starts_with('.');
    let candidates: Vec<&Found> = if !by_alias.is_empty() && !looks_like_path {
        by_alias
    } else {
        let wanted = std::path::absolute(selector)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| selector.to_owned());
        folders
            .iter()
            .filter(|f| same_path(&f.definition.local_path, &wanted))
            .collect()
    };
    match candidates.as_slice() {
        [] => Err(TakeoverError::NotFound(selector.to_owned())),
        [one] => Ok((*one).clone()),
        many => Err(TakeoverError::Ambiguous(
            selector.to_owned(),
            many.iter()
                .map(|f| format!("{} (account {})", f.definition.alias, f.account_id))
                .collect::<Vec<_>>()
                .join(", "),
        )),
    }
}

/// Our account for the same server and login name as `official`, or a new
/// copy of the official account group (keeping the official id when it is
/// free in ours).
fn ensure_account(
    official: &Settings,
    official_account: &AccountDefinition,
    ours: &mut Settings,
) -> (String, bool) {
    let our_accounts = load_accounts(ours);
    if let Some(a) = our_accounts.accounts.iter().find(|a| {
        a.url_string() == official_account.url_string()
            && a.credentials_user() == official_account.credentials_user()
    }) {
        return (a.id.clone(), false);
    }
    let used = ours.child_groups(ACCOUNTS_GROUP);
    let id = if used.contains(&official_account.id)
        || our_accounts.blocked_ids.contains(&official_account.id)
    {
        generate_free_account_id(ours, &our_accounts.blocked_ids)
    } else {
        official_account.id.clone()
    };
    let from = join_key(ACCOUNTS_GROUP, &official_account.id);
    let to = join_key(ACCOUNTS_GROUP, &id);
    for key in official.all_keys(&from) {
        let first = key.split('/').next().unwrap_or_default();
        if FolderGroup::from_name(first).is_some() {
            continue;
        }
        if let Some(raw) = official.raw_value(&join_key(&from, &key)) {
            ours.set_raw_value(&join_key(&to, &key), &raw);
        }
    }
    if !ours.contains(&join_key(ACCOUNTS_GROUP, "version")) {
        ours.set_value(&join_key(ACCOUNTS_GROUP, "version"), MAX_ACCOUNTS_VERSION);
    }
    (id, true)
}

/// Takeover over two loaded configurations (see the module documentation).
/// `selector` is a folder alias of the official configuration or a local
/// path; `official_file` is recorded in the backup.
pub fn take_over(
    official: &mut Settings,
    ours: &mut Settings,
    selector: &str,
    official_file: &Path,
) -> Result<TakeoverOutcome, TakeoverError> {
    let found = select(official, selector)?;
    found
        .definition
        .check_supported()
        .map_err(|e| TakeoverError::Unsupported(e.to_string()))?;
    if let Some(f) = all_folders(ours)
        .into_iter()
        .find(|f| same_path(&f.definition.local_path, &found.definition.local_path))
    {
        return Err(TakeoverError::AlreadyOurs(format!(
            "{} (alias {})",
            f.definition.local_path, f.definition.alias
        )));
    }
    let official_account = load_account(official, &found.account_id)
        .ok_or_else(|| TakeoverError::NotFound(selector.to_owned()))?;
    let (account_id, created_account) = ensure_account(official, &official_account, ours);

    let alias = allocate_alias(&found.definition.alias, &all_aliases(ours), &[]);
    let escaped = escape_alias(&alias);
    let from = folder_group(&found.account_id, found.group, &found.escaped_alias);
    let to = folder_group(&account_id, found.group, &escaped);
    official.copy_group_raw(&from, ours, &to);

    // The verbatim backup, for a lossless hand-back.
    let backup = join_key(TAKEOVER_GROUP, &escaped);
    ours.remove(&backup);
    let b = |k: &str| join_key(&backup, k);
    ours.set_value(&b("source"), official_file.to_string_lossy().as_ref());
    ours.set_value(&b("accountId"), &found.account_id);
    ours.set_value(&b("folderGroup"), found.group.name());
    ours.set_value(&b("alias"), &found.escaped_alias);
    ours.set_value(&b("ourAccountId"), &account_id);
    ours.set_value(&b("createdAccount"), created_account);
    official.copy_group_raw(&from, ours, &b("folder"));

    remove_folder(official, &found.account_id, &found.escaped_alias);

    let mut folder = found.definition;
    folder.alias = alias.clone();
    Ok(TakeoverOutcome {
        alias,
        account_id,
        created_account,
        official_account,
        folder,
    })
}

/// The aliases of our taken-over folders (with a backup).
pub fn taken_over_aliases(ours: &Settings) -> Vec<String> {
    ours.child_groups(TAKEOVER_GROUP)
        .iter()
        .map(|a| unescape_alias(a))
        .collect()
}

/// Hand-back over two loaded configurations (see the module documentation).
pub fn hand_back(
    ours: &mut Settings,
    official: &mut Settings,
    alias: &str,
) -> Result<HandbackOutcome, TakeoverError> {
    let escaped = escape_alias(alias);
    let backup = join_key(TAKEOVER_GROUP, &escaped);
    let b = |k: &str| join_key(&backup, k);
    if ours.all_keys(&b("folder")).is_empty() {
        return Err(TakeoverError::NoBackup(alias.to_owned()));
    }
    let official_id = ours.string(&b("accountId"));
    let our_id = ours.string(&b("ourAccountId"));
    let group =
        FolderGroup::from_name(&ours.string(&b("folderGroup"))).unwrap_or(FolderGroup::Folders);
    let official_escaped = ours.string(&b("alias"));
    let created_account = ours.bool_or(&b("createdAccount"), false);
    let folder = FolderDefinition::load(ours, &b("folder"), &official_escaped);

    // The official account must still be there.
    let our_account = load_account(ours, &our_id);
    let official_account = load_account(official, &official_id);
    let same = match (&our_account, &official_account) {
        (Some(o), Some(f)) => {
            o.url_string() == f.url_string() && o.credentials_user() == f.credentials_user()
        }
        (None, Some(_)) => true,
        _ => false,
    };
    if !same {
        return Err(TakeoverError::OfficialAccountGone(official_id));
    }
    if let Some(f) = all_folders(official)
        .into_iter()
        .find(|f| same_path(&f.definition.local_path, &folder.local_path))
    {
        return Err(TakeoverError::OfficialHasFolder(format!(
            "{} (alias {})",
            f.definition.local_path, f.definition.alias
        )));
    }
    let wanted = unescape_alias(&official_escaped);
    let official_alias = allocate_alias(&wanted, &all_aliases(official), &[]);
    let official_group_name = if official_alias == wanted {
        official_escaped.clone()
    } else {
        escape_alias(&official_alias)
    };
    let to = folder_group(&official_id, group, &official_group_name);
    ours.copy_group_raw(&b("folder"), official, &to);

    remove_folder(ours, &our_id, &escaped);
    ours.remove(&backup);
    let mut removed_account = None;
    if created_account {
        let account_group = join_key(ACCOUNTS_GROUP, &our_id);
        let has_folders = FolderGroup::ALL.iter().any(|g| {
            !ours
                .child_groups(&join_key(&account_group, g.name()))
                .is_empty()
        });
        if !has_folders {
            removed_account = our_account;
            ours.remove(&account_group);
        }
    }
    let mut folder = folder;
    folder.alias = official_alias.clone();
    Ok(HandbackOutcome {
        official_alias,
        official_account_id: official_id,
        folder,
        removed_account,
    })
}

// ---------------------------------------------------------------------------
// Official client detection
// ---------------------------------------------------------------------------

fn file_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Whether a process looks like the official desktop client.
fn is_official_client(comm: &str, exe: Option<&str>, argv0: &str) -> bool {
    let argv0_name = file_name(argv0);
    let lower = argv0_name.to_ascii_lowercase();
    comm == "nextcloud"
        || exe.is_some_and(|e| file_name(e) == "nextcloud")
        || argv0_name == "nextcloud"
        || (lower.starts_with("nextcloud") && lower.ends_with(".appimage"))
}

/// The official client's processes owned by `uid` (all users when `None`).
pub fn official_client_processes(uid: Option<u32>) -> Vec<RunningProcess> {
    use std::os::unix::fs::MetadataExt as _;
    let me = std::process::id();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.filter_map(Result::ok) {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let dir = e.path();
        if let Some(uid) = uid
            && std::fs::metadata(&dir).map(|m| m.uid()).ok() != Some(uid)
        {
            continue;
        }
        let comm = std::fs::read_to_string(dir.join("comm"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        let exe = std::fs::read_link(dir.join("exe"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        let argv0 = std::fs::read(dir.join("cmdline"))
            .ok()
            .and_then(|c| {
                c.split(|b| *b == 0)
                    .next()
                    .map(|a| String::from_utf8_lossy(a).into_owned())
            })
            .unwrap_or_default();
        if is_official_client(&comm, exe.as_deref(), &argv0) {
            out.push(RunningProcess { pid, name: comm });
        }
    }
    out
}

fn ensure_official_client_stopped(official_file: &Path) -> Result<(), TakeoverError> {
    use std::os::unix::fs::MetadataExt as _;
    let uid = std::fs::metadata(official_file).ok().map(|m| m.uid());
    let running = official_client_processes(uid);
    if running.is_empty() {
        Ok(())
    } else {
        Err(TakeoverError::OfficialClientRunning(running))
    }
}

/// A copy of the official file as it was before our last change.
fn backup_path(official_file: &Path) -> PathBuf {
    let mut p = official_file.as_os_str().to_owned();
    p.push(".ncsyncd-bak");
    PathBuf::from(p)
}

/// [`take_over`] on files: refuses while the official client runs, locks
/// both files, writes ours (with the backup) first, then the official one
/// (a verbatim copy of it is kept as `<file>.ncsyncd-bak`); ours is rolled
/// back if the official one cannot be written.
pub fn take_over_files(
    official_file: &Path,
    our_file: &Path,
    selector: &str,
) -> Result<TakeoverOutcome, TakeoverError> {
    take_over_files_checked(
        official_file,
        our_file,
        selector,
        ensure_official_client_stopped,
    )
}

type ProcessCheck = fn(&Path) -> Result<(), TakeoverError>;

fn take_over_files_checked(
    official_file: &Path,
    our_file: &Path,
    selector: &str,
    check: ProcessCheck,
) -> Result<TakeoverOutcome, TakeoverError> {
    check(official_file)?;
    if !official_file.exists() {
        return Err(TakeoverError::NotFound(format!(
            "{selector} ({} does not exist)",
            official_file.display()
        )));
    }
    let _official_lock = LockFile::acquire(official_file)?;
    let _our_lock = LockFile::acquire(our_file)?;
    let mut official = Settings::load(official_file)?;
    let mut ours = Settings::load(our_file)?;
    let ours_before = ours.clone();
    let outcome = take_over(&mut official, &mut ours, selector, official_file)?;
    ours.write_locked(our_file)?;
    let _ = std::fs::copy(official_file, backup_path(official_file));
    if let Err(e) = official.write_locked(official_file) {
        let _ = ours_before.write_locked(our_file);
        return Err(e.into());
    }
    Ok(outcome)
}

/// [`hand_back`] on files: refuses while the official client runs, writes
/// the official file first, then ours; the official one is rolled back if
/// ours cannot be written.
pub fn hand_back_files(
    our_file: &Path,
    official_file: &Path,
    alias: &str,
) -> Result<HandbackOutcome, TakeoverError> {
    hand_back_files_checked(
        our_file,
        official_file,
        alias,
        ensure_official_client_stopped,
    )
}

fn hand_back_files_checked(
    our_file: &Path,
    official_file: &Path,
    alias: &str,
    check: ProcessCheck,
) -> Result<HandbackOutcome, TakeoverError> {
    check(official_file)?;
    let _official_lock = LockFile::acquire(official_file)?;
    let _our_lock = LockFile::acquire(our_file)?;
    let mut official = Settings::load(official_file)?;
    let mut ours = Settings::load(our_file)?;
    let official_before = official.clone();
    let outcome = hand_back(&mut ours, &mut official, alias)?;
    if official_file.exists() {
        let _ = std::fs::copy(official_file, backup_path(official_file));
    }
    official.write_locked(official_file)?;
    if let Err(e) = ours.write_locked(our_file) {
        let _ = official_before.write_locked(official_file);
        return Err(e.into());
    }
    Ok(outcome)
}

/// Where the hand-back of a taken-over folder goes (the `source` recorded
/// at takeover time).
pub fn handback_source(ours: &Settings, alias: &str) -> Option<PathBuf> {
    let s = ours.string(&join_key(
        &join_key(TAKEOVER_GROUP, &escape_alias(alias)),
        "source",
    ));
    (!s.is_empty()).then(|| PathBuf::from(s))
}

#[cfg(test)]
mod tests;
