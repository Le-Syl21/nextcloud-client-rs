# Changelog

All notable changes to this project are listed here. Versions follow
[Semantic Versioning](https://semver.org/); the behavioural reference is the
official Nextcloud desktop client v34.0.5.

## Unreleased

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
