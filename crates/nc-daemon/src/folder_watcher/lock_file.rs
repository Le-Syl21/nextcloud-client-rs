// SPDX-FileCopyrightText: 2024 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2014 ownCloud GmbH
// SPDX-License-Identifier: GPL-2.0-or-later
//
// Port of the lock file helpers of upstream `src/libsync/filesystem.{h,cpp}`
// (`FileLockingInfo`, `filePathLockFilePatternMatch`,
// `lockFileTargetFilePath`, `findAllLockFilesInDir` and their helpers in the
// anonymous namespace) (nextcloud/desktop v34.0.5).

//! Office / LibreOffice / AutoCAD / Adobe / Affinity lock file detection.
//!
//! The folder watcher uses these to report the documents that an
//! application opened (lock file created) or closed (lock file removed).
//! They belong to `OCC::FileSystem` upstream (`src/libsync`) and are also used
//! by `SyncEngine` (`lockFileDetected`); they live here until the sync engine
//! needs them, then they can move to `nc_sync::filesystem` unchanged.

use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use log::debug;
use regex::Regex;

const LOG: &str = "nextcloud.sync.filesystem";

/// `lockFilePatterns`.
const LOCK_FILE_PATTERNS: [&str; 2] = [".~lock.", "~$"];
/// `officeFileExtensions`.
const OFFICE_FILE_EXTENSIONS: [&str; 8] =
    ["doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "odp"];

/// `autoCADLockFileExtensions`: AutoCAD creates `.dwl` (plain text) and
/// `.dwl2` (XML) lock files when a `.dwg` drawing is opened, both with the
/// document's base name.
const AUTOCAD_LOCK_FILE_EXTENSIONS: [&str; 2] = ["dwl", "dwl2"];
/// `autoCADDocumentExtension`.
const AUTOCAD_DOCUMENT_EXTENSION: &str = "dwg";

/// `adobeLockFileDocumentExtensions`: Adobe lock file names only carry the
/// guarded document's base name; the document is found among the siblings.
/// `idlk`: InDesign documents (`indd`) and InCopy stories (`icml`);
/// `prlock`: Premiere Pro projects (`prproj`). Ordered like the upstream
/// `std::map` (by key).
const ADOBE_LOCK_FILE_DOCUMENT_EXTENSIONS: [(&str, &[&str]); 2] =
    [("idlk", &["indd", "icml"]), ("prlock", &["prproj"])];

/// `affinityLockFileSuffix`: Affinity by Canva creates `~lock~` suffix lock
/// files (e.g. `Screenshot.af~lock~`).
const AFFINITY_LOCK_FILE_SUFFIX: &str = "~lock~";
/// `affinityDocumentExtensions`.
const AFFINITY_DOCUMENT_EXTENSIONS: [&str; 4] = ["afphoto", "afdesign", "afpub", "af"];

/// `FileSystem::FileLockingInfo::Type`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileLockingType {
    #[default]
    Unset,
    Locked,
    Unlocked,
}

/// `FileSystem::FileLockingInfo`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileLockingInfo {
    pub path: String,
    pub locking_type: FileLockingType,
}

/// The file name of a `/`-separated path (`QFileInfo::fileName`).
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `QFileInfo::suffix`: the characters after the last `.` of the file name.
fn suffix(path: &str) -> &str {
    let name = file_name(path);
    name.rfind('.').map_or("", |i| &name[i + 1..])
}

/// `QFileInfo::completeBaseName`: the file name up to its last `.`.
fn complete_base_name(path: &str) -> &str {
    let name = file_name(path);
    name.rfind('.').map_or(name, |i| &name[..i])
}

/// `QFileInfo::dir().absoluteFilePath(name)` for an absolute `path`.
fn sibling_path(path: &str, name: &str) -> String {
    match path.rfind('/') {
        Some(0) => format!("/{name}"),
        Some(i) => format!("{}/{name}", &path[..i]),
        None => name.to_owned(),
    }
}

/// `QFileInfo::exists` / `QFile::exists` (follows symlinks).
fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// `isAutoCADLockFileExtension`.
fn is_autocad_lock_file_extension(path: &str) -> bool {
    let suffix = suffix(path).to_lowercase();
    AUTOCAD_LOCK_FILE_EXTENSIONS.contains(&suffix.as_str())
}

/// `autoCADLockFileTargetFilePath`: the `.dwg` with the lock file's base
/// name, when it exists.
fn autocad_lock_file_target_file_path(lock_file_path: &str) -> Option<String> {
    let base_name = complete_base_name(lock_file_path);
    if base_name.is_empty() {
        return None;
    }
    let candidate = sibling_path(
        lock_file_path,
        &format!("{base_name}.{AUTOCAD_DOCUMENT_EXTENSION}"),
    );
    exists(&candidate).then_some(candidate)
}

/// `autoCADSiblingLockFileExists`: AutoCAD creates `.dwl` and `.dwl2` as a
/// pair; the document is only unlocked when both are gone.
fn autocad_sibling_lock_file_exists(lock_file_path: &str) -> bool {
    let base_name = complete_base_name(lock_file_path);
    if base_name.is_empty() {
        return false;
    }
    let deleted_ext = suffix(lock_file_path).to_lowercase();
    AUTOCAD_LOCK_FILE_EXTENSIONS.iter().any(|ext| {
        *ext != deleted_ext && exists(&sibling_path(lock_file_path, &format!("{base_name}.{ext}")))
    })
}

/// `adobeDocumentExtensionsFor`.
fn adobe_document_extensions_for(lock_extension: &str) -> Option<&'static [&'static str]> {
    ADOBE_LOCK_FILE_DOCUMENT_EXTENSIONS
        .iter()
        .find(|(k, _)| *k == lock_extension)
        .map(|(_, v)| *v)
}

/// `isAdobeLockFileExtension`.
fn is_adobe_lock_file_extension(path: &str) -> bool {
    adobe_document_extensions_for(&suffix(path).to_lowercase()).is_some()
}

static IDLK_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^~(?<base>.+)~[^~]*\(\.idlk$").expect("valid regex"));
static PRLOCK_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?<base>.+)\.prlock$").expect("valid regex"));

/// `adobeLockFileDocumentBaseName`: `Test` from `~Test~0kjyv(.idlk` or from
/// `Test.prlock`.
fn adobe_lock_file_document_base_name(
    lock_file_name: &str,
    lock_extension: &str,
) -> Option<String> {
    let pattern = match lock_extension {
        "idlk" => &*IDLK_PATTERN,
        "prlock" => &*PRLOCK_PATTERN,
        _ => return None,
    };
    let captures = pattern.captures(lock_file_name)?;
    Some(captures.name("base").map_or("", |m| m.as_str()).to_owned())
}

/// `adobeLockFileTargetFilePath`: the sibling with the lock file's document
/// base name and one of the expected document extensions.
fn adobe_lock_file_target_file_path(lock_file_path: &str) -> Option<String> {
    let lock_file_name = file_name(lock_file_path);
    let lock_extension = suffix(lock_file_name).to_lowercase();
    let document_extensions = adobe_document_extensions_for(&lock_extension)?;
    if document_extensions.is_empty() {
        return None;
    }
    let base_name = adobe_lock_file_document_base_name(lock_file_name, &lock_extension)?;
    if base_name.is_empty() {
        return None;
    }
    document_extensions
        .iter()
        .map(|ext| sibling_path(lock_file_path, &format!("{base_name}.{ext}")))
        .find(|candidate| exists(candidate))
}

/// `isAffinityLockFile`.
fn is_affinity_lock_file(path: &str) -> bool {
    path.ends_with(AFFINITY_LOCK_FILE_SUFFIX)
}

/// `QDir(dirPath).entryInfoList(QDir::Files)` file names: regular files
/// (symlinks followed), no hidden files, sorted like `QDir::Name |
/// QDir::IgnoreCase`.
fn list_visible_files(dir_path: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir_path) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_file = fs::metadata(e.path()).is_ok_and(|m| m.is_file());
            (is_file && !name.starts_with('.')).then_some(name)
        })
        .collect();
    names.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    names
}

/// `findMatchingUnlockedFileInDir`: some software changes the lock file name
/// so that it does not match the original file exactly (removing symbols),
/// so the first file of `dir_path` whose name contains `lock_file_name` (and
/// that is not a lock file itself) is taken.
fn find_matching_unlocked_file_in_dir(dir_path: &str, lock_file_name: &str) -> String {
    for candidate in list_visible_files(dir_path) {
        if LOCK_FILE_PATTERNS.iter().any(|p| candidate.contains(p)) {
            continue;
        }
        if candidate.contains(lock_file_name) {
            return format!("{dir_path}/{candidate}");
        }
    }
    String::new()
}

/// `FileSystem::filePathLockFilePatternMatch`: the lock file pattern
/// (prefix, suffix or `.extension`) `path` matches, or an empty string.
pub fn file_path_lock_file_pattern_match(path: &str) -> String {
    debug!(target: LOG, "Checking if it is a lock file: {path}");

    let Some(last) = path.split('/').rfind(|s| !s.is_empty()) else {
        return String::new();
    };

    // Office / LibreOffice lock files are identified by a filename prefix.
    for pattern in LOCK_FILE_PATTERNS {
        if last.starts_with(pattern) {
            debug!(target: LOG, "Found a lock file with prefix: {pattern} in path: {path}");
            return pattern.to_owned();
        }
    }

    // Affinity lock files are identified by the `~lock~` suffix.
    if is_affinity_lock_file(last) {
        debug!(target: LOG, "Found an Affinity lock file with suffix ~lock~ in path: {path}");
        return AFFINITY_LOCK_FILE_SUFFIX.to_owned();
    }

    // AutoCAD lock files (.dwl / .dwl2) are identified by extension, not prefix.
    if is_autocad_lock_file_extension(last) {
        let suffix = suffix(last).to_lowercase();
        debug!(target: LOG, "Found an AutoCAD lock file with extension: {suffix} in path: {path}");
        return format!(".{suffix}");
    }

    // Adobe lock files (.idlk / .prlock) are identified by extension, not prefix.
    let suffix = suffix(last).to_lowercase();
    if adobe_document_extensions_for(&suffix).is_some() {
        let pattern = format!(".{suffix}");
        debug!(target: LOG, "Found an Adobe lock file with extension: {pattern} in path: {path}");
        return pattern;
    }

    String::new()
}

/// `FileSystem::isMatchingOfficeFileExtension` (only call it for files).
pub fn is_matching_office_file_extension(path: &str) -> bool {
    let split: Vec<&str> = path.split('.').collect();
    let extension = if split.len() > 1 {
        split[split.len() - 1]
    } else {
        ""
    };
    OFFICE_FILE_EXTENSIONS.contains(&extension)
}

/// `FileSystem::isMatchingAutoCADDocumentExtension` (only call it for files).
pub fn is_matching_autocad_document_extension(path: &str) -> bool {
    suffix(path).to_lowercase() == AUTOCAD_DOCUMENT_EXTENSION
}

/// `FileSystem::isMatchingAdobeDocumentExtension` (only call it for files).
pub fn is_matching_adobe_document_extension(path: &str) -> bool {
    let split: Vec<&str> = path.split('.').collect();
    let extension = if split.len() > 1 {
        split[split.len() - 1].to_lowercase()
    } else {
        String::new()
    };
    ADOBE_LOCK_FILE_DOCUMENT_EXTENSIONS
        .iter()
        .any(|(_, docs)| docs.contains(&extension.as_str()))
}

/// `FileSystem::isMatchingAffinityDocumentExtension` (only call it for files).
pub fn is_matching_affinity_document_extension(path: &str) -> bool {
    let extension = suffix(path).to_lowercase();
    AFFINITY_DOCUMENT_EXTENSIONS.contains(&extension.as_str())
}

/// `FileSystem::lockFileTargetFilePath`: the document guarded by the lock
/// file `lock_file_path` and whether it is now locked (the lock file exists)
/// or unlocked (it is gone). `lock_file_name_pattern` is the result of
/// [`file_path_lock_file_pattern_match`].
pub fn lock_file_target_file_path(
    lock_file_path: &str,
    lock_file_name_pattern: &str,
) -> FileLockingInfo {
    let mut result = FileLockingInfo::default();

    // Affinity lock files use a `~lock~` suffix (e.g., Screenshot.afphoto~lock~).
    // Strip the suffix to recover the guarded document path.
    if is_affinity_lock_file(lock_file_path) {
        let name = file_name(lock_file_path);
        let document_name = &name[..name.len().saturating_sub(AFFINITY_LOCK_FILE_SUFFIX.len())];
        if !document_name.is_empty() {
            result.path = sibling_path(lock_file_path, document_name);
            if exists(&result.path) {
                result.locking_type = if exists(lock_file_path) {
                    FileLockingType::Locked
                } else {
                    FileLockingType::Unlocked
                };
            } else {
                result.path.clear();
            }
        }
        return result;
    }

    // AutoCAD lock files (.dwl / .dwl2) share the document's base name, so the
    // guarded document is resolved by replacing the extension with .dwg.
    // AutoCAD creates .dwl and .dwl2 as a pair: only report Unlocked when both
    // are gone; if a sibling lock file still exists, leave the type as Unset so
    // the watcher does not prematurely unlock the document.
    if is_autocad_lock_file_extension(lock_file_path) {
        if let Some(target) = autocad_lock_file_target_file_path(lock_file_path) {
            result.path = target;
            if exists(lock_file_path) {
                result.locking_type = FileLockingType::Locked;
            } else if !autocad_sibling_lock_file_exists(lock_file_path) {
                result.locking_type = FileLockingType::Unlocked;
            }
        }
        return result;
    }

    // Adobe lock files (.idlk / .prlock) are resolved by sibling lookup: the lock
    // file name carries only the document base name, not its extension.
    if is_adobe_lock_file_extension(lock_file_path) {
        if let Some(target) = adobe_lock_file_target_file_path(lock_file_path) {
            result.path = target;
            if !result.path.is_empty() {
                result.locking_type = if exists(lock_file_path) {
                    FileLockingType::Locked
                } else {
                    FileLockingType::Unlocked
                };
            }
        }
        return result;
    }

    if lock_file_name_pattern.is_empty() {
        return result;
    }

    // `QString::replace` removes every occurrence, not only the prefix.
    let without_prefix = lock_file_path.replace(lock_file_name_pattern, "");
    let mut split: Vec<String> = without_prefix.split('.').map(str::to_owned).collect();
    if split.len() < 2 {
        return result;
    }

    // Remove possible non-alphanumerical characters at the end of the
    // extension (upstream filters the UTF-8 bytes with `std::isalnum`).
    let extension: String = split
        .pop()
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    split.push(extension);
    let without_prefix_new = split.join(".");

    debug!(target: LOG, "Assumed locked/unlocked file path {without_prefix_new} Going to try to find matching file");
    let mut split_file_path: Vec<&str> = without_prefix_new.split('/').collect();
    if split_file_path.len() > 1 {
        let lock_file_name_without_prefix = split_file_path.pop().unwrap_or_default();
        // Some software will modify the lock file name such that it does not
        // correspond to the original file (removing some symbols from the
        // name), so search for a matching file.
        result.path = find_matching_unlocked_file_in_dir(
            &split_file_path.join("/"),
            lock_file_name_without_prefix,
        );
    }

    if result.path.is_empty() || !exists(&result.path) {
        result.path.clear();
        return result;
    }
    result.locking_type = if exists(lock_file_path) {
        FileLockingType::Locked
    } else {
        FileLockingType::Unlocked
    };
    result
}

/// `FileSystem::findAllLockFilesInDir`: the lock files directly in
/// `dir_path` (hidden ones included).
pub fn find_all_lock_files_in_dir(dir_path: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir_path) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| fs::metadata(e.path()).is_ok_and(|m| m.is_file()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    names
        .into_iter()
        .map(|name| format!("{}/{name}", dir_path.trim_end_matches('/')))
        .filter(|path| !file_path_lock_file_pattern_match(path).is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_match() {
        assert_eq!(
            file_path_lock_file_pattern_match("/a/.~lock.doc.odt#"),
            ".~lock."
        );
        assert_eq!(file_path_lock_file_pattern_match("/a/~$doc.docx"), "~$");
        assert_eq!(
            file_path_lock_file_pattern_match("/a/pic.afphoto~lock~"),
            "~lock~"
        );
        assert_eq!(file_path_lock_file_pattern_match("/a/plan.DWL2"), ".dwl2");
        assert_eq!(
            file_path_lock_file_pattern_match("/a/~Test~0kjyv(.idlk"),
            ".idlk"
        );
        assert_eq!(file_path_lock_file_pattern_match("/a/doc.odt"), "");
        assert_eq!(file_path_lock_file_pattern_match("///"), "");
    }

    #[test]
    fn office_lock_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        fs::write(format!("{root}/document.docx"), "x").unwrap();
        let lock = format!("{root}/.~lock.document.docx#");
        fs::write(&lock, "x").unwrap();
        let info = lock_file_target_file_path(&lock, &file_path_lock_file_pattern_match(&lock));
        assert_eq!(info.path, format!("{root}/document.docx"));
        assert_eq!(info.locking_type, FileLockingType::Locked);
        assert_eq!(find_all_lock_files_in_dir(root), vec![lock.clone()]);
        fs::remove_file(&lock).unwrap();
        let info = lock_file_target_file_path(&lock, &file_path_lock_file_pattern_match(&lock));
        assert_eq!(info.locking_type, FileLockingType::Unlocked);
        let plain = format!("{root}/document.docx");
        assert_eq!(
            lock_file_target_file_path(&plain, "").locking_type,
            FileLockingType::Unset
        );
    }

    #[test]
    fn autocad_adobe_affinity_targets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        fs::write(format!("{root}/plan.dwg"), "x").unwrap();
        fs::write(format!("{root}/plan.dwl"), "x").unwrap();
        fs::write(format!("{root}/plan.dwl2"), "x").unwrap();
        fs::remove_file(format!("{root}/plan.dwl")).unwrap();
        let info = lock_file_target_file_path(&format!("{root}/plan.dwl"), "");
        assert_eq!(info.path, format!("{root}/plan.dwg"));
        assert_eq!(info.locking_type, FileLockingType::Unset);
        fs::remove_file(format!("{root}/plan.dwl2")).unwrap();
        let info = lock_file_target_file_path(&format!("{root}/plan.dwl2"), "");
        assert_eq!(info.locking_type, FileLockingType::Unlocked);

        fs::write(format!("{root}/Test.indd"), "x").unwrap();
        let idlk = format!("{root}/~Test~0kjyv(.idlk");
        fs::write(&idlk, "x").unwrap();
        let info = lock_file_target_file_path(&idlk, "");
        assert_eq!(info.path, format!("{root}/Test.indd"));
        assert_eq!(info.locking_type, FileLockingType::Locked);

        fs::write(format!("{root}/Shot.afphoto"), "x").unwrap();
        let info = lock_file_target_file_path(&format!("{root}/Shot.afphoto~lock~"), "");
        assert_eq!(info.path, format!("{root}/Shot.afphoto"));
        assert_eq!(info.locking_type, FileLockingType::Unlocked);
    }
}
