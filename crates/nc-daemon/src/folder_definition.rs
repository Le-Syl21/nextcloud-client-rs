// SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of upstream `FolderDefinition` (`src/gui/folder.cpp`, `folder.h`),
// `Folder::saveToSettings`/`removeFromSettings`, and the configuration parts
// of `src/gui/folderman.cpp` (`setupFolders`, `setupFoldersHelper`,
// `backwardMigrationSettingsKeys`, `escapeAlias`/`unescapeAlias`, the alias
// allocation of `addFolderInternal`, `checkPathValidityForNewFolder`,
// `findGoodPathForNewSyncFolder`) (nextcloud/desktop v34.0.5).

//! Folder definitions: `[Accounts] <id>\Folders\<alias>\...` (and the
//! `Multifolders` and `FoldersWithPlaceholders` groups), as data.

use std::path::Path;

use crate::account_config::{ACCOUNTS_GROUP, AccountDefinition};
use crate::settings::{Settings, join_key};

/// The three groups folders are saved in (see `Folder::saveToSettings`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FolderGroup {
    /// `Folders`: read by old clients; used when the local path is synced
    /// by one account only.
    Folders,
    /// `Multifolders`: the same local path is used by several accounts.
    Multifolders,
    /// `FoldersWithPlaceholders`: virtual files are (or were) enabled.
    FoldersWithPlaceholders,
}

impl FolderGroup {
    pub const ALL: [FolderGroup; 3] = [
        FolderGroup::Folders,
        FolderGroup::Multifolders,
        FolderGroup::FoldersWithPlaceholders,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Folders => "Folders",
            Self::Multifolders => "Multifolders",
            Self::FoldersWithPlaceholders => "FoldersWithPlaceholders",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|g| g.name() == name)
    }
}

/// `Vfs::Mode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VfsMode {
    #[default]
    Off,
    WithSuffix,
    WindowsCfApi,
    XAttr,
}

impl VfsMode {
    /// `Vfs::modeToString`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::WithSuffix => "suffix",
            Self::WindowsCfApi => "wincfapi",
            Self::XAttr => "xattr",
        }
    }

    /// `Vfs::modeFromString` (which does not know `xattr`).
    pub fn from_mode_string(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "suffix" => Some(Self::WithSuffix),
            "wincfapi" => Some(Self::WindowsCfApi),
            _ => None,
        }
    }
}

/// `FolderDefinition`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FolderDefinition {
    /// The name of the folder (unescaped).
    pub alias: String,
    /// Path on the local machine, always with a trailing `/`.
    pub local_path: String,
    /// Path to the journal, usually relative to `local_path`.
    pub journal_path: String,
    /// Path on the remote (no trailing `/`, except `/`).
    pub target_path: String,
    pub paused: bool,
    pub ignore_hidden_files: bool,
    pub virtual_files_mode: VfsMode,
    /// The settings asked for an upgrade of the vfs mode (`usePlaceholders`).
    pub upgrade_vfs_mode: bool,
}

/// Errors about folders.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FolderError {
    #[error(
        "folder {alias:?} ({path}) uses virtual files (mode {mode}); ncsyncd only syncs folders without virtual files"
    )]
    VirtualFilesNotSupported {
        alias: String,
        path: String,
        mode: &'static str,
    },
}

const VERSION: &str = "version";
/// `maxFoldersVersion`: the highest version of a folder group this client reads.
pub const MAX_FOLDERS_VERSION: i64 = 1;
/// `FolderDefinition::maxSettingsVersion()`.
pub const MAX_SETTINGS_VERSION: i64 = 13;

const TAGS: [(char, &str); 12] = [
    ('/', "__SLASH__"),
    ('\\', "__BSLASH__"),
    ('?', "__QMARK__"),
    ('%', "__PERCENT__"),
    ('*', "__STAR__"),
    (':', "__COLON__"),
    ('|', "__PIPE__"),
    ('"', "__QUOTE__"),
    ('<', "__LESS_THAN__"),
    ('>', "__GREATER_THAN__"),
    ('[', "__PAR_OPEN__"),
    (']', "__PAR_CLOSE__"),
];

/// `FolderMan::escapeAlias`.
pub fn escape_alias(alias: &str) -> String {
    let mut a = alias.to_owned();
    for (c, tag) in TAGS {
        a = a.replace(c, tag);
    }
    a
}

/// `FolderMan::unescapeAlias`.
pub fn unescape_alias(alias: &str) -> String {
    let mut a = alias.to_owned();
    for (c, tag) in TAGS {
        a = a.replace(tag, &c.to_string());
    }
    a
}

/// `Utility::trailingSlashPath`.
fn trailing_slash_path(path: &str) -> String {
    if path.ends_with('/') {
        path.to_owned()
    } else {
        format!("{path}/")
    }
}

impl FolderDefinition {
    /// `FolderDefinition::prepareLocalPath`: `/` separators and a trailing `/`.
    pub fn prepare_local_path(path: &str) -> String {
        trailing_slash_path(path)
    }

    /// `FolderDefinition::prepareTargetPath`: remove the ending `/`, then
    /// ensure a starting one: `/foo/bar` and `/`.
    pub fn prepare_target_path(path: &str) -> String {
        let p = path.strip_suffix('/').unwrap_or(path);
        if p.starts_with('/') {
            p.to_owned()
        } else {
            format!("/{p}")
        }
    }

    /// `absoluteJournalPath()`: the journal path relative to the local path
    /// (`QDir(localPath).filePath(journalPath)`).
    pub fn absolute_journal_path(&self) -> String {
        if self.journal_path.starts_with('/') {
            return self.journal_path.clone();
        }
        if self.journal_path.is_empty() {
            return self.local_path.trim_end_matches('/').to_owned();
        }
        format!(
            "{}{}",
            trailing_slash_path(&self.local_path),
            self.journal_path
        )
    }

    /// `defaultJournalPath(account)`: `.sync_<hash>.db` from the account URL,
    /// the remote path and the login name (`SyncJournalDb::makeDbName`).
    pub fn default_journal_path(&self, account: &AccountDefinition) -> String {
        nc_journal::journal::make_db_name(
            Path::new(&self.local_path),
            &account.url_string(),
            &self.target_path,
            &account.credentials_user(),
        )
    }

    /// The daemon syncs folders without virtual files only.
    pub fn check_supported(&self) -> Result<(), FolderError> {
        if self.virtual_files_mode != VfsMode::Off {
            return Err(FolderError::VirtualFilesNotSupported {
                alias: self.alias.clone(),
                path: self.local_path.clone(),
                mode: self.virtual_files_mode.as_str(),
            });
        }
        Ok(())
    }

    /// `FolderDefinition::save` into `group` (the folder's own group).
    pub fn save(&self, settings: &mut Settings, group: &str) {
        let key = |k: &str| join_key(group, k);
        settings.set_value(&key("localPath"), &self.local_path);
        settings.set_value(&key("journalPath"), &self.journal_path);
        settings.set_value(&key("targetPath"), &self.target_path);
        settings.set_value(&key("paused"), self.paused);
        settings.set_value(&key("ignoreHiddenFiles"), self.ignore_hidden_files);
        settings.set_value(&key("virtualFilesMode"), self.virtual_files_mode.as_str());
        // Ensure new vfs modes won't be attempted by older clients
        let version = if self.virtual_files_mode == VfsMode::WindowsCfApi {
            3
        } else {
            2
        };
        settings.set_value(&key(VERSION), version);
        // Windows / macOS only.
        settings.remove(&key("navigationPaneClsid"));
        settings.remove(&key("securityScopedBookmarkData"));
    }

    /// `FolderDefinition::load` from `group`; `escaped_alias` is the group
    /// name.
    pub fn load(settings: &Settings, group: &str, escaped_alias: &str) -> Self {
        let key = |k: &str| join_key(group, k);
        let mut virtual_files_mode = VfsMode::Off;
        let mut upgrade_vfs_mode = false;
        let vfs = settings.string(&key("virtualFilesMode"));
        if !vfs.is_empty() {
            match VfsMode::from_mode_string(&vfs) {
                Some(m) => virtual_files_mode = m,
                None => {
                    log::warn!(target: "nextcloud.gui.folder", "Unknown virtualFilesMode: {vfs} assuming 'off'");
                }
            }
        } else if settings.bool_or(&key("usePlaceholders"), false) {
            virtual_files_mode = VfsMode::WithSuffix;
            upgrade_vfs_mode = true; // maybe winvfs is available?
        }
        Self {
            alias: unescape_alias(escaped_alias),
            // Old settings can contain paths with native separators; on
            // Linux they already are `/`.
            local_path: Self::prepare_local_path(&settings.string(&key("localPath"))),
            journal_path: settings.string(&key("journalPath")),
            target_path: Self::prepare_target_path(&settings.string(&key("targetPath"))),
            paused: settings.bool_or(&key("paused"), false),
            ignore_hidden_files: settings.bool_or(&key("ignoreHiddenFiles"), true),
            virtual_files_mode,
            upgrade_vfs_mode,
        }
    }
}

fn account_group(id: &str) -> String {
    join_key(ACCOUNTS_GROUP, id)
}

/// The settings group of a folder.
pub fn folder_group(account_id: &str, group: FolderGroup, escaped_alias: &str) -> String {
    join_key(
        &join_key(&account_group(account_id), group.name()),
        escaped_alias,
    )
}

/// `FolderMan::backwardMigrationSettingsKeys`: folder groups and folders
/// too new for this client, as full group paths.
pub fn backward_migration_settings_keys(settings: &Settings) -> (Vec<String>, Vec<String>) {
    let mut delete_keys = Vec::new();
    let mut ignore_keys = Vec::new();
    for account_id in settings.child_groups(ACCOUNTS_GROUP) {
        for g in FolderGroup::ALL {
            let group = join_key(&account_group(&account_id), g.name());
            let folders_version = settings.i64_or(&join_key(&group, VERSION), 1);
            if folders_version <= MAX_FOLDERS_VERSION {
                for alias in settings.child_groups(&group) {
                    let fg = join_key(&group, &alias);
                    let v = settings.i64_or(&join_key(&fg, VERSION), 1);
                    if v > MAX_SETTINGS_VERSION {
                        log::info!(target: "nextcloud.gui.folder.manager", "Ignoring folder: {alias} version: {v}");
                        ignore_keys.push(fg);
                    }
                }
            } else {
                log::info!(target: "nextcloud.gui.folder.manager", "Ignoring group: {} version: {folders_version}", g.name());
                delete_keys.push(group);
            }
        }
    }
    (delete_keys, ignore_keys)
}

/// A journal migration `setupFoldersHelper` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalMigration {
    None,
    /// "Migration #2": the journal was at this absolute path (3.0 era); it
    /// has to be moved next to the files (see [`move_absolute_journal`]) and
    /// the folder saved.
    FromAbsolutePath(String),
    /// The folder comes from `Folders`: a legacy `.csync_journal.db` may
    /// have to be renamed (`SyncJournalDb::maybeMigrateDb`).
    MaybeMigrateLegacyDb,
}

/// A folder as `setupFoldersHelper` would add it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedFolder {
    pub account_id: String,
    pub group: FolderGroup,
    /// The settings group name (escaped alias).
    pub escaped_alias: String,
    pub definition: FolderDefinition,
    /// `setSaveBackwardsCompatible(true)`: it was in `Folders`.
    pub save_backwards_compatible: bool,
    /// `setSaveInFoldersWithPlaceholders()`.
    pub save_in_folders_with_placeholders: bool,
    pub journal_migration: JournalMigration,
    /// The definition changed and should be saved (migration #2).
    pub needs_save: bool,
}

/// What [`load_folders`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FoldersLoad {
    pub folders: Vec<LoadedFolder>,
    /// Folders too new for this client (`_additionalBlockedFolderAliases`).
    pub blocked_aliases: Vec<String>,
}

/// `FolderMan::setupFolders` (data part) for the given accounts: reads
/// `Folders` (backwards compatible), then `Multifolders`, then
/// `FoldersWithPlaceholders` of every account that has settings, skipping
/// what is too new. The returned folders still have to be checked with
/// [`FolderDefinition::check_supported`].
pub fn load_folders(settings: &Settings, accounts: &[AccountDefinition]) -> FoldersLoad {
    let (mut skip, ignore) = backward_migration_settings_keys(settings);
    skip.extend(ignore);
    let with_settings = settings.child_groups(ACCOUNTS_GROUP);
    let mut out = FoldersLoad::default();
    for account in accounts {
        if !with_settings.contains(&account.id) {
            continue;
        }
        for (g, backwards_compatible, with_placeholders) in [
            (FolderGroup::Folders, true, false),
            (FolderGroup::Multifolders, false, false),
            (FolderGroup::FoldersWithPlaceholders, false, true),
        ] {
            let group = join_key(&account_group(&account.id), g.name());
            if skip.contains(&group) {
                log::warn!(target: "nextcloud.gui.folder.manager", "Folder structure {} is too new, ignoring", g.name());
                continue;
            }
            load_folders_helper(
                settings,
                account,
                g,
                &skip,
                backwards_compatible,
                with_placeholders,
                &mut out,
            );
        }
    }
    out
}

/// `FolderMan::setupFoldersHelper` (data part).
fn load_folders_helper(
    settings: &Settings,
    account: &AccountDefinition,
    g: FolderGroup,
    ignore_keys: &[String],
    backwards_compatible: bool,
    folders_with_placeholders: bool,
    out: &mut FoldersLoad,
) {
    let group = join_key(&account_group(&account.id), g.name());
    for escaped_alias in settings.child_groups(&group) {
        let fg = join_key(&group, &escaped_alias);
        if ignore_keys.contains(&fg) {
            log::info!(target: "nextcloud.gui.folder.manager", "Folder {escaped_alias} is too new, ignoring");
            out.blocked_aliases.push(escaped_alias);
            continue;
        }
        let mut def = FolderDefinition::load(settings, &fg, &escaped_alias);
        // Upstream computes the default journal name for every folder; it is
        // only used by the migrations below, and computing it probes the
        // folder with a test file, so it is computed only when needed.
        let default_journal_path = |d: &FolderDefinition| d.default_journal_path(account);
        // Migration: Old settings don't have journalPath
        if def.journal_path.is_empty() {
            def.journal_path = default_journal_path(&def);
        }
        // Migration #2: journalPath might be absolute (in DataAppDir most
        // likely): move it back to the root of the local tree.
        if !def.journal_path.starts_with('.') {
            let new = default_journal_path(&def);
            let old = std::mem::replace(&mut def.journal_path, new);
            out.folders.push(LoadedFolder {
                account_id: account.id.clone(),
                group: g,
                escaped_alias,
                definition: def,
                save_backwards_compatible: false,
                save_in_folders_with_placeholders: false,
                journal_migration: JournalMigration::FromAbsolutePath(old),
                needs_save: true,
            });
            continue;
        }
        // Migration: ._ files sometimes can't be created. So if the
        // configured journalPath has a dot-underscore ("._sync_*.db") but
        // the current default doesn't have the underscore, switch to the new
        // default if no db exists yet.
        if def.journal_path.starts_with("._sync_")
            && !Path::new(&def.absolute_journal_path()).exists()
        {
            let new = default_journal_path(&def);
            if new.starts_with(".sync_") {
                def.journal_path = new;
            }
        }
        out.folders.push(LoadedFolder {
            account_id: account.id.clone(),
            group: g,
            escaped_alias,
            definition: def,
            save_backwards_compatible: backwards_compatible,
            save_in_folders_with_placeholders: folders_with_placeholders,
            journal_migration: if backwards_compatible {
                JournalMigration::MaybeMigrateLegacyDb
            } else {
                JournalMigration::None
            },
            needs_save: false,
        });
    }
}

/// Migration #2 of `setupFoldersHelper`: moves a 3.0-era journal (and its
/// `-shm`/`-wal`) from `old` to the folder's journal path. Returns `false`
/// when a move failed.
pub fn move_absolute_journal(old: &str, def: &FolderDefinition) -> bool {
    let target = format!(
        "{}/{}",
        def.local_path.trim_end_matches('/'),
        def.journal_path
    );
    let mut ok = true;
    for suffix in ["", "-shm", "-wal"] {
        let from = format!("{old}{suffix}");
        if Path::new(&from).exists() {
            ok &= std::fs::rename(&from, format!("{target}{suffix}")).is_ok();
        }
    }
    if ok {
        log::info!(target: "nextcloud.gui.folder.manager", "Successfully migrated syncjournal database.");
    } else {
        log::warn!(target: "nextcloud.gui.folder.manager", "Wasn't able to move 3.0 syncjournal database files to new location. One-time loss off sync settings possible.");
    }
    ok
}

/// `Folder::removeFromSettings`: the folder leaves all three groups.
pub fn remove_folder(settings: &mut Settings, account_id: &str, escaped_alias: &str) {
    for g in FolderGroup::ALL {
        settings.remove(&folder_group(account_id, g, escaped_alias));
    }
}

/// The group `Folder::saveToSettings` picks.
pub fn choose_group(
    virtual_files_enabled_or_were: bool,
    save_backwards_compatible: bool,
    one_account_only: bool,
) -> FolderGroup {
    if virtual_files_enabled_or_were {
        FolderGroup::FoldersWithPlaceholders
    } else if save_backwards_compatible || one_account_only {
        FolderGroup::Folders
    } else {
        FolderGroup::Multifolders
    }
}

/// `Folder::saveToSettings`: removes the folder from every group, then
/// saves its definition in `group`.
pub fn save_folder(
    settings: &mut Settings,
    account_id: &str,
    group: FolderGroup,
    def: &FolderDefinition,
) {
    let escaped = escape_alias(&def.alias);
    remove_folder(settings, account_id, &escaped);
    def.save(settings, &folder_group(account_id, group, &escaped));
}

/// Changes only the `paused` key of a folder, wherever it is saved (the
/// daemon's pause/resume). Returns `false` when the folder is not found.
pub fn save_paused(settings: &mut Settings, account_id: &str, alias: &str, paused: bool) -> bool {
    let escaped = escape_alias(alias);
    for g in FolderGroup::ALL {
        let fg = folder_group(account_id, g, &escaped);
        if !settings.child_keys(&fg).is_empty() {
            settings.set_value(&join_key(&fg, "paused"), paused);
            return true;
        }
    }
    false
}

/// The alias `FolderMan::addFolderInternal` gives a folder: the wanted one
/// if free, else `wanted.toInt() + 1`, `+ 2`, ... (folder aliases are
/// unique over all accounts).
pub fn allocate_alias(wanted: &str, used: &[String], blocked: &[String]) -> String {
    let base: i64 = wanted.trim().parse::<i32>().map(i64::from).unwrap_or(0);
    let mut alias = wanted.to_owned();
    let mut count = 0;
    while alias.is_empty() || used.contains(&alias) || blocked.contains(&alias) {
        count += 1;
        alias = (base + count).to_string();
    }
    alias
}

/// Every folder alias (unescaped) of the settings, over all accounts.
pub fn all_aliases(settings: &Settings) -> Vec<String> {
    let mut out = Vec::new();
    for id in settings.child_groups(ACCOUNTS_GROUP) {
        for g in FolderGroup::ALL {
            let group = join_key(&account_group(&id), g.name());
            for a in settings.child_groups(&group) {
                let a = unescape_alias(&a);
                if !out.contains(&a) {
                    out.push(a);
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Path validity (FolderMan::checkPathValidityForNewFolder & co.)
// ---------------------------------------------------------------------------

/// `FolderMan::PathValidityResult` (errors only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathValidityError {
    ErrorRecursiveValidity,
    ErrorContainsFolder,
    ErrorContainedInFolder,
    ErrorNonEmptyFolder,
}

/// An account seen by the server: its URL with the login name
/// (`QUrl` with `userName` set).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerIdentity {
    pub url: String,
    pub user: String,
}

/// A configured folder, as `checkPathValidityForNewFolder` sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExistingFolder {
    /// `Folder::path()`: the canonical local path with a trailing `/`
    /// (see [`folder_canonical_path`]).
    pub path: String,
    pub server: ServerIdentity,
}

/// What `Folder` computes as its path: the canonical local path (the
/// configured one when it cannot be resolved), with a trailing `/`.
pub fn folder_canonical_path(local_path: &str) -> String {
    let p = std::fs::canonicalize(local_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| {
            log::warn!(target: "nextcloud.gui.folder", "Broken symlink: {local_path}");
            local_path.to_owned()
        });
    trailing_slash_path(&p)
}

/// `QDir::cleanPath` for absolute or relative `/` paths.
pub fn clean_path(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            c => parts.push(c),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_owned()
    } else {
        joined
    }
}

/// `QFileInfo(path)`: (`dir().path()`, `fileName()`).
fn split_file_info(path: &str) -> (String, String) {
    match path.rfind('/') {
        None => (".".to_owned(), path.to_owned()),
        Some(0) => ("/".to_owned(), path[1..].to_owned()),
        Some(i) => (path[..i].to_owned(), path[i + 1..].to_owned()),
    }
}

/// `canonicalPath()` of folderman.cpp: like `canonicalFilePath`, but also
/// for paths that do not exist (their existing ancestors are resolved).
fn canonical_path(path: &str) -> String {
    if !Path::new(path).exists() {
        let (parent, name) = split_file_info(path);
        if parent == path {
            return path.to_owned();
        }
        return format!("{}/{name}", canonical_path(&parent));
    }
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `numberOfSyncJournals`: `.sync_*.db` and `._sync_*.db` files in a
/// directory.
fn number_of_sync_journals(path: &str) -> usize {
    let Ok(rd) = std::fs::read_dir(path) else {
        return 0;
    };
    rd.filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            (n.starts_with(".sync_") || n.starts_with("._sync_")) && n.ends_with(".db")
        })
        .count()
}

fn is_writable(path: &str) -> bool {
    rustix::fs::access(path, rustix::fs::Access::WRITE_OK).is_ok()
}

/// `checkPathValidityRecursive`.
fn check_path_validity_recursive(path: &str) -> Option<String> {
    if path.is_empty() {
        return Some(
            "Please choose a different location. The selected folder isn't valid.".to_owned(),
        );
    }
    if number_of_sync_journals(path) != 0 {
        return Some(format!(
            "Please choose a different location. {path} is already being used as a sync folder."
        ));
    }
    let p = Path::new(path);
    if !p.exists() {
        let (parent, _) = split_file_info(path);
        if parent != path {
            return check_path_validity_recursive(&parent);
        }
        return Some(format!(
            "Please choose a different location. The path {path} doesn't exist."
        ));
    }
    if !p.is_dir() {
        return Some(format!(
            "Please choose a different location. The path {path} isn't a folder."
        ));
    }
    if !is_writable(path) {
        return Some(format!(
            "Please choose a different location. You don't have enough permissions to write to {path}."
        ));
    }
    None
}

fn starts_with_cs(s: &str, prefix: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        s.starts_with(prefix)
    } else {
        s.to_lowercase().starts_with(&prefix.to_lowercase())
    }
}

/// `FolderMan::checkPathValidityForNewFolder`: `Ok(())` when `path` can
/// become a new sync folder for `server` (when given).
pub fn check_path_validity_for_new_folder(
    path: &str,
    server: Option<&ServerIdentity>,
    existing: &[ExistingFolder],
) -> Result<(), (PathValidityError, String)> {
    if let Some(msg) = check_path_validity_recursive(path) {
        log::debug!(target: "nextcloud.gui.folder.manager", "{path} {msg}");
        return Err((PathValidityError::ErrorRecursiveValidity, msg));
    }
    // check if the local directory isn't used yet in another ownCloud sync
    let cs = !nc_journal::utility::fs_case_preserving();
    let user_dir = format!("{}/", clean_path(&canonical_path(path)));
    for f in existing {
        let folder_dir = format!("{}/", clean_path(&canonical_path(&f.path)));
        let different_paths = if cs {
            folder_dir != user_dir
        } else {
            folder_dir.to_lowercase() != user_dir.to_lowercase()
        };
        if different_paths && starts_with_cs(&folder_dir, &user_dir, cs) {
            return Err((
                PathValidityError::ErrorContainsFolder,
                format!(
                    "Please choose a different location. {path} is already being used as a sync folder."
                ),
            ));
        }
        if different_paths && starts_with_cs(&user_dir, &folder_dir, cs) {
            return Err((
                PathValidityError::ErrorContainedInFolder,
                format!(
                    "Please choose a different location. {path} is already contained in a folder used as a sync folder."
                ),
            ));
        }
        // if both paths are equal, the server url needs to be different
        // otherwise it would mean that a new connection from the same local
        // folder to the same account is added which is not wanted.
        if let Some(server) = server
            && !different_paths
            && *server == f.server
        {
            return Err((
                PathValidityError::ErrorNonEmptyFolder,
                format!(
                    "Please choose a different location. {path} is already being used as a sync folder for {}.",
                    server.url
                ),
            ));
        }
    }
    Ok(())
}

/// `FolderMan::GoodPathStrategy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoodPathStrategy {
    AllowOnlyNewPath,
    AllowOverrideExistingPath,
}

/// `FolderMan::folderForPath`: the configured folder containing `path`.
pub fn folder_for_path<'a>(
    path: &str,
    existing: &'a [ExistingFolder],
) -> Option<&'a ExistingFolder> {
    let absolute = format!("{}/", clean_path(path));
    let cs = !nc_journal::utility::fs_case_preserving();
    existing.iter().find(|f| {
        let folder_path = format!("{}/", clean_path(&f.path));
        starts_with_cs(&absolute, &folder_path, cs)
    })
}

/// `FolderMan::findGoodPathForNewSyncFolder`.
pub fn find_good_path_for_new_sync_folder(
    base_path: &str,
    server: Option<&ServerIdentity>,
    existing: &[ExistingFolder],
    strategy: GoodPathStrategy,
) -> String {
    // If the parent folder is a sync folder or contained in one, we can't
    // possibly find a valid sync folder inside it.
    let (parent, _) = split_file_info(base_path);
    let parent_canonical = std::fs::canonicalize(&parent)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if folder_for_path(&parent_canonical, existing).is_some() {
        return base_path.to_owned();
    }
    let mut folder = base_path.to_owned();
    let mut attempt = 1;
    loop {
        let is_good = check_path_validity_for_new_folder(&folder, server, existing).is_ok()
            && (strategy == GoodPathStrategy::AllowOverrideExistingPath
                || !Path::new(&folder).exists());
        if is_good {
            break;
        }
        attempt += 1;
        if attempt > 100 {
            return base_path.to_owned();
        }
        folder = format!("{base_path}{attempt}");
    }
    folder
}

#[cfg(test)]
mod tests;
