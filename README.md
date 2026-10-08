# nextcloud-client-rs

**Unofficial** Rust port of the sync engine of the Nextcloud desktop client, as a
command-line tool (`ncsync`) and a headless daemon (`ncsyncd`).

This project is not affiliated with, endorsed by, or supported by Nextcloud GmbH.
"Nextcloud" is a trademark of Nextcloud GmbH and is used here only to describe
the server this software talks to.

## Goal

Behave exactly like the official client (same discovery, reconciliation and
propagation decisions, same on-disk journal `.sync_xxxxxx.db`), so that a folder
can be handed over between the official client and this one. Only the CLI and
the daemon are in scope: no GUI, no shell integration, no virtual files and no
end-to-end encryption in v1.

The behavioural reference ("oracle") is the upstream
[nextcloud/desktop](https://github.com/nextcloud/desktop) tag **v34.0.5**
(commit `62ebad6043b1e7c8e319f41be25d41f5a2c733e5`). Upstream tests are ported
one-to-one by name; see [docs/test-parity.md](docs/test-parity.md). Where a
reputable crate gives the exact upstream behaviour it is used instead of a
port; see [docs/crates-vs-port.md](docs/crates-vs-port.md).

## Status

Phase 1: one-shot synchronization at `nextcloudcmd` parity (`ncsync sync`).
Phase 2: the daemon `ncsyncd`, with the official client's sync logic
(folder watcher, etag polling or notify_push, scheduling and back-off,
several accounts and folders), its `nextcloud.cfg` configuration format,
folder takeover and hand-back, and systemd services.
Phase 3 (in progress): server-side file locking, as in the official client.

| Crate | Mirrors upstream | Content |
|---|---|---|
| `nc-journal` | `src/common`, `src/csync` | journal (`SyncJournalDb`), `c_jhash64`, exclude engine, checksums, remote permissions |
| `nc-dav` | `src/libsync` (network layer) | HTTP transport (reqwest), `QNetworkReply` error model, PROPFIND parser, account, capabilities, network jobs |
| `nc-sync` | `src/libsync` (engine) | discovery, reconciliation, propagator (downloads with resume, uploads v1 and chunked v2, bulk upload (off by default), remote and local operations, conflicts), sync engine, sync file status tracker |
| `nc-daemon` | `src/gui` (sync logic, no GUI) | folder manager, folders, inotify folder watcher, account state and connection validator, `nextcloud.cfg` settings, credentials, Login Flow v2, takeover, control socket, event loop |
| `ncsync` | `src/cmd` | `ncsync sync` (the `nextcloudcmd` equivalent), configuration commands, daemon control |
| `ncsyncd` | `src/gui/application.cpp` | the daemon |
| `nc-testutils` | `test/syncenginetestutils.*` | FakeFolder harness (in-memory server behind the transport trait) and the ported FakeFolder tests |

Phase 3 (in progress): bulk upload (`BulkPropagatorJob`, one
`POST /remote.php/dav/bulk` for many small files) is ported but **off by
default, like upstream v34.0.5**, which disables it; the experimental
`SyncOptions::bulk_upload` opt-in (not exposed by `ncsync`) enables it. The
sync file status tracker (the overlay icon statuses) is ported in `nc-sync`.

Out of scope for now: virtual files, end-to-end encryption (encrypted folders
are skipped like the official client does without E2EE), the GUI.

## Usage

```sh
ncsync sync [OPTIONS] <source_dir> <server_url>
```

The options are those of `nextcloudcmd` (`-u`, `-p`, `-n`, `--non-interactive`,
`--exclude`, `--unsyncedfolders`, `--path`, `--trust`, `--httpproxy`,
`--max-sync-retries`, `-h` for hidden files, `-s`, `--logdebug`, ...); see
`ncsync sync --help`. Prefer an app password, given with `--password-file`
or `NC_PASSWORD` (with `--non-interactive`) rather than `-p`. Extensions: `--new-big-folder-size-limit`,
`--confirm-external-storage`, `--abort-on-mass-deletion` and
`--max-deletions`, `--password-file`.

The journal is the official client's `.sync_xxxxxxxxxxxx.db`, named the same
way, in the synchronized folder.

## The daemon: `ncsyncd`

`ncsyncd` syncs every folder of its configuration continuously, like the
official desktop client without its GUI:

* local changes are seen by inotify (and a full local scan every hour, as
  upstream); remote changes by notify_push ("Client Push") when the server
  has it, otherwise by checking the remote etag every 30 s;
* one folder syncs at a time, failed syncs are retried with upstream's
  back-off, a folder is fully synced every 2 hours anyway;
* the configuration file has the format of the official client's
  `nextcloud.cfg` (same groups and keys), so folders can move between the
  two clients without losing their journal (`ncsync takeover` /
  `ncsync handback`);
* a folder that the official client's configuration also lists is never
  synced (never run both clients on one folder): take it over first;
* folders in virtual files mode are refused;
* when the server has file locking (the `files_lock` app), a document
  opened in an office application (LibreOffice/Office `.~lock.*#` and `~$*`
  lock files, AutoCAD, Adobe InDesign/InCopy/Premiere and Affinity lock
  files) is locked on the server while its lock file exists, and unlocked
  when the application removes it; a file locked by someone else is made
  read-only locally.

### Setting it up (user service)

```sh
ncsync account add https://cloud.example.com          # Login Flow v2: open the URL (or scan the QR code)
ncsync account add https://cloud.example.com -u alice --app-password-file ~/app-password   # or an app password
ncsync folder add ~/Nextcloud                         # --remote /Photos to sync a subfolder; --account ID
ncsync folder list
install -Dm644 contrib/systemd/ncsyncd.service ~/.config/systemd/user/ncsyncd.service
systemctl --user daemon-reload && systemctl --user enable --now ncsyncd
loginctl enable-linger "$USER"                        # keep syncing when logged out
```

The configuration is `~/.config/ncsyncd/ncsyncd.cfg`; the app password goes
to the Secret Service (service `ncsyncd`, key `<login>:<url>/:<account id>`,
the official client's key layout), or, when there is no keyring (a headless
server), to `~/.local/state/ncsyncd/credentials/ncsyncd-<id>` (mode 0600),
referenced by the account's `ncsyncd_passwordFile` key.

A configuration made by these commands looks like this (any key of the
official client's `nextcloud.cfg` is understood, e.g. `[Nextcloud]
remotePollInterval`, `forceSyncInterval`, `fullLocalDiscoveryInterval`,
`newBigFolderSizeLimit`, `moveToTrash`, the per-account bandwidth limits):

```ini
[Accounts]
version=13
0\version=13
0\url=https://cloud.example.com
0\authType=webflow
0\webflow_user=alice
0\dav_user=alice
0\networkUploadLimitSetting=1
0\networkUploadLimit=500
0\Folders\1\localPath=/home/alice/Nextcloud/
0\Folders\1\journalPath=.sync_cf3fcd105d51.db
0\Folders\1\targetPath=/
0\Folders\1\paused=false
0\Folders\1\ignoreHiddenFiles=false
0\Folders\1\virtualFilesMode=off
0\Folders\1\version=2

[Nextcloud]
remotePollInterval=30000
```

(`networkUploadLimitSetting=1` with `networkUploadLimit=500`: 500 KB/s.)
Edit the file while the daemon is stopped, or reload it with
`systemctl --user restart ncsyncd`; `systemctl --user reload ncsyncd`
(SIGHUP) re-reads the credentials only.

### Setting it up (system service, one per user)

The template `ncsyncd@.service` runs one daemon per user, as that user
(`User=%i`, never as root): `ncsyncd@alice` runs as `alice`, with its
configuration and state in `/var/lib/ncsyncd/alice/` (owned by `alice`).
It needs no login session and no keyring. Set it up as root:

```sh
install -Dm644 contrib/systemd/ncsyncd@.service /etc/systemd/system/ncsyncd@.service
ncsync account add https://cloud.example.com -u alice --app-password-file /root/app-password --instance alice
ncsync folder add /srv/data --remote /Server --instance alice
systemctl daemon-reload && systemctl enable --now ncsyncd@alice
```

The synchronized folders must belong to the user (`folder add` gives a
folder it creates to the user; everything `ncsync ... --instance alice`
writes in `/var/lib/ncsyncd/alice/` is given to `alice` too).

For a system instance the app password is never put in a keyring. It is
read, in this order, from the systemd credential `ncsyncd-<user>-<account id>`
(`$CREDENTIALS_DIRECTORY`), then from the account's `ncsyncd_passwordFile`
(`account add --instance alice` writes `/var/lib/ncsyncd/alice/credentials/ncsyncd-<id>`,
mode 0600). The unit imports the user's credentials from the systemd
credential store (`ImportCredential=ncsyncd-%i-*`, systemd 254 or later),
so with a credential, encrypted at rest with `systemd-creds`, the password
file is not needed:

```sh
systemd-creds encrypt --name=ncsyncd-alice-0 /root/app-password /etc/credstore.encrypted/ncsyncd-alice-0
# then remove the account's ncsyncd_passwordFile key and the file
systemctl restart ncsyncd@alice
```

The user name is part of the credential name so that one user's instance
never receives another user's secrets.

### Controlling it

```sh
ncsync status                 # accounts and folders (--json for the daemon's JSON answer)
ncsync pause [ALIAS|PATH]     # all folders without an argument
ncsync resume [ALIAS|PATH]
ncsync sync-now [ALIAS|PATH]  # like the tray's "Sync now"
ncsync status --instance alice  # a system instance (as root or alice)
```

The control socket is `$XDG_RUNTIME_DIR/ncsyncd/control.sock` (user) or
`/run/ncsyncd/<user>/control.sock` (system), mode 0600. The daemon
reports `READY`, a one-line `STATUS` and the watchdog to systemd
(`Type=notify`, `WatchdogSec=120`); the watchdog is fed from the event loop.

### Moving a folder from the official client

With the official client **stopped** (it rewrites its configuration when it
quits):

```sh
ncsync takeover ~/Nextcloud   # or the folder's alias in the official client
ncsync handback 1             # gives it back, restoring its original entry
```

`takeover` copies the account (with its app password from the official
client's keyring entry when it can be read, otherwise Login Flow v2 or
`--app-password-file`) and the folder entry, keeping the local path and the
journal, and removes the folder from the official configuration (a backup
of the official file is kept as `nextcloud.cfg.ncsyncd-bak`). Both commands
refuse while the official client runs.

## Divergences from upstream

One deliberate behavioural difference with the official client v34.0.5:

* **inotify queue overflow.** When the kernel's inotify queue overflows
  (`IN_Q_OVERFLOW`, more events than `fs.inotify.max_queued_events` before
  they are read), events are lost. Upstream's Linux folder watcher ignores
  that event, so changes made during the overflow go unnoticed until the
  next periodic full local discovery (`fullLocalDiscoveryInterval`, one
  hour by default). `ncsyncd` treats the overflow as lost changes: the next
  sync does a full local discovery, and a sync is scheduled right away
  (after the usual short delay), so nothing waits for the hourly scan.

## Licensing

The project as a whole is licensed under **GPL-2.0-or-later**, like the
upstream client, and every crate declares that licence (`nc-testutils`, which
is never published, is CC0-1.0 like upstream `test/`). Every ported file keeps
the copyright lines and SPDX header of the upstream file it was ported from:

* files ported from upstream `src/common` and `src/csync` that are
  LGPL-2.1-or-later keep their LGPL-2.1-or-later header (most of
  `nc-journal`); LGPL-2.1-or-later code may be distributed under the GPL, so
  the crate as a whole is GPL-2.0-or-later;
* a few upstream files in `src/common` and `src/csync` are GPL-2.0-or-later
  (`c_jhash.h`, `checksumcalculator.*`, `checksumconsts.h`) and so are their ports;
* files ported from `src/libsync` and `src/cmd` are GPL-2.0-or-later;
* ported tests and test utilities are CC0-1.0 like upstream `test/`, except
  the few upstream test files with another header (`testcapabilities.cpp`,
  `testpushnotifications.cpp` and `pushnotificationstestutils.*`
  GPL-2.0-or-later, `testlongpath.cpp` LGPL-2.1-or-later), whose ports keep it.

See `REUSE.toml` and the `LICENSES/` directory.

## Building and testing

```sh
cargo build --release -p ncsync -p ncsyncd
cargo test --workspace
tools/itest/run.sh          # ncsync sync end to end against a throw-away Nextcloud container (Docker)
tools/itest/run-daemon.sh   # ncsyncd end to end, with redis and notify_push
tools/itest/run.sh --down
```
