/*
 * SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2018 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2016 ownCloud GmbH
 * SPDX-FileCopyrightText: 2020 ownCloud GmbH
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

//! The in-memory file tree (`FileInfo`) and the `FileModifier` interface.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, SystemTime};

use nc_journal::remote_permissions::RemotePermissions;

use crate::path::PathComponents;
use crate::util::{msecs_since_epoch, rand, secs_since_epoch};

/// `generateEtag()`: hex milliseconds since epoch followed by a hex random.
pub fn generate_etag() -> String {
    format!("{:x}{:x}", msecs_since_epoch(SystemTime::now()), rand())
}

/// `generateFileId()`: a value as seen in `oc:id` attributes (file ID +
/// instance ID).
pub fn generate_file_id() -> String {
    format!("{}oc1x2y3z4w", rand())
}

/// `FileModifier::LockState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LockState {
    FileLocked,
    #[default]
    FileUnlocked,
}

/// Arguments of `FileModifier::modifyLockState` (grouped in a struct).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockChange {
    pub lock_state: LockState,
    pub lock_type: i32,
    pub lock_owner: String,
    pub lock_owner_id: String,
    pub lock_editor_id: String,
    pub lock_time: u64,
    pub lock_timeout: u64,
}

/// `FileInfo::EtagsAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtagsAction {
    Keep,
    Invalidate,
}

/// `FileInfo::FolderQuota`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderQuota {
    pub bytes_used: i64,
    pub bytes_available: i64,
    bytes_available_string: String,
}

impl Default for FolderQuota {
    fn default() -> Self {
        Self::new(0, 5_000_000_000)
    }
}

impl FolderQuota {
    pub fn new(bytes_used: i64, bytes_available: i64) -> Self {
        Self {
            bytes_used,
            bytes_available,
            bytes_available_string: String::new(),
        }
    }

    pub fn bytes_available_string(&self) -> String {
        if self.bytes_available_string.is_empty() {
            self.bytes_available.to_string()
        } else {
            self.bytes_available_string.clone()
        }
    }

    pub fn set_bytes_available_string(&mut self, s: &str) {
        self.bytes_available_string = s.to_owned();
    }
}

/// Operations both trees (local disk, remote memory) support.
pub trait FileModifier {
    fn remove(&mut self, relative_path: &str);
    /// Upstream defaults: `size = 64`, `content_char = 'W'`.
    fn insert(&mut self, relative_path: &str, size: i64, content_char: u8);
    fn set_contents(&mut self, relative_path: &str, content_char: u8);
    fn append_byte(&mut self, relative_path: &str);
    fn mkdir(&mut self, relative_path: &str);
    fn rename(&mut self, relative_path: &str, relative_destination: &str);
    fn set_mod_time(&mut self, relative_path: &str, mod_time: SystemTime);
    fn modify_lock_state(&mut self, relative_path: &str, change: LockChange);
    fn set_e2ee(&mut self, relative_path: &str, enabled: bool);
}

/// One node of the fake file tree (a directory unless built as a file).
#[derive(Clone)]
pub struct FileInfo {
    pub name: String,
    pub operation_status: u16,
    pub is_dir: bool,
    pub is_shared: bool,
    pub download_forbidden: bool,
    /// When null, PROPFIND reports everything (`GRDNVCKW`, plus `S` if shared).
    pub permissions: RemotePermissions,
    pub last_modified: SystemTime,
    pub etag: String,
    pub file_id: String,
    pub checksums: String,
    /// Raw XML appended inside `<d:prop>` of PROPFIND responses.
    pub extra_dav_properties: String,
    pub size: i64,
    pub content_char: u8,
    pub lock_state: LockState,
    pub lock_type: i32,
    pub lock_token: String,
    pub lock_owner: String,
    pub lock_owner_id: String,
    pub lock_editor_id: String,
    pub lock_time: u64,
    pub lock_timeout: u64,
    pub is_encrypted: bool,
    pub is_live_photo: bool,
    pub folder_quota: FolderQuota,
    /// Sorted by name to be able to compare trees.
    pub children: BTreeMap<String, FileInfo>,
    pub parent_path: String,
}

impl Default for FileInfo {
    fn default() -> Self {
        Self {
            name: String::new(),
            operation_status: 200,
            is_dir: true,
            is_shared: false,
            download_forbidden: false,
            permissions: RemotePermissions::null(),
            last_modified: SystemTime::now() - Duration::from_secs(7 * 24 * 3600),
            etag: generate_etag(),
            file_id: generate_file_id(),
            checksums: String::new(),
            extra_dav_properties: String::new(),
            size: 0,
            content_char: b'W',
            lock_state: LockState::FileUnlocked,
            lock_type: 0,
            lock_token: String::new(),
            lock_owner: String::new(),
            lock_owner_id: String::new(),
            lock_editor_id: String::new(),
            lock_time: 0,
            lock_timeout: 0,
            is_encrypted: false,
            is_live_photo: false,
            folder_quota: FolderQuota::default(),
            children: BTreeMap::new(),
            parent_path: String::new(),
        }
    }
}

impl FileInfo {
    /// `FileInfo(name)`: an empty directory.
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..Self::default()
        }
    }

    /// `FileInfo(name, size)`: a file filled with `'W'`.
    pub fn file(name: &str, size: i64) -> Self {
        Self::file_with_char(name, size, b'W')
    }

    /// `FileInfo(name, size, contentChar)`.
    pub fn file_with_char(name: &str, size: i64, content_char: u8) -> Self {
        Self {
            name: name.to_owned(),
            is_dir: false,
            size,
            content_char,
            ..Self::default()
        }
    }

    /// `FileInfo(name, size, contentChar, mtime)`.
    pub fn file_with_mtime(name: &str, size: i64, content_char: u8, mtime: SystemTime) -> Self {
        Self {
            last_modified: mtime,
            ..Self::file_with_char(name, size, content_char)
        }
    }

    /// `FileInfo(name, {children...})`: a directory.
    pub fn dir(name: &str, children: impl IntoIterator<Item = FileInfo>) -> Self {
        let mut fi = Self::named(name);
        for child in children {
            fi.add_child(child);
        }
        fi
    }

    /// `FileInfo::A12_B12_C12_S12()`: the default tree of most sync tests.
    #[allow(non_snake_case)]
    pub fn A12_B12_C12_S12() -> Self {
        let mut fi = Self::dir(
            "",
            [
                Self::dir("A", [Self::file("a1", 4), Self::file("a2", 4)]),
                Self::dir("B", [Self::file("b1", 16), Self::file("b2", 16)]),
                Self::dir("C", [Self::file("c1", 24), Self::file("c2", 24)]),
            ],
        );
        let mut shared = Self::dir("S", [Self::file("s1", 32), Self::file("s2", 32)]);
        shared.is_shared = true;
        for child in shared.children.values_mut() {
            child.is_shared = true;
        }
        // Upstream inserts without fixing up parent paths (they are "S", the
        // root path being empty, so it makes no difference).
        fi.children.insert(shared.name.clone(), shared);
        fi
    }

    pub fn add_child(&mut self, info: FileInfo) {
        let path = self.path();
        let name = info.name.clone();
        let dest = replace_child(&mut self.children, name, info);
        dest.parent_path = path;
        dest.fixup_parent_path_recursively();
    }

    /// `find`: walks `path`, regenerating the etags of the target and of
    /// every ancestor (including `self`) when `action` is `Invalidate`.
    pub fn find_with(
        &mut self,
        path: impl Into<PathComponents>,
        action: EtagsAction,
    ) -> Option<&mut FileInfo> {
        let components = path.into();
        self.find_components(components.as_slice(), action)
    }

    fn find_components(
        &mut self,
        components: &[String],
        action: EtagsAction,
    ) -> Option<&mut FileInfo> {
        let Some((first, rest)) = components.split_first() else {
            if action == EtagsAction::Invalidate {
                self.etag = generate_etag();
            }
            return Some(self);
        };
        let child = self.children.get_mut(first)?;
        let found = child.find_components(rest, action);
        if found.is_some() && action == EtagsAction::Invalidate {
            // Update parents on the way back
            self.etag = generate_etag();
        }
        found
    }

    /// `find(path)` without touching etags, mutable.
    pub fn find_mut(&mut self, path: impl Into<PathComponents>) -> Option<&mut FileInfo> {
        self.find_with(path, EtagsAction::Keep)
    }

    /// `find(path)` without touching etags.
    pub fn find(&self, path: impl Into<PathComponents>) -> Option<&FileInfo> {
        let components = path.into();
        let mut node = self;
        for c in components.as_slice() {
            node = node.children.get(c)?;
        }
        Some(node)
    }

    /// `findInvalidatingEtags`.
    pub fn find_invalidating_etags(
        &mut self,
        path: impl Into<PathComponents>,
    ) -> Option<&mut FileInfo> {
        self.find_with(path, EtagsAction::Invalidate)
    }

    /// `findRecursive`: the first component is looked up with `action`, the
    /// rest without touching etags. Panics when not found (upstream
    /// dereferences a null pointer).
    pub fn find_recursive(
        &mut self,
        path: impl Into<PathComponents>,
        action: EtagsAction,
    ) -> FileInfo {
        let components = path.into();
        let (first, rest) = components
            .as_slice()
            .split_first()
            .expect("findRecursive: empty path");
        let mut result = self
            .find_components(std::slice::from_ref(first), action)
            .expect("findRecursive: not found");
        for c in rest {
            result = result
                .children
                .get_mut(c)
                .expect("findRecursive: not found");
        }
        result.clone()
    }

    /// `createDir`: (re)places an empty directory, invalidating the parents'
    /// etags. `None` if the parent does not exist.
    pub fn create_dir(&mut self, relative_path: &str) -> Option<&mut FileInfo> {
        let components = PathComponents::new(relative_path);
        let parent = self.find_invalidating_etags(components.parent_dir_components())?;
        let parent_path = parent.path();
        let name = components.file_name().to_owned();
        let child = replace_child(&mut parent.children, name.clone(), FileInfo::named(&name));
        child.parent_path = parent_path;
        child.etag = generate_etag();
        Some(child)
    }

    /// `create`: (re)places a file, invalidating the parents' etags. `None`
    /// if the parent does not exist.
    pub fn create(
        &mut self,
        relative_path: &str,
        size: i64,
        content_char: u8,
    ) -> Option<&mut FileInfo> {
        let components = PathComponents::new(relative_path);
        let parent = self.find_invalidating_etags(components.parent_dir_components())?;
        let parent_path = parent.path();
        let name = components.file_name().to_owned();
        let child = replace_child(
            &mut parent.children,
            name.clone(),
            FileInfo::file(&name, size),
        );
        child.parent_path = parent_path;
        child.content_char = content_char;
        child.etag = generate_etag();
        Some(child)
    }

    /// `setModTimeKeepEtag`.
    pub fn set_mod_time_keep_etag(&mut self, relative_path: &str, mod_time: SystemTime) {
        self.find_mut(relative_path)
            .expect("setModTimeKeepEtag: not found")
            .last_modified = mod_time;
    }

    /// `setIsLivePhoto`.
    pub fn set_is_live_photo(&mut self, relative_path: &str, is_live_photo: bool) {
        self.find_mut(relative_path)
            .expect("setIsLivePhoto: not found")
            .is_live_photo = is_live_photo;
    }

    /// `setFolderQuota`.
    pub fn set_folder_quota(
        &mut self,
        relative_path: &str,
        quota: FolderQuota,
        action: EtagsAction,
    ) {
        self.find_with(relative_path, action)
            .expect("setFolderQuota: not found")
            .folder_quota = quota;
    }

    /// `path()`: `parentPath/name` (no leading slash; empty for the root).
    pub fn path(&self) -> String {
        if self.parent_path.is_empty() {
            self.name.clone()
        } else {
            format!("{}/{}", self.parent_path, self.name)
        }
    }

    /// `absolutePath()`: `trailingSlashPath(parentPath) + name`.
    pub fn absolute_path(&self) -> String {
        if self.parent_path.ends_with('/') {
            format!("{}{}", self.parent_path, self.name)
        } else {
            format!("{}/{}", self.parent_path, self.name)
        }
    }

    /// Value of `oc:fileid` in PROPFIND responses: unlike `oc:id`, without
    /// the instance ID. Empty if the instance-ID marker is missing (Qt 6
    /// `left(-1)`).
    pub fn numeric_file_id(&self) -> String {
        self.file_id
            .find("oc1x2y3z4w")
            .map(|i| self.file_id[..i].to_owned())
            .unwrap_or_default()
    }

    pub fn fixup_parent_path_recursively(&mut self) {
        let p = self.path();
        for (key, child) in &mut self.children {
            assert_eq!(*key, child.name);
            child.parent_path = p.clone();
            child.fixup_parent_path_recursively();
        }
    }
}

impl FileModifier for FileInfo {
    fn remove(&mut self, relative_path: &str) {
        let components = PathComponents::new(relative_path);
        let parent = self
            .find_invalidating_etags(components.parent_dir_components())
            .expect("FileInfo::remove: parent not found");
        parent.children.remove(components.file_name());
    }

    fn insert(&mut self, relative_path: &str, size: i64, content_char: u8) {
        self.create(relative_path, size, content_char);
    }

    fn set_contents(&mut self, relative_path: &str, content_char: u8) {
        let file = self
            .find_invalidating_etags(relative_path)
            .expect("setContents: not found");
        file.content_char = content_char;
        file.last_modified = SystemTime::now();
    }

    fn append_byte(&mut self, relative_path: &str) {
        self.find_invalidating_etags(relative_path)
            .expect("appendByte: not found")
            .size += 1;
    }

    fn mkdir(&mut self, relative_path: &str) {
        self.create_dir(relative_path);
    }

    fn rename(&mut self, old_path: &str, new_path: &str) {
        let new_components = PathComponents::new(new_path);
        let dir = self
            .find_invalidating_etags(new_components.parent_dir_components())
            .expect("rename: destination dir not found");
        assert!(dir.is_dir, "rename: destination parent is not a directory");
        let components = PathComponents::new(old_path);
        let parent = self
            .find_invalidating_etags(components.parent_dir_components())
            .expect("rename: source parent not found");
        // QMap::take() returns a default-constructed value for a missing key.
        let mut fi = parent
            .children
            .remove(components.file_name())
            .unwrap_or_default();
        let dir = self
            .find_mut(new_components.parent_dir_components())
            .expect("rename: destination dir vanished");
        fi.parent_path = dir.path();
        fi.name = new_components.file_name().to_owned();
        fi.fixup_parent_path_recursively();
        dir.children.insert(fi.name.clone(), fi);
    }

    fn set_mod_time(&mut self, relative_path: &str, mod_time: SystemTime) {
        self.find_invalidating_etags(relative_path)
            .expect("setModTime: not found")
            .last_modified = mod_time;
    }

    fn modify_lock_state(&mut self, relative_path: &str, change: LockChange) {
        let file = self
            .find_invalidating_etags(relative_path)
            .expect("modifyLockState: not found");
        file.lock_state = change.lock_state;
        file.lock_type = change.lock_type;
        file.lock_owner = change.lock_owner;
        file.lock_owner_id = change.lock_owner_id;
        file.lock_editor_id = change.lock_editor_id;
        file.lock_time = change.lock_time;
        file.lock_timeout = change.lock_timeout;
    }

    fn set_e2ee(&mut self, relative_path: &str, enabled: bool) {
        self.find_invalidating_etags(relative_path)
            .expect("setE2EE: not found")
            .is_encrypted = enabled;
    }
}

/// `map[name] = info`, returning the stored value.
pub(crate) fn replace_child(
    map: &mut BTreeMap<String, FileInfo>,
    name: String,
    info: FileInfo,
) -> &mut FileInfo {
    use std::collections::btree_map::Entry;
    match map.entry(name) {
        Entry::Occupied(mut e) => {
            e.insert(info);
            e.into_mut()
        }
        Entry::Vacant(e) => e.insert(info),
    }
}

/// Consider files to be equal between local and remote as a user would:
/// name, kind, size, content character and children.
impl PartialEq for FileInfo {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.is_dir == other.is_dir
            && self.size == other.size
            && self.content_char == other.content_char
            && self.children == other.children
    }
}

/// Prints `toStringNoElide`, so that `assert_eq!` on trees shows a readable
/// diff like upstream's `QTest::toString(FileInfo)`.
impl fmt::Debug for FileInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&to_string_no_elide(self))
    }
}

fn add_files(dest: &mut Vec<String>, fi: &FileInfo) {
    if fi.is_dir {
        dest.push(format!("{} - dir", fi.path()));
        for child in fi.children.values() {
            add_files(dest, child);
        }
    } else {
        dest.push(format!(
            "{} - {} {}-bytes",
            fi.path(),
            fi.size,
            fi.content_char as char
        ));
    }
}

/// `toStringNoElide`.
pub fn to_string_no_elide(fi: &FileInfo) -> String {
    let mut files = Vec::new();
    for child in fi.children.values() {
        add_files(&mut files, child);
    }
    files.sort();
    format!(
        "FileInfo with {} files(\n\t{}\n)",
        files.len(),
        files.join("\n\t")
    )
}

fn add_files_db_data(dest: &mut Vec<String>, fi: &FileInfo) {
    // could include etag, permissions etc, but would need extra work
    if fi.is_dir {
        dest.push(format!(
            "{} - dir {} {}",
            fi.name,
            secs_since_epoch(fi.last_modified),
            fi.file_id
        ));
        for child in fi.children.values() {
            add_files_db_data(dest, child);
        }
    } else {
        dest.push(format!(
            "{} - file {} {} {}",
            fi.name,
            fi.size,
            secs_since_epoch(fi.last_modified),
            fi.file_id
        ));
    }
}

/// `printDbData`.
pub fn print_db_data(fi: &FileInfo) -> String {
    let mut files = Vec::new();
    for child in fi.children.values() {
        add_files_db_data(&mut files, child);
    }
    format!("FileInfo with {} files({})", files.len(), files.join(", "))
}

/// `findConflict`: the conflict copy of `filename` in `dir`, if any.
///
/// Faithful to upstream, including its quirk: the parent is looked up with
/// `QFileInfo::path()`, which is `"."` for a top-level file, so top-level
/// conflicts are never found.
pub fn find_conflict<'a>(dir: &'a FileInfo, filename: &str) -> Option<&'a FileInfo> {
    let (parent, file_name) = match filename.rfind('/') {
        Some(0) => ("/", &filename[1..]),
        Some(i) => (&filename[..i], &filename[i + 1..]),
        None => (".", filename),
    };
    let parent_dir = dir.find(parent)?;
    // QFileInfo::baseName(): up to the first dot.
    let base_name = file_name.split('.').next().unwrap_or_default();
    let start = format!("{base_name} (conflicted copy");
    parent_dir
        .children
        .values()
        .find(|item| item.name.starts_with(&start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a12_tree_and_paths() {
        let fi = FileInfo::A12_B12_C12_S12();
        assert_eq!(fi.children.len(), 4);
        let a1 = fi.find("A/a1").unwrap();
        assert_eq!(a1.path(), "A/a1");
        assert_eq!(a1.absolute_path(), "A/a1");
        assert_eq!(fi.find("A").unwrap().absolute_path(), "/A");
        assert_eq!(fi.absolute_path(), "/");
        assert_eq!(a1.size, 4);
        assert!(!a1.is_dir);
        assert!(fi.find("S/s1").unwrap().is_shared);
        assert!(fi.find("A/nope").is_none());
        assert_eq!(fi.find("").unwrap().name, "");
    }

    #[test]
    fn etag_propagation_on_invalidate() {
        let mut fi = FileInfo::A12_B12_C12_S12();
        let root = fi.etag.clone();
        let a = fi.find("A").unwrap().etag.clone();
        let b = fi.find("B").unwrap().etag.clone();
        let a1 = fi.find("A/a1").unwrap().etag.clone();
        std::thread::sleep(Duration::from_millis(2));
        fi.append_byte("A/a1");
        assert_ne!(fi.etag, root);
        assert_ne!(fi.find("A").unwrap().etag, a);
        assert_ne!(fi.find("A/a1").unwrap().etag, a1);
        assert_eq!(fi.find("B").unwrap().etag, b);
        assert_eq!(fi.find("A/a1").unwrap().size, 5);

        // Keep: nothing changes
        let root = fi.etag.clone();
        fi.set_mod_time_keep_etag("A/a2", SystemTime::now());
        assert_eq!(fi.etag, root);
    }

    #[test]
    fn modifier_operations() {
        let mut fi = FileInfo::A12_B12_C12_S12();
        fi.insert("A/new", 10, b'X');
        let new = fi.find("A/new").unwrap();
        assert_eq!((new.size, new.content_char, new.is_dir), (10, b'X', false));
        assert_eq!(new.parent_path, "A");
        fi.mkdir("A/sub");
        fi.insert("A/sub/f", 1, b'W');
        fi.rename("A/sub", "B/moved");
        assert!(fi.find("A/sub").is_none());
        assert_eq!(fi.find("B/moved/f").unwrap().path(), "B/moved/f");
        fi.remove("B/moved");
        assert!(fi.find("B/moved").is_none());
        fi.set_contents("C/c1", b'Z');
        assert_eq!(fi.find("C/c1").unwrap().content_char, b'Z');
        assert!(fi.create("missing/x", 1, b'W').is_none());
        fi.set_e2ee("C", true);
        assert!(fi.find("C").unwrap().is_encrypted);
        fi.modify_lock_state(
            "C/c2",
            LockChange {
                lock_state: LockState::FileLocked,
                lock_owner_id: "alice".into(),
                ..LockChange::default()
            },
        );
        assert_eq!(fi.find("C/c2").unwrap().lock_state, LockState::FileLocked);
    }

    #[test]
    fn equality_ignores_metadata() {
        let a = FileInfo::A12_B12_C12_S12();
        let mut b = FileInfo::A12_B12_C12_S12();
        b.find_mut("A/a1").unwrap().etag = "other".into();
        b.find_mut("A/a1").unwrap().last_modified = SystemTime::UNIX_EPOCH;
        assert_eq!(a, b);
        b.append_byte("A/a1");
        assert_ne!(a, b);
        assert!(format!("{b:?}").contains("A/a1 - 5 W-bytes"));
    }

    #[test]
    fn numeric_file_id_and_generators() {
        let fi = FileInfo::named("x");
        assert!(fi.file_id.ends_with("oc1x2y3z4w"));
        assert!(fi.numeric_file_id().chars().all(|c| c.is_ascii_digit()));
        assert!(!fi.numeric_file_id().is_empty());
        assert!(generate_etag().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn find_conflict_quirk() {
        let mut fi = FileInfo::A12_B12_C12_S12();
        fi.insert("A/a1 (conflicted copy 2026-10-07 101010)", 4, b'W');
        fi.insert("top (conflicted copy 2026-10-07 101010).txt", 4, b'W');
        assert!(find_conflict(&fi, "A/a1").is_some());
        assert!(find_conflict(&fi, "B/b1").is_none());
        assert!(find_conflict(&fi, "top.txt").is_none());
    }
}
