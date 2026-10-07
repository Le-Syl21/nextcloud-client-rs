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

Phase 1: one-shot synchronization at `nextcloudcmd` parity.

| Crate | Mirrors upstream | Content |
|---|---|---|
| `nc-journal` | `src/common`, `src/csync` | journal (`SyncJournalDb`), `c_jhash64`, exclude engine, checksums, remote permissions |
| `nc-dav` | `src/libsync` (network layer) | HTTP transport (reqwest), `QNetworkReply` error model, PROPFIND parser, account, capabilities, network jobs |
| `nc-sync` | `src/libsync` (engine) | discovery, reconciliation, propagator (downloads with resume, uploads v1 and chunked v2, remote and local operations, conflicts), sync engine |
| `ncsync` | `src/cmd` | `ncsync sync`, the `nextcloudcmd` equivalent |
| `nc-testutils` | `test/syncenginetestutils.*` | FakeFolder harness (in-memory server behind the transport trait) and the ported FakeFolder tests |

Out of scope for now: virtual files, end-to-end encryption (encrypted folders
are skipped like the official client does without E2EE), bulk upload (also
disabled upstream), the GUI.

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
`--max-deletions`, `--password-file`. Not enforced yet: `--uplimit` / `--downlimit`.

The journal is the official client's `.sync_xxxxxxxxxxxx.db`, named the same
way, in the synchronized folder.

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
  the few upstream test files with another header (`testcapabilities.cpp`
  GPL-2.0-or-later, `testlongpath.cpp` LGPL-2.1-or-later), whose ports keep it.

See `REUSE.toml` and the `LICENSES/` directory.

## Building and testing

```sh
cargo build --release -p ncsync
cargo test --workspace
tools/itest/run.sh      # end-to-end run against a throw-away Nextcloud container (Docker)
tools/itest/run.sh --down
```
