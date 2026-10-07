// SPDX-FileCopyrightText: 2021 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2016 ownCloud, Inc.
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.

//! Helpers shared by the ported upstream sync tests (the anonymous
//! namespaces of `test/testsyncengine.cpp` and friends).

#![allow(dead_code)]

use nc_sync::{Instruction, Status};
use nc_testutils::{FileInfo, ItemCompletedSpy, PathComponents};

/// `itemDidComplete(spy, path)`.
pub fn item_did_complete(spy: &ItemCompletedSpy, path: &str) -> bool {
    let item = spy.find_item(path);
    item.instruction != Instruction::None && item.instruction != Instruction::UpdateMetadata
}

/// `itemDidCompleteSuccessfully(spy, path)`.
pub fn item_did_complete_successfully(spy: &ItemCompletedSpy, path: &str) -> bool {
    spy.find_item(path).status == Status::Success
}

/// `itemDidCompleteSuccessfullyWithExpectedRank(spy, path, rank)`.
pub fn item_did_complete_successfully_with_expected_rank(
    spy: &ItemCompletedSpy,
    path: &str,
    rank: usize,
) -> bool {
    spy.find_item_with_expected_rank(path, rank).status == Status::Success
}

/// `itemSuccessfullyCompletedGetRank(spy, path)`.
pub fn item_successfully_completed_get_rank(spy: &ItemCompletedSpy, path: &str) -> i64 {
    spy.items()
        .iter()
        .position(|i| i.destination() == path)
        .map_or(-1, |p| p as i64)
}

/// `findCaseClashConflicts(dir)`.
pub fn find_case_clash_conflicts(dir: &FileInfo) -> Vec<String> {
    dir.children
        .values()
        .filter(|i| i.name.contains("(case clash from"))
        .map(|i| i.path())
        .collect()
}

/// `expectConflict(state, path)`.
pub fn expect_conflict(state: &FileInfo, path: &str) -> bool {
    let components = PathComponents::new(path);
    let Some(base) = state.find(components.parent_dir_components()) else {
        return false;
    };
    base.children
        .values()
        .any(|i| i.name.starts_with(components.file_name()) && i.name.contains("(case clash from"))
}

/// `1_MiB` and friends.
pub const fn mib(v: i64) -> i64 {
    v * 1024 * 1024
}
