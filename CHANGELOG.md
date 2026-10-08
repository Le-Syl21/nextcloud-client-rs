# Changelog

All notable changes to this project are listed here. Versions follow
[Semantic Versioning](https://semver.org/); the behavioural reference is the
official Nextcloud desktop client v34.0.5.

## Unreleased

- **Live reload of `ncsyncd`.** `ncsync account add|remove`, `folder add|remove`,
  `takeover` and `handback` tell a running daemon to re-read its configuration (a new
  `reload` control request; no daemon, no error) and print whether it did. New accounts and
  folders are added and scheduled, removed ones stop: a running sync is aborted and the
  journal is only wiped (`folder remove`) or closed and kept (`handback`, hand edits) once
  the engine is back; the request is answered then. SIGHUP (`systemctl reload`) now does
  the same full reload instead of re-reading the credentials only, and tells systemd
  `RELOADING`/`READY`.
- **`ncsync account remove <id> [--force]`**, like the official client's "Remove account"
  (`AccountManager::deleteAccount`): refused while folders use the account (or a taken-over
  folder belongs to it) unless `--force`, which removes them too (files kept, journals of
  taken-over folders kept); the app password is revoked on the server
  (`DELETE ocs/v2.php/core/apppassword`, best effort) unless the official client has the
  same account, then forgotten (keyring service `ncsyncd`, the password file `ncsync`
  wrote); the official client's keyring is never touched, and systemd credentials or
  password files of your own are left in place and named.
- **`ncsync folder exclude|include|excluded <folder> <subfolder>...`**: selective sync
  ("Choose what to sync") from the command line, with upstream's
  `FolderStatusModel::slotApplySelectiveSync` semantics (white list, undecided list,
  remote and local rediscovery of the changed paths, immediate sync). It goes through the
  running daemon (a new `selective-sync` control request), which holds the journal, or edits
  the journal directly when no daemon syncs the folder.
- A folder removed while it syncs no longer has its journal deleted under the running
  engine (remote wipe included): the wipe waits for the engine to stop; and a folder
  removed right after its sync no longer keeps the next syncs from starting (upstream's
  `_currentSyncFolder` is a `QPointer`, cleared when the folder is deleted).

## 0.1.0-beta.2 (2026-10-08)

First beta tested on a real desktop (Secret Service keyring, systemd user unit, inotify,
Login Flow v2 by QR code) against a Nextcloud 35 server.

- Login Flow v2: the poll's 404 while the user has not logged in yet is a debug line, not
  two warnings every 3 s in the terminal (upstream logs it as a warning, which only reaches
  its log file).

## 0.1.0-beta.1 (2026-10-08)

First public version: an unofficial Rust port of the sync engine of the
Nextcloud desktop client v34.0.5, for Linux.

- **`ncsync sync`**: one-shot synchronization at `nextcloudcmd` parity (same
  options, same journal `.sync_xxxxxxxxxxxx.db`), including `nextcloudcmd`'s
  account provisioning mode (`--userid`), with Login Flow v2 (link and QR
  code in the terminal) when no app password is given.
- **`ncsyncd`**: the headless daemon with the official client's sync logic:
  inotify folder watcher, notify_push or etag polling, scheduling and
  back-off, several accounts and folders, the `nextcloud.cfg` configuration
  format, server-side file locking for office documents, remote wipe, client
  status reporting; systemd user service and `ncsyncd@` system template
  (`Type=notify`, watchdog, `ImportCredential=`).
- **`ncsync account|folder|takeover|handback|status|pause|resume|sync-now`**:
  configuration, folder takeover from and hand-back to the official client,
  daemon control.
- 384 of the 472 upstream test functions of the covered files ported and
  passing (88 not applicable, none pending); 0 differences with
  `nextcloudcmd` v34.0.5 in the 19-step side-by-side bench and its round trip
  (see docs/test-parity.md).
- Not supported: virtual files, end-to-end encryption, Windows and macOS;
  bulk upload is ported but off, as upstream. See the README's Limitations.
- Release binaries for Linux x86_64 and aarch64 (glibc 2.35 or later), with
  the systemd units, `SHA256SUMS`, and
  GitHub build provenance attestations.
