/*
 * SPDX-FileCopyrightText: 2022 Nextcloud GmbH and Nextcloud contributors
 * SPDX-FileCopyrightText: 2015 ownCloud, Inc.
 * SPDX-License-Identifier: CC0-1.0
 *
 * This software is in the public domain, furnished "as is", without technical
 * support, and with no warranty, express or implied, as to its usefulness for
 * any purpose.
 */

// Port of upstream test/testsyncfileitem.cpp (nextcloud/desktop v34.0.5).

use nc_sync::item::destination_less;
use nc_sync::{Instruction, SyncFileItem};

fn create_item(file: &str) -> SyncFileItem {
    SyncFileItem {
        file: file.to_owned(),
        ..Default::default()
    }
}

/// `operator<(const SyncFileItem &, const SyncFileItem &)`.
fn lt(item1: &SyncFileItem, item2: &SyncFileItem) -> bool {
    destination_less(item1.destination(), item2.destination())
}

#[test]
fn test_comparator() {
    // testComparator_data
    let moved_item1 = SyncFileItem {
        file: "folder/source/file.f".to_owned(),
        rename_target: "folder/destination/file.f".to_owned(),
        instruction: Instruction::Rename,
        ..Default::default()
    };

    let rows = [
        (
            "a1",
            create_item("client"),
            create_item("client/build"),
            create_item("client-build"),
        ),
        (
            "a2",
            create_item("test/t1"),
            create_item("test/t2"),
            create_item("test/t3"),
        ),
        (
            "a3",
            create_item("ABCD"),
            create_item("abcd"),
            create_item("zzzz"),
        ),
        (
            "move1",
            create_item("folder/destination"),
            moved_item1.clone(),
            create_item("folder/destination-2"),
        ),
        (
            "move2",
            create_item("folder/destination/1"),
            moved_item1.clone(),
            create_item("folder/source"),
        ),
        (
            "move3",
            create_item("abc"),
            moved_item1.clone(),
            create_item("ijk"),
        ),
    ];

    // testComparator
    for (row, a, b, c) in &rows {
        assert!(lt(a, b), "row {row}");
        assert!(lt(b, c), "row {row}");
        assert!(lt(a, c), "row {row}");

        assert!(!lt(b, a), "row {row}");
        assert!(!lt(c, b), "row {row}");
        assert!(!lt(c, a), "row {row}");

        assert!(!lt(a, a), "row {row}");
        assert!(!lt(b, b), "row {row}");
        assert!(!lt(c, c), "row {row}");
    }
}
