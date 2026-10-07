/*
 * SPDX-FileCopyrightText: 2020 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2016 ownCloud GmbH
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

//! Port of upstream `test/testexcludedfiles.cpp` (v34.0.5), one Rust test per
//! upstream test function, same order, same assertions.
//!
//! Upstream keeps a global `excludedFiles` across test functions (converted
//! from CMocka); here each test builds its own instance with the same
//! `setup()` / `setup_init()` helpers.

use super::*;
use crate::csync::ItemType;

use ExcludeType::*;

const EXCLUDE_LIST_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data/sync-exclude.lst");

fn setup() -> ExcludedFiles {
    let mut excluded_files = ExcludedFiles::default();
    excluded_files.set_wildcards_match_slash(false);
    excluded_files
}

fn setup_init() -> ExcludedFiles {
    let mut excluded_files = setup();

    excluded_files.add_exclude_file_path(EXCLUDE_LIST_FILE);
    assert!(excluded_files.reload_exclude_files());

    /* and add some unicode stuff */
    excluded_files.add_manual_exclude("*.💩"); // is this source file utf8 encoded?
    excluded_files.add_manual_exclude("пятницы.*");
    excluded_files.add_manual_exclude("*/*.out");
    excluded_files.add_manual_exclude("latex*/*.run.xml");
    excluded_files.add_manual_exclude("latex/*/*.tex.tmp");

    assert!(excluded_files.reload_exclude_files());
    excluded_files
}

/// Small wrapper mirroring the static helpers of the upstream test class.
struct Ex(ExcludedFiles);

impl Ex {
    fn file_full(&self, path: &str) -> ExcludeType {
        self.0.full_pattern_match(path, ItemType::File)
    }
    fn dir_full(&self, path: &str) -> ExcludeType {
        self.0.full_pattern_match(path, ItemType::Directory)
    }
    fn file_traversal(&mut self, path: &str) -> ExcludeType {
        self.0.traversal_pattern_match(path, ItemType::File)
    }
    fn dir_traversal(&mut self, path: &str) -> ExcludeType {
        self.0.traversal_pattern_match(path, ItemType::Directory)
    }
    fn add(&mut self, expr: &str) {
        self.0.add_manual_exclude(expr);
    }
    fn add_in(&mut self, expr: &str, base: &str) {
        self.0.add_manual_exclude_with_base(expr, base);
    }
    fn reload(&mut self) -> bool {
        self.0.reload_exclude_files()
    }
}

fn pattern_of(map: &BTreeMap<String, PatternRegex>, key: &str) -> String {
    map.get(key)
        .map(|r| r.pattern().to_string())
        .unwrap_or_default()
}

/// `QStandardPaths::writableLocation(TempLocation)` in test mode: a private
/// temporary directory.
fn temp_location() -> tempfile::TempDir {
    tempfile::tempdir().expect("temporary directory")
}

#[test]
fn test_fun() {
    let mut excluded = ExcludedFiles::default();
    let exclude_hidden = true;
    let keep_hidden = false;

    assert!(!excluded.is_excluded("/a/b", "/a", keep_hidden));
    assert!(!excluded.is_excluded("/a/b~", "/a", keep_hidden));
    assert!(!excluded.is_excluded("/a/.b", "/a", keep_hidden));
    assert!(excluded.is_excluded("/a/.b", "/a", exclude_hidden));

    excluded.add_exclude_file_path(EXCLUDE_LIST_FILE);
    excluded.reload_exclude_files();

    assert!(!excluded.is_excluded("/a/b", "/a", keep_hidden));
    assert!(excluded.is_excluded("/a/b~", "/a", keep_hidden));
    assert!(!excluded.is_excluded("/a/.b", "/a", keep_hidden));
    assert!(excluded.is_excluded("/a/.Trashes", "/a", keep_hidden));
    assert!(excluded.is_excluded("/a/foo_conflict-bar", "/a", keep_hidden));
    assert!(excluded.is_excluded("/a/foo (conflicted copy bar)", "/a", keep_hidden));
    assert!(excluded.is_excluded("/a/.b", "/a", exclude_hidden));
}

#[test]
fn check_csync_exclude_add() {
    let mut ex = Ex(setup());
    ex.add("/tmp/check_csync1/*");
    assert_eq!(ex.file_full("/tmp/check_csync1/foo"), FileExcludeList);
    assert_eq!(ex.file_full("/tmp/check_csync2/foo"), NotExcluded);
    assert!(ex.0.all_excludes["/"].contains(&"/tmp/check_csync1/*".to_string()));

    assert!(pattern_of(&ex.0.full_regex_file, "/").contains("csync1"));
    assert!(pattern_of(&ex.0.full_traversal_regex_file, "/").contains("csync1"));
    assert!(!pattern_of(&ex.0.bname_traversal_regex_file, "/").contains("csync1"));

    ex.add("foo");
    assert!(pattern_of(&ex.0.bname_traversal_regex_file, "/").contains("foo"));
    assert!(pattern_of(&ex.0.full_regex_file, "/").contains("foo"));
    assert!(!pattern_of(&ex.0.full_traversal_regex_file, "/").contains("foo"));
}

#[test]
fn check_csync_exclude_add_per_dir() {
    let mut ex = Ex(setup());
    ex.add_in("*", "/tmp/check_csync1/");
    assert_eq!(ex.file_full("/tmp/check_csync1/foo"), NotExcluded);
    assert_eq!(ex.file_full("/tmp/check_csync2/foo"), NotExcluded);
    // QMap::operator[] on a missing key yields an empty list.
    assert!(
        !ex.0
            .all_excludes
            .get("/tmp/check_csync1/")
            .is_some_and(|v| v.contains(&"*".to_string()))
    );

    ex.add("foo");
    assert!(pattern_of(&ex.0.full_regex_file, "/").contains("foo"));

    ex.add_in("foo/bar", "/tmp/check_csync1/");
    assert!(pattern_of(&ex.0.full_regex_file, "/tmp/check_csync1/").contains("bar"));
    assert!(pattern_of(&ex.0.full_traversal_regex_file, "/tmp/check_csync1/").contains("bar"));
    assert!(!pattern_of(&ex.0.bname_traversal_regex_file, "/tmp/check_csync1/").contains("foo"));
}

#[test]
fn check_csync_excluded() {
    let mut ex = Ex(setup_init());
    assert_eq!(ex.file_full(""), NotExcluded);
    assert_eq!(ex.file_full("/"), NotExcluded);
    assert_eq!(ex.file_full("A"), NotExcluded);
    assert_eq!(ex.file_full("krawel_krawel"), NotExcluded);
    assert_eq!(ex.file_full(".kde/share/config/kwin.eventsrc"), NotExcluded);
    assert_eq!(
        ex.file_full(".directory/cache-maximegalon/cache1.txt"),
        FileExcludeList
    );
    assert_eq!(ex.dir_full("mozilla/.directory"), FileExcludeList);

    /*
     * Test for patterns in subdirs. '.beagle' is defined as a pattern and has
     * to be found in top dir as well as in directories underneath.
     */
    assert_eq!(ex.dir_full(".apdisk"), FileExcludeList);
    assert_eq!(ex.dir_full("foo/.apdisk"), FileExcludeList);
    assert_eq!(ex.dir_full("foo/bar/.apdisk"), FileExcludeList);

    assert_eq!(ex.file_full(".java"), NotExcluded);

    /* Files in the ignored dir .java will also be ignored. */
    assert_eq!(ex.file_full(".apdisk/totally_amazing.jar"), FileExcludeList);

    /* and also in subdirs */
    assert_eq!(
        ex.file_full("projects/.apdisk/totally_amazing.jar"),
        FileExcludeList
    );

    /* csync-journal is ignored in general silently. */
    assert_eq!(ex.file_full(".csync_journal.db"), FileSilentlyExcluded);
    assert_eq!(ex.file_full(".csync_journal.db.ctmp"), FileSilentlyExcluded);
    assert_eq!(
        ex.file_full("subdir/.csync_journal.db"),
        FileSilentlyExcluded
    );

    /* also the new form of the database name */
    assert_eq!(ex.file_full("._sync_5bdd60bdfcfa.db"), FileSilentlyExcluded);
    assert_eq!(
        ex.file_full("._sync_5bdd60bdfcfa.db.ctmp"),
        FileSilentlyExcluded
    );
    assert_eq!(
        ex.file_full("._sync_5bdd60bdfcfa.db-shm"),
        FileSilentlyExcluded
    );
    assert_eq!(
        ex.file_full("subdir/._sync_5bdd60bdfcfa.db"),
        FileSilentlyExcluded
    );

    assert_eq!(ex.file_full(".sync_5bdd60bdfcfa.db"), FileSilentlyExcluded);
    assert_eq!(
        ex.file_full(".sync_5bdd60bdfcfa.db.ctmp"),
        FileSilentlyExcluded
    );
    assert_eq!(
        ex.file_full(".sync_5bdd60bdfcfa.db-shm"),
        FileSilentlyExcluded
    );
    assert_eq!(
        ex.file_full("subdir/.sync_5bdd60bdfcfa.db"),
        FileSilentlyExcluded
    );

    /* pattern ]*.directory - ignore and remove */
    assert_eq!(ex.file_full("my.~directory"), FileExcludeAndRemove);
    assert_eq!(
        ex.file_full("/a_folder/my.~directory"),
        FileExcludeAndRemove
    );

    /* Adobe lock files are excluded by extension (.idlk/.prlock) */
    assert_eq!(ex.file_full("Test.idlk"), FileExcludeList);
    assert_eq!(ex.file_full("~Test~0kjyv(.idlk"), FileExcludeList);
    assert_eq!(ex.file_full("subdir/Test.idlk"), FileExcludeList);
    assert_eq!(ex.file_full("Test.prlock"), FileExcludeList);
    assert_eq!(ex.file_full("subdir/Test.prlock"), FileExcludeList);

    /* Adobe documents themselves are NOT excluded */
    assert_eq!(ex.file_full("Test.indd"), NotExcluded);
    assert_eq!(ex.file_full("Test.icml"), NotExcluded);
    assert_eq!(ex.file_full("Test.prproj"), NotExcluded);

    /* Not excluded because the pattern .netscape/cache requires directory. */
    assert_eq!(ex.file_full(".netscape/cache"), NotExcluded);

    /* Not excluded  */
    assert_eq!(ex.file_full("unicode/中文.hé"), NotExcluded);
    /* excluded  */
    assert_eq!(ex.file_full("unicode/пятницы.txt"), FileExcludeList);
    assert_eq!(ex.file_full("unicode/中文.💩"), FileExcludeList);

    /* path wildcards */
    assert_eq!(ex.file_full("foobar/my_manuscript.out"), FileExcludeList);
    assert_eq!(
        ex.file_full("latex_tmp/my_manuscript.run.xml"),
        FileExcludeList
    );

    assert_eq!(ex.file_full("word_tmp/my_manuscript.run.xml"), NotExcluded);

    assert_eq!(ex.file_full("latex/my_manuscript.tex.tmp"), NotExcluded);

    assert_eq!(
        ex.file_full("latex/songbook/my_manuscript.tex.tmp"),
        FileExcludeList
    );

    #[cfg(windows)]
    {
        assert_eq!(ex.file_full(" file_leading_space"), NotExcluded);
        assert_eq!(ex.file_full("file_trailing_space "), NotExcluded);
        assert_eq!(
            ex.file_full(" file_leading_and_trailing_space "),
            NotExcluded
        );
        assert_eq!(ex.file_full("file_trailing_dot."), FileExcludeInvalidChar);
        assert_eq!(ex.file_full("AUX"), FileSilentlyExcluded);
        assert_eq!(ex.file_full("file_invalid_char<"), FileExcludeInvalidChar);
        assert_eq!(ex.file_full("file_invalid_char\n"), FileExcludeInvalidChar);
    }

    /* ? character */
    ex.add("bond00?");
    ex.reload();
    assert_eq!(ex.file_full("bond00"), NotExcluded);
    assert_eq!(ex.file_full("bond007"), FileExcludeList);
    assert_eq!(ex.file_full("bond0071"), NotExcluded);

    /* brackets */
    ex.add("a [bc] d");
    ex.reload();
    assert_eq!(ex.file_full("a d d"), NotExcluded);
    assert_eq!(ex.file_full("a  d"), NotExcluded);
    assert_eq!(ex.file_full("a b d"), FileExcludeList);
    assert_eq!(ex.file_full("a c d"), FileExcludeList);

    #[cfg(not(windows))] // Because of CSYNC_FILE_EXCLUDE_INVALID_CHAR on windows
    {
        /* escapes */
        ex.add("a \\*");
        ex.add("b \\?");
        ex.add("c \\[d]");
        ex.reload();
        assert_eq!(ex.file_full("a \\*"), NotExcluded);
        assert_eq!(ex.file_full("a bc"), NotExcluded);
        assert_eq!(ex.file_full("a *"), FileExcludeList);
        assert_eq!(ex.file_full("b \\?"), NotExcluded);
        assert_eq!(ex.file_full("b f"), NotExcluded);
        assert_eq!(ex.file_full("b ?"), FileExcludeList);
        assert_eq!(ex.file_full("c \\[d]"), NotExcluded);
        assert_eq!(ex.file_full("c d"), NotExcluded);
        assert_eq!(ex.file_full("c [d]"), FileExcludeList);
    }
}

#[test]
fn check_csync_excluded_per_dir() {
    let temp = temp_location();
    let temp_dir = temp.path().to_str().unwrap().to_string();
    let mut ex = Ex(ExcludedFiles::new(&format!("{temp_dir}/")));
    ex.0.set_wildcards_match_slash(false);
    ex.add("A");
    ex.reload();

    assert_eq!(ex.file_full("A"), FileExcludeList);

    ex.0.clear_manual_excludes();
    ex.add_in("A", &format!("{temp_dir}/B/"));
    ex.reload();

    assert_eq!(ex.file_full("A"), NotExcluded);
    assert_eq!(ex.file_full("B/A"), FileExcludeList);

    ex.0.clear_manual_excludes();
    ex.add_in("A/a1", &format!("{temp_dir}/B/"));
    ex.reload();

    assert_eq!(ex.file_full("A"), NotExcluded);
    assert_eq!(ex.file_full("B/A/a1"), FileExcludeList);

    let foo_dir = "check_csync1/foo";
    fs::create_dir_all(temp.path().join(foo_dir)).unwrap();

    let foo_exclude_list = format!("{temp_dir}/{foo_dir}/.sync-exclude.lst");
    fs::write(&foo_exclude_list, b"bar").unwrap();
    assert_eq!(fs::metadata(&foo_exclude_list).unwrap().len(), 3);

    ex.0.add_exclude_file_path(&foo_exclude_list);
    ex.reload();
    assert_eq!(ex.file_full(&format!("{foo_dir}/bar")), FileExcludeList);
    assert_eq!(ex.file_full(&format!("{foo_dir}/baz")), NotExcluded);
}

#[test]
fn check_csync_excluded_traversal_per_dir() {
    let mut ex = Ex(setup_init());
    assert_eq!(ex.file_traversal("/"), NotExcluded);

    /* path wildcards */
    ex.add_in("*/*.tex.tmp", "/latex/");
    assert_eq!(
        ex.file_traversal("latex/my_manuscript.tex.tmp"),
        NotExcluded
    );
    assert_eq!(
        ex.file_traversal("latex/songbook/my_manuscript.tex.tmp"),
        FileExcludeList
    );
}

#[test]
fn check_csync_excluded_traversal() {
    let mut ex = Ex(setup_init());
    assert_eq!(ex.file_traversal(""), NotExcluded);
    assert_eq!(ex.file_traversal("/"), NotExcluded);

    assert_eq!(ex.file_traversal("A"), NotExcluded);

    assert_eq!(ex.file_traversal("krawel_krawel"), NotExcluded);
    assert_eq!(
        ex.file_traversal(".kde/share/config/kwin.eventsrc"),
        NotExcluded
    );
    assert_eq!(ex.dir_traversal("mozilla/.directory"), FileExcludeList);

    /*
     * Test for patterns in subdirs. '.beagle' is defined as a pattern and has
     * to be found in top dir as well as in directories underneath.
     */
    assert_eq!(ex.dir_traversal(".apdisk"), FileExcludeList);
    assert_eq!(ex.dir_traversal("foo/.apdisk"), FileExcludeList);
    assert_eq!(ex.dir_traversal("foo/bar/.apdisk"), FileExcludeList);

    assert_eq!(ex.file_traversal(".java"), NotExcluded);

    /* csync-journal is ignored in general silently. */
    assert_eq!(ex.file_traversal(".csync_journal.db"), FileSilentlyExcluded);
    assert_eq!(
        ex.file_traversal(".csync_journal.db.ctmp"),
        FileSilentlyExcluded
    );
    assert_eq!(
        ex.file_traversal("subdir/.csync_journal.db"),
        FileSilentlyExcluded
    );
    assert_eq!(
        ex.file_traversal("/two/subdir/.csync_journal.db"),
        FileSilentlyExcluded
    );

    /* also the new form of the database name */
    assert_eq!(
        ex.file_traversal("._sync_5bdd60bdfcfa.db"),
        FileSilentlyExcluded
    );
    assert_eq!(
        ex.file_traversal("._sync_5bdd60bdfcfa.db.ctmp"),
        FileSilentlyExcluded
    );
    assert_eq!(
        ex.file_traversal("._sync_5bdd60bdfcfa.db-shm"),
        FileSilentlyExcluded
    );
    assert_eq!(
        ex.file_traversal("subdir/._sync_5bdd60bdfcfa.db"),
        FileSilentlyExcluded
    );

    assert_eq!(
        ex.file_traversal(".sync_5bdd60bdfcfa.db"),
        FileSilentlyExcluded
    );
    assert_eq!(
        ex.file_traversal(".sync_5bdd60bdfcfa.db.ctmp"),
        FileSilentlyExcluded
    );
    assert_eq!(
        ex.file_traversal(".sync_5bdd60bdfcfa.db-shm"),
        FileSilentlyExcluded
    );
    assert_eq!(
        ex.file_traversal("subdir/.sync_5bdd60bdfcfa.db"),
        FileSilentlyExcluded
    );

    /* Other builtin excludes */
    assert_eq!(ex.file_traversal("foo/Desktop.ini"), FileSilentlyExcluded);
    assert_eq!(ex.file_traversal("Desktop.ini"), FileSilentlyExcluded);

    /* pattern ]*.directory - ignore and remove */
    assert_eq!(ex.file_traversal("my.~directory"), FileExcludeAndRemove);
    assert_eq!(
        ex.file_traversal("/a_folder/my.~directory"),
        FileExcludeAndRemove
    );

    /* Not excluded because the pattern .netscape/cache requires directory. */
    assert_eq!(ex.file_traversal(".netscape/cache"), NotExcluded);

    /* Not excluded  */
    assert_eq!(ex.file_traversal("unicode/中文.hé"), NotExcluded);
    /* excluded  */
    assert_eq!(ex.file_traversal("unicode/пятницы.txt"), FileExcludeList);
    assert_eq!(ex.file_traversal("unicode/中文.💩"), FileExcludeList);

    /* path wildcards */
    assert_eq!(
        ex.file_traversal("foobar/my_manuscript.out"),
        FileExcludeList
    );
    assert_eq!(
        ex.file_traversal("latex_tmp/my_manuscript.run.xml"),
        FileExcludeList
    );
    assert_eq!(
        ex.file_traversal("word_tmp/my_manuscript.run.xml"),
        NotExcluded
    );
    assert_eq!(
        ex.file_traversal("latex/my_manuscript.tex.tmp"),
        NotExcluded
    );
    assert_eq!(
        ex.file_traversal("latex/songbook/my_manuscript.tex.tmp"),
        FileExcludeList
    );

    #[cfg(windows)]
    {
        assert_eq!(ex.file_traversal(" file_leading_space"), NotExcluded);
        assert_eq!(ex.file_traversal("file_trailing_space "), NotExcluded);
        assert_eq!(
            ex.file_traversal(" file_leading_and_trailing_space "),
            NotExcluded
        );
        assert_eq!(
            ex.file_traversal("file_trailing_dot."),
            FileExcludeInvalidChar
        );
        assert_eq!(ex.file_traversal("AUX"), FileSilentlyExcluded);
        assert_eq!(
            ex.file_traversal("file_invalid_char<"),
            FileExcludeInvalidChar
        );
    }

    /* From here the actual traversal tests */

    ex.add("/exclude");
    ex.reload();

    /* Check toplevel dir, the pattern only works for toplevel dir. */
    assert_eq!(ex.dir_traversal("/exclude"), FileExcludeList);
    assert_eq!(ex.dir_traversal("/foo/exclude"), NotExcluded);

    /* check for a file called exclude. Must still work */
    assert_eq!(ex.file_traversal("/exclude"), FileExcludeList);
    assert_eq!(ex.file_traversal("/foo/exclude"), NotExcluded);

    /* Add an exclude for directories only: excl/ */
    ex.add("excl/");
    ex.reload();
    assert_eq!(ex.dir_traversal("/excl"), FileExcludeList);
    assert_eq!(ex.dir_traversal("meep/excl"), FileExcludeList);

    // because leading dirs aren't checked!
    assert_eq!(ex.file_traversal("meep/excl/file"), NotExcluded);
    assert_eq!(ex.file_traversal("/excl"), NotExcluded);

    ex.add("/excludepath/withsubdir");
    ex.reload();

    assert_eq!(ex.dir_traversal("/excludepath/withsubdir"), FileExcludeList);
    assert_eq!(
        ex.file_traversal("/excludepath/withsubdir"),
        FileExcludeList
    );
    assert_eq!(ex.dir_traversal("/excludepath/withsubdir2"), NotExcluded);

    // because leading dirs aren't checked!
    assert_eq!(ex.dir_traversal("/excludepath/withsubdir/foo"), NotExcluded);

    /* Check ending of pattern */
    assert_eq!(ex.file_traversal("/exclude"), FileExcludeList);
    assert_eq!(ex.file_traversal("/excludeX"), NotExcluded);
    assert_eq!(ex.file_traversal("exclude"), NotExcluded);

    ex.add("exclude");
    ex.reload();
    assert_eq!(ex.file_traversal("exclude"), FileExcludeList);

    /* ? character */
    ex.add("bond00?");
    ex.reload();
    assert_eq!(ex.file_traversal("bond00"), NotExcluded);
    assert_eq!(ex.file_traversal("bond007"), FileExcludeList);
    assert_eq!(ex.file_traversal("bond0071"), NotExcluded);

    /* brackets */
    ex.add("a [bc] d");
    ex.reload();
    assert_eq!(ex.file_traversal("a d d"), NotExcluded);
    assert_eq!(ex.file_traversal("a  d"), NotExcluded);
    assert_eq!(ex.file_traversal("a b d"), FileExcludeList);
    assert_eq!(ex.file_traversal("a c d"), FileExcludeList);

    #[cfg(not(windows))] // Because of CSYNC_FILE_EXCLUDE_INVALID_CHAR on windows
    {
        /* escapes */
        ex.add("a \\*");
        ex.add("b \\?");
        ex.add("c \\[d]");
        ex.reload();
        assert_eq!(ex.file_traversal("a \\*"), NotExcluded);
        assert_eq!(ex.file_traversal("a bc"), NotExcluded);
        assert_eq!(ex.file_traversal("a *"), FileExcludeList);
        assert_eq!(ex.file_traversal("b \\?"), NotExcluded);
        assert_eq!(ex.file_traversal("b f"), NotExcluded);
        assert_eq!(ex.file_traversal("b ?"), FileExcludeList);
        assert_eq!(ex.file_traversal("c \\[d]"), NotExcluded);
        assert_eq!(ex.file_traversal("c d"), NotExcluded);
        assert_eq!(ex.file_traversal("c [d]"), FileExcludeList);
    }
}

#[test]
fn check_csync_dir_only() {
    let mut ex = Ex(setup());
    ex.add("filedir");
    ex.add("dir/");

    assert_eq!(ex.file_traversal("other"), NotExcluded);
    assert_eq!(ex.file_traversal("filedir"), FileExcludeList);
    assert_eq!(ex.file_traversal("dir"), NotExcluded);
    assert_eq!(ex.file_traversal("s/other"), NotExcluded);
    assert_eq!(ex.file_traversal("s/filedir"), FileExcludeList);
    assert_eq!(ex.file_traversal("s/dir"), NotExcluded);

    assert_eq!(ex.dir_traversal("other"), NotExcluded);
    assert_eq!(ex.dir_traversal("filedir"), FileExcludeList);
    assert_eq!(ex.dir_traversal("dir"), FileExcludeList);
    assert_eq!(ex.dir_traversal("s/other"), NotExcluded);
    assert_eq!(ex.dir_traversal("s/filedir"), FileExcludeList);
    assert_eq!(ex.dir_traversal("s/dir"), FileExcludeList);

    assert_eq!(ex.dir_full("filedir/foo"), FileExcludeList);
    assert_eq!(ex.file_full("filedir/foo"), FileExcludeList);
    assert_eq!(ex.dir_full("dir/foo"), FileExcludeList);
    assert_eq!(ex.file_full("dir/foo"), FileExcludeList);
}

#[test]
fn check_csync_pathes() {
    let mut ex = Ex(setup_init());
    ex.add("/exclude");
    ex.reload();

    /* Check toplevel dir, the pattern only works for toplevel dir. */
    assert_eq!(ex.dir_full("/exclude"), FileExcludeList);

    assert_eq!(ex.dir_full("/foo/exclude"), NotExcluded);

    /* check for a file called exclude. Must still work */
    assert_eq!(ex.file_full("/exclude"), FileExcludeList);

    assert_eq!(ex.file_full("/foo/exclude"), NotExcluded);

    /* Add an exclude for directories only: excl/ */
    ex.add("excl/");
    ex.reload();
    assert_eq!(ex.dir_full("/excl"), FileExcludeList);
    assert_eq!(ex.dir_full("meep/excl"), FileExcludeList);
    assert_eq!(ex.file_full("meep/excl/file"), FileExcludeList);

    assert_eq!(ex.file_full("/excl"), NotExcluded);

    ex.add("/excludepath/withsubdir");
    ex.reload();

    assert_eq!(ex.dir_full("/excludepath/withsubdir"), FileExcludeList);
    assert_eq!(ex.file_full("/excludepath/withsubdir"), FileExcludeList);

    assert_eq!(ex.dir_full("/excludepath/withsubdir2"), NotExcluded);

    assert_eq!(ex.dir_full("/excludepath/withsubdir/foo"), FileExcludeList);
}

#[test]
fn check_csync_wildcards() {
    let mut ex = Ex(setup());
    ex.add("a/foo*bar");
    ex.add("b/foo*bar*");
    ex.add("c/foo?bar");
    ex.add("d/foo?bar*");
    ex.add("e/foo?bar?");
    ex.add("g/bar*");
    ex.add("h/bar?");

    ex.0.set_wildcards_match_slash(false);

    assert_eq!(ex.file_traversal("a/fooXYZbar"), FileExcludeList);
    assert_eq!(ex.file_traversal("a/fooX/Zbar"), NotExcluded);

    assert_eq!(ex.file_traversal("b/fooXYZbarABC"), FileExcludeList);
    assert_eq!(ex.file_traversal("b/fooX/ZbarABC"), NotExcluded);

    assert_eq!(ex.file_traversal("c/fooXbar"), FileExcludeList);
    assert_eq!(ex.file_traversal("c/foo/bar"), NotExcluded);

    assert_eq!(ex.file_traversal("d/fooXbarABC"), FileExcludeList);
    assert_eq!(ex.file_traversal("d/foo/barABC"), NotExcluded);

    assert_eq!(ex.file_traversal("e/fooXbarA"), FileExcludeList);
    assert_eq!(ex.file_traversal("e/foo/barA"), NotExcluded);

    assert_eq!(ex.file_traversal("g/barABC"), FileExcludeList);
    assert_eq!(ex.file_traversal("g/XbarABC"), NotExcluded);

    assert_eq!(ex.file_traversal("h/barZ"), FileExcludeList);
    assert_eq!(ex.file_traversal("h/XbarZ"), NotExcluded);

    ex.0.set_wildcards_match_slash(true);

    assert_eq!(ex.file_traversal("a/fooX/Zbar"), FileExcludeList);
    assert_eq!(ex.file_traversal("b/fooX/ZbarABC"), FileExcludeList);
    assert_eq!(ex.file_traversal("c/foo/bar"), FileExcludeList);
    assert_eq!(ex.file_traversal("d/foo/barABC"), FileExcludeList);
    assert_eq!(ex.file_traversal("e/foo/barA"), FileExcludeList);
}

#[test]
fn check_csync_regex_translation() {
    let _ex = setup();
    let translate = |pattern: &str| ExcludedFiles::convert_to_regexp_syntax(pattern, false);

    assert_eq!(translate(""), "");
    assert_eq!(translate("abc"), "abc");
    assert_eq!(translate("a*c"), "a[^/]*c");
    assert_eq!(translate("a?c"), "a[^/]c");
    assert_eq!(translate("a[xyz]c"), "a[xyz]c");
    assert_eq!(translate("a[xyzc"), "a\\[xyzc");
    assert_eq!(translate("a[!xyz]c"), "a[^xyz]c");
    assert_eq!(translate("a\\*b\\?c\\[d\\\\e"), "a\\*b\\?c\\[d\\\\e");
    assert_eq!(translate("a.c"), "a\\.c");
    assert_eq!(translate("?𠜎?"), "[^/]\\𠜎[^/]"); // 𠜎 is 4-byte utf8
}

#[test]
fn check_csync_bname_trigger() {
    let _ex = setup();
    let mut wildcards_match_slash = false;
    let translate = |pattern: &str, wms: bool| ExcludedFiles::extract_bname_trigger(pattern, wms);

    assert_eq!(translate("", wildcards_match_slash), "");
    assert_eq!(translate("a/b/", wildcards_match_slash), "");
    assert_eq!(translate("a/b/c", wildcards_match_slash), "c");
    assert_eq!(translate("c", wildcards_match_slash), "c");
    assert_eq!(translate("a/foo*", wildcards_match_slash), "foo*");
    assert_eq!(translate("a/abc*foo*", wildcards_match_slash), "abc*foo*");

    wildcards_match_slash = true;

    assert_eq!(translate("", wildcards_match_slash), "");
    assert_eq!(translate("a/b/", wildcards_match_slash), "");
    assert_eq!(translate("a/b/c", wildcards_match_slash), "c");
    assert_eq!(translate("c", wildcards_match_slash), "c");
    assert_eq!(translate("*", wildcards_match_slash), "*");
    assert_eq!(translate("a/foo*", wildcards_match_slash), "foo*");
    assert_eq!(translate("a/abc?foo*", wildcards_match_slash), "*foo*");
    assert_eq!(translate("a/abc*foo*", wildcards_match_slash), "*foo*");
    assert_eq!(translate("a/abc?foo?", wildcards_match_slash), "*foo?");
    assert_eq!(translate("a/abc*foo?*", wildcards_match_slash), "*foo?*");
    assert_eq!(translate("a/abc*/foo*", wildcards_match_slash), "foo*");
}

#[test]
fn check_csync_is_windows_reserved_word() {
    assert!(csync_is_windows_reserved_word("CON"));
    assert!(csync_is_windows_reserved_word("con"));
    assert!(csync_is_windows_reserved_word("CON."));
    assert!(csync_is_windows_reserved_word("con."));
    assert!(csync_is_windows_reserved_word("CON.ference"));
    assert!(!csync_is_windows_reserved_word("CONference"));
    assert!(!csync_is_windows_reserved_word("conference"));
    assert!(!csync_is_windows_reserved_word("conf.erence"));
    assert!(!csync_is_windows_reserved_word("co"));

    assert!(csync_is_windows_reserved_word("COM2"));
    assert!(csync_is_windows_reserved_word("com2"));
    assert!(csync_is_windows_reserved_word("COM2."));
    assert!(csync_is_windows_reserved_word("com2."));
    assert!(csync_is_windows_reserved_word("COM2.ference"));
    assert!(!csync_is_windows_reserved_word("COM2ference"));
    assert!(!csync_is_windows_reserved_word("com2ference"));
    assert!(!csync_is_windows_reserved_word("com2f.erence"));
    assert!(!csync_is_windows_reserved_word("com"));

    assert!(csync_is_windows_reserved_word("CLOCK$"));
    assert!(csync_is_windows_reserved_word("$Recycle.Bin"));
    assert!(csync_is_windows_reserved_word("ClocK$"));
    assert!(csync_is_windows_reserved_word("$recycle.bin"));

    assert!(csync_is_windows_reserved_word("A:"));
    assert!(csync_is_windows_reserved_word("a:"));
    assert!(csync_is_windows_reserved_word("z:"));
    assert!(csync_is_windows_reserved_word("Z:"));
    assert!(csync_is_windows_reserved_word("M:"));
    assert!(csync_is_windows_reserved_word("m:"));
}

/// Upstream wraps the loop in `QBENCHMARK`; here it runs once (N = 1000)
/// and only checks the result, which keeps it fast in debug builds.
#[test]
fn check_csync_excluded_performance1() {
    let ex = Ex(setup_init());
    const N: i32 = 1000;
    let mut total_rc = 0;

    for _ in 0..N {
        total_rc += ex.dir_full("/this/is/quite/a/long/path/with/many/components") as i32;
        total_rc += ex.file_full(
            "/1/2/3/4/5/6/7/8/9/10/11/12/13/14/15/16/17/18/19/20/21/22/23/24/25/26/27/29",
        ) as i32;
    }
    assert_eq!(total_rc, 0); // mainly to avoid optimization
}

/// Upstream reuses the global instance left by the previous test
/// (`setup_init()` of performance1); here it is set up explicitly.
#[test]
fn check_csync_excluded_performance2() {
    let mut ex = Ex(setup_init());
    const N: i32 = 1000;
    let mut total_rc = 0;

    for _ in 0..N {
        total_rc += ex.dir_traversal("/this/is/quite/a/long/path/with/many/components") as i32;
        total_rc += ex.file_traversal(
            "/1/2/3/4/5/6/7/8/9/10/11/12/13/14/15/16/17/18/19/20/21/22/23/24/25/26/27/29",
        ) as i32;
    }
    assert_eq!(total_rc, 0); // mainly to avoid optimization
}

/// `strcmp()` semantics: compare up to the first NUL byte.
fn c_str(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())]
}

#[test]
fn check_csync_exclude_expand_escapes() {
    let mut line = br#"keep \' \" \? \\ \a \b \f \n \r \t \v \z \#"#.to_vec();
    csync_exclude_expand_escapes(&mut line);
    assert_eq!(
        c_str(&line),
        b"keep ' \" ? \\\\ \x07 \x08 \x0C \n \r \t \x0B \\z #"
    );

    let mut line = b"".to_vec();
    csync_exclude_expand_escapes(&mut line);
    assert_eq!(c_str(&line), b"");

    let mut line = b"\\".to_vec();
    csync_exclude_expand_escapes(&mut line);
    assert_eq!(c_str(&line), b"\\");
}

#[test]
fn check_version_directive() {
    let mut excludes = ExcludedFiles::default();
    excludes.set_client_version((2, 5, 0));

    let tests: [(&str, bool); 14] = [
        ("#!version == 2.5.0", true),
        ("#!version == 2.6.0", false),
        ("#!version < 2.6.0", true),
        ("#!version <= 2.6.0", true),
        ("#!version > 2.6.0", false),
        ("#!version >= 2.6.0", false),
        ("#!version < 2.4.0", false),
        ("#!version <= 2.4.0", false),
        ("#!version > 2.4.0", true),
        ("#!version >= 2.4.0", true),
        ("#!version < 2.5.0", false),
        ("#!version <= 2.5.0", true),
        ("#!version > 2.5.0", false),
        ("#!version >= 2.5.0", true),
    ];
    for (directive, expected) in tests {
        assert_eq!(
            excludes.version_directive_keep_next_line(directive.as_bytes()),
            expected,
            "{directive}"
        );
    }
}

#[test]
fn test_add_exclude_file_path_add_same_file_path_list_size_does_not_increase() {
    let mut excluded_files = ExcludedFiles::default();
    let file_path = "exclude/.sync-exclude.lst";

    excluded_files.add_exclude_file_path(file_path);
    excluded_files.add_exclude_file_path(file_path);

    assert_eq!(excluded_files.exclude_files.len(), 1);
}

#[test]
fn test_add_exclude_file_path_add_different_file_paths_list_size_increase() {
    let mut excluded_files = ExcludedFiles::default();

    let file_path1 = "exclude1/.sync-exclude.lst";
    let file_path2 = "exclude2/.sync-exclude.lst";

    excluded_files.add_exclude_file_path(file_path1);
    excluded_files.add_exclude_file_path(file_path2);

    assert_eq!(excluded_files.exclude_files.len(), 2);
}

#[test]
fn test_add_exclude_file_path_add_default_exclude_file_return_correct_map() {
    let base_path = "syncFolder/";
    let folder1 = "syncFolder/folder1/";
    let folder2 = format!("{folder1}folder2/");
    let mut excluded_files = ExcludedFiles::new(base_path);

    let default_exclude_list = "desktop-client/config-folder/sync-exclude.lst";
    let folder1_exclude_list = format!("{folder1}.sync-exclude.lst");
    let folder2_exclude_list = format!("{folder2}.sync-exclude.lst");

    excluded_files.add_exclude_file_path(default_exclude_list);
    excluded_files.add_exclude_file_path(&folder1_exclude_list);
    excluded_files.add_exclude_file_path(&folder2_exclude_list);

    assert_eq!(excluded_files.exclude_files.len(), 3);
    assert_eq!(
        excluded_files.exclude_files[base_path][0],
        default_exclude_list
    );
    assert_eq!(
        excluded_files.exclude_files[folder1][0],
        folder1_exclude_list
    );
    assert_eq!(
        excluded_files.exclude_files[&folder2][0],
        folder2_exclude_list
    );
}

#[test]
fn test_reload_exclude_files_file_does_not_exist_return_true() {
    let mut excluded_files = ExcludedFiles::default();
    let non_existing_file = "directory/.sync-exclude.lst";
    excluded_files.add_exclude_file_path(non_existing_file);
    assert!(excluded_files.reload_exclude_files());
    assert_eq!(excluded_files.all_excludes.len(), 0);
}

#[test]
fn test_reload_exclude_files_file_exists_return_true() {
    let temp = temp_location();
    let temp_dir = temp.path().to_str().unwrap().to_string();
    let mut excluded_files = ExcludedFiles::new(&format!("{temp_dir}/"));

    let sub_temp_dir = "exclude";
    fs::create_dir_all(temp.path().join(sub_temp_dir)).unwrap();

    let existing_file_path = format!("{temp_dir}/{sub_temp_dir}/.sync-exclude.lst");
    fs::write(&existing_file_path, b"").unwrap();

    excluded_files.add_exclude_file_path(&existing_file_path);
    assert!(excluded_files.reload_exclude_files());
    assert_eq!(excluded_files.all_excludes.len(), 1);
}

// ---------------------------------------------------------------------------
// Rust-only tests of the PCRE -> `regex` translation (not in upstream).
// ---------------------------------------------------------------------------

#[test]
fn rust_only_pcre_translation_keeps_pcre_semantics() {
    let re = |p: &str| Regex::new(&pcre_to_rust_regex(p)).unwrap();
    // QRegularExpression::escape() output for non-ASCII and '<' / '>'.
    assert!(re("^\\пятницы\\.\\<a\\>$").is_match("пятницы.<a>"));
    // NUL escape.
    assert!(re("^a\\0$").is_match("a\0"));
    // PCRE '$' also matches before a final newline.
    assert!(re("^foo$").is_match("foo\n"));
    assert!(!re("^foo$").is_match("foo\nbar"));
    // '[' and set operators are literal inside PCRE classes.
    assert!(re("^[[]$").is_match("["));
    assert!(re("^[a&&b]$").is_match("&"));
    assert!(re("^[]a]$").is_match("]"));
    assert!(re("^[[:digit:]]$").is_match("7"));
}

#[test]
fn rust_only_default_list_patterns_all_compile() {
    let mut ex = ExcludedFiles::default();
    ex.add_exclude_file_path(EXCLUDE_LIST_FILE);
    assert!(ex.reload_exclude_files());
    for map in [
        &ex.bname_traversal_regex_file,
        &ex.bname_traversal_regex_dir,
        &ex.full_traversal_regex_file,
        &ex.full_traversal_regex_dir,
        &ex.full_regex_file,
        &ex.full_regex_dir,
    ] {
        for regex in map.values() {
            assert!(regex.regex.is_some(), "{}", regex.pattern());
        }
    }
    // "]Icon\r*": the escaped CR matches.
    assert_eq!(
        ex.full_pattern_match("Icon\rfoo", ItemType::File),
        FileExcludeAndRemove
    );
    assert_eq!(
        DEFAULT_SYNC_EXCLUDE_LST.as_bytes(),
        fs::read(EXCLUDE_LIST_FILE).unwrap()
    );
}

#[test]
fn rust_only_version_directive_skips_next_line() {
    let mut ex = ExcludedFiles::default();
    ex.set_client_version((2, 5, 0));
    ex.load_exclude_file_patterns("/", b"#!version < 2.5.0\nold\n#!version >= 2.5.0\nnew\n");
    assert_eq!(ex.active_exclude_patterns(), vec!["new".to_string()]);
}
