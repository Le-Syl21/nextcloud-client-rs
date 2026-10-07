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

//! The local side: `DiskFileModifier` and `FakeFolder::toDisk`/`fromDisk`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::file_info::{FileInfo, FileModifier, LockChange};

/// Sets a file's or directory's mtime (`OCC::FileSystem::setModTime`), with
/// one-second resolution like upstream's `qDateTimeToTime_t`.
fn set_mod_time(path: &Path, mod_time: SystemTime) {
    let secs = crate::util::secs_since_epoch(mod_time);
    let file = File::open(path).unwrap_or_else(|e| panic!("open {path:?} for setModTime: {e}"));
    file.set_modified(crate::util::from_secs(secs))
        .unwrap_or_else(|e| panic!("setModTime {path:?}: {e}"));
}

fn write_fill(file: &mut File, size: i64, content_char: u8) {
    let buf = [content_char; 1024];
    let size = usize::try_from(size).expect("negative size");
    for _ in 0..size / buf.len() {
        file.write_all(&buf).unwrap();
    }
    file.write_all(&buf[..size % buf.len()]).unwrap();
}

/// Applies [`FileModifier`] operations to a real directory.
#[derive(Debug, Clone)]
pub struct DiskFileModifier {
    root_dir: PathBuf,
}

impl DiskFileModifier {
    pub fn new(root_dir_path: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir_path.into(),
        }
    }

    /// `find`: the absolute path of `relative_path` (upstream returns a `QFile`).
    pub fn find(&self, relative_path: &str) -> PathBuf {
        self.root_dir.join(relative_path)
    }
}

impl FileModifier for DiskFileModifier {
    fn remove(&mut self, relative_path: &str) {
        let path = self.find(relative_path);
        if path.is_file() {
            fs::remove_file(&path).unwrap_or_else(|e| panic!("remove {path:?}: {e}"));
        } else {
            fs::remove_dir_all(&path)
                .unwrap_or_else(|e| panic!("delete failed for: {path:?}: {e}"));
        }
    }

    fn insert(&mut self, relative_path: &str, size: i64, content_char: u8) {
        let path = self.find(relative_path);
        let mut file = File::create(&path)
            .unwrap_or_else(|e| panic!("error while opening the file {path:?}: {e}"));
        write_fill(&mut file, size, content_char);
        drop(file);
        // Set the mtime 30 seconds in the past, for some tests that need to make
        // sure that the mtime differs.
        set_mod_time(&path, SystemTime::now() - Duration::from_secs(30));
        assert_eq!(fs::metadata(&path).unwrap().len() as i64, size);
    }

    fn set_contents(&mut self, relative_path: &str, content_char: u8) {
        let path = self.find(relative_path);
        let size = fs::metadata(&path)
            .unwrap_or_else(|e| panic!("{path:?} must exist: {e}"))
            .len();
        let mut file = File::create(&path).unwrap();
        write_fill(&mut file, size as i64, content_char);
    }

    fn append_byte(&mut self, relative_path: &str) {
        let path = self.find(relative_path);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap_or_else(|e| panic!("error while opening the file {path:?}: {e}"));
        let mut first = Vec::with_capacity(1);
        (&mut file).take(1).read_to_end(&mut first).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(&first).unwrap();
    }

    fn mkdir(&mut self, relative_path: &str) {
        let _ = fs::create_dir_all(self.find(relative_path));
    }

    fn rename(&mut self, from: &str, to: &str) {
        let from_path = self.find(from);
        assert!(from_path.exists(), "rename: {from_path:?} does not exist");
        fs::rename(&from_path, self.find(to))
            .unwrap_or_else(|e| panic!("failed to rename from: {from} to: {to}: {e}"));
    }

    fn set_mod_time(&mut self, relative_path: &str, mod_time: SystemTime) {
        set_mod_time(&self.find(relative_path), mod_time);
    }

    fn modify_lock_state(&mut self, _relative_path: &str, _change: LockChange) {}

    fn set_e2ee(&mut self, _relative_path: &str, _enabled: bool) {}
}

/// `FakeFolder::toDisk`: writes the children of `template` under `dir`.
pub fn to_disk(dir: &Path, template: &FileInfo) {
    for child in template.children.values() {
        let path = dir.join(&child.name);
        if child.is_dir {
            let _ = fs::create_dir(&path);
            to_disk(&path, child);
        } else {
            let mut file = File::create(&path)
                .unwrap_or_else(|e| panic!("error while opening the file {path:?}: {e}"));
            write_fill(&mut file, child.size, child.content_char);
            drop(file);
            set_mod_time(&path, child.last_modified);
        }
    }
}

/// `FakeFolder::fromDisk` + `fixupParentPathRecursively`: reads `dir` into a
/// tree. Like upstream, empty files are skipped (their content character is
/// unknown) and the content character is the first byte.
pub fn from_disk(dir: &Path) -> FileInfo {
    let mut root = FileInfo::default();
    from_disk_into(dir, &mut root);
    root.fixup_parent_path_recursively();
    root
}

fn from_disk_into(dir: &Path, template: &mut FileInfo) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"));
    for entry in entries {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        // QFileInfo follows symlinks.
        let meta = fs::metadata(entry.path()).unwrap();
        if meta.is_dir() {
            let sub = crate::file_info::replace_child(
                &mut template.children,
                name.clone(),
                FileInfo::named(&name),
            );
            from_disk_into(&entry.path(), sub);
        } else {
            let mut first = Vec::with_capacity(1);
            File::open(entry.path())
                .unwrap()
                .take(1)
                .read_to_end(&mut first)
                .unwrap();
            let Some(&content_char) = first.first() else {
                eprintln!("Empty file at: {:?}", entry.path());
                continue;
            };
            template.children.insert(
                name.clone(),
                FileInfo::file_with_mtime(
                    &name,
                    meta.len() as i64,
                    content_char,
                    meta.modified().unwrap(),
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_modifier() {
        let tmp = tempfile::tempdir().unwrap();
        let template = FileInfo::A12_B12_C12_S12();
        to_disk(tmp.path(), &template);
        assert_eq!(from_disk(tmp.path()), template);
        let mtime = fs::metadata(tmp.path().join("A/a1"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            crate::util::secs_since_epoch(mtime),
            crate::util::secs_since_epoch(template.find("A/a1").unwrap().last_modified)
        );

        let mut local = DiskFileModifier::new(tmp.path());
        let mut expected = template.clone();
        for m in [&mut local as &mut dyn FileModifier, &mut expected] {
            m.insert("A/new", 2000, b'Q');
            m.append_byte("B/b1");
            m.set_contents("C/c1", b'Y');
            m.mkdir("D");
            m.rename("A/a2", "D/a2");
            m.remove("S");
        }
        assert_eq!(from_disk(tmp.path()), expected);
        assert_eq!(fs::read(tmp.path().join("B/b1")).unwrap().len(), 17);

        // empty files are invisible to fromDisk
        local.insert("A/empty", 0, b'W');
        assert!(from_disk(tmp.path()).find("A/empty").is_none());
    }
}
