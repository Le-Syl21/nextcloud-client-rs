// SPDX-FileCopyrightText: 2024 Nextcloud GmbH and Nextcloud contributors
// SPDX-FileCopyrightText: 2020 ownCloud GmbH
// SPDX-License-Identifier: CC0-1.0
//
// This software is in the public domain, furnished "as is", without technical
// support, and with no warranty, express or implied, as to its usefulness for
// any purpose.
//
// Port of upstream test/testdatabaseerror.cpp (nextcloud/desktop v34.0.5).

use nc_testutils::{FakeFolder, FileInfo, FileModifier};

#[test]
fn test_database_error() {
    /* This test will make many iteration, at each iteration, the iᵗʰ database access will fail.
     * The test ensure that if there is a failure, the next sync recovers. And if there was
     * no error, then everything was sync'ed properly.
     */

    let mut final_state = FileInfo::default();
    for count in 0.. {
        eprintln!("Starting Iteration {count}");

        let mut fake_folder = FakeFolder::new(FileInfo::A12_B12_C12_S12());

        // Do a couple of changes
        {
            let mut rm = fake_folder.remote_modifier();
            rm.insert("A/a0", 64, b'W');
            rm.append_byte("A/a1");
            rm.remove("A/a2");
            rm.rename("S/s1", "S/s1_renamed");
            rm.mkdir("D");
            rm.mkdir("D/subdir");
            rm.insert("D/subdir/file", 64, b'W');
        }
        fake_folder.local_modifier().insert("B/b0", 64, b'W');
        fake_folder.local_modifier().append_byte("B/b1");
        fake_folder.remote_modifier().remove("B/b2");
        fake_folder.local_modifier().mkdir("NewDir");
        fake_folder.local_modifier().rename("C", "NewDir/C");

        // Set the counter
        fake_folder.sync_journal().set_autotest_fail_counter(count);

        // run the sync
        let result = fake_folder.sync_once();

        eprintln!("Result of iteration {count} was {result}");

        if fake_folder.sync_journal().autotest_fail_counter() >= 0 {
            // No error was thrown, we are finished
            assert!(result);
            assert_eq!(
                fake_folder.current_local_state(),
                fake_folder.current_remote_state()
            );
            assert_eq!(fake_folder.current_remote_state(), final_state);
            return;
        }

        if !result {
            fake_folder.sync_journal().set_autotest_fail_counter(-1);
            // Try again
            assert!(fake_folder.sync_once(), "iteration {count}");
        }

        assert_eq!(
            fake_folder.current_local_state(),
            fake_folder.current_remote_state(),
            "iteration {count}"
        );
        if count == 0 {
            final_state = fake_folder.current_remote_state();
        } else {
            // the final state should be the same for every iteration
            assert_eq!(
                fake_folder.current_remote_state(),
                final_state,
                "iteration {count}"
            );
        }
    }
}
