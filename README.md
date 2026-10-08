<img src=".github/flags/gb.svg" height="14" alt="GB"> [English](#english) | <img src=".github/flags/fr.svg" height="14" alt="FR"> [Français](#français)

# nextcloud-client-rs

[![License: GPL-2.0-or-later](https://img.shields.io/badge/license-GPL--2.0--or--later-blue.svg)](LICENSE)
[![Status: beta](https://img.shields.io/badge/status-beta-orange.svg)](#limitations)
[![Reference: nextcloud/desktop v34.0.5](https://img.shields.io/badge/reference-desktop%20v34.0.5-0082c9.svg)](https://github.com/nextcloud/desktop/tree/v34.0.5)
[![Discord](https://img.shields.io/badge/Discord-Le--Syl21%20Tools-5865F2?logo=discord&logoColor=white)](https://discord.gg/T37DYHmt2j)

## <a name="english"></a><img src=".github/flags/gb.svg" height="14" alt="GB"> English

**An unofficial Rust port of the sync engine of the Nextcloud desktop client:
a command-line tool, `ncsync`, and a headless daemon, `ncsyncd`.**

> **Beta.** It syncs, and it is checked against the official client (below), but it has
> not run for long on real desktops yet: see [Limitations](#limitations) before you trust
> it with data you have no other copy of. What is in it: [CHANGELOG.md](CHANGELOG.md).

This project is not affiliated with, endorsed by, or supported by Nextcloud GmbH.
"Nextcloud" is a trademark of Nextcloud GmbH, used here only to name the server this
software talks to; the project does not use the Nextcloud logo.

### What it is

- **`ncsync sync`** syncs a folder once, like `nextcloudcmd`, with the same options.
- **`ncsyncd`** syncs continuously, like the official desktop client without its window:
  it watches the folders (inotify), hears remote changes through notify_push (or polls),
  retries with the same back-off, handles several accounts and folders, and runs as a
  systemd user service or as one system service per user (`ncsyncd@alice`).
- **`ncsync account|folder|takeover|handback|status|pause|resume|sync-now`** set it up and
  drive it.

The goal is to **behave exactly like the official client**: same discovery,
reconciliation and propagation decisions, same journal on disk (`.sync_xxxxxxxxxxxx.db`),
same configuration format (`nextcloud.cfg`). The behavioural reference is the upstream
[nextcloud/desktop](https://github.com/nextcloud/desktop) tag **v34.0.5** (commit
`62ebad6043b1e7c8e319f41be25d41f5a2c733e5`). Because the journal and the configuration are
the official ones, a folder can move from the official client to `ncsyncd` and back
(**takeover / hand-back**) without a resync.

How close it is:

- **384 of the 472 upstream test functions** of the files covering the ported code are
  ported one to one, by name, and pass; the other 88 do not apply (GUI, Windows, macOS,
  Qt internals), none is pending. Every one is listed in
  [docs/test-parity.md](docs/test-parity.md).
- **0 differences in 19 scripted scenarios** run side by side with the official
  `nextcloudcmd` v34.0.5 against the same server (local trees, server trees and a dump of
  every journal table compared after every step), and in the round trip where the two
  clients alternate on one folder and journal: after every takeover the other client had
  nothing to do (19/19). Details at the end of [docs/test-parity.md](docs/test-parity.md).
- Where a well-known crate gives exactly the upstream behaviour it is used instead of a
  port (SQLite, HTTP, TLS, XML, hashes...): see [docs/crates-vs-port.md](docs/crates-vs-port.md).

### Install

**Release binaries** (Linux x86_64 and aarch64): download the archive from the
[releases page](https://github.com/Le-Syl21/nextcloud-client-rs/releases), check it against
`SHA256SUMS`, and install the two programs where the systemd units expect them:

```sh
tar xzf nextcloud-client-rs-linux-x86_64.tar.gz
cd nextcloud-client-rs-linux-x86_64
sudo install -m755 ncsync ncsyncd /usr/bin/
```

The units in `systemd/` start `/usr/bin/ncsyncd`; if you put it elsewhere
(`/usr/local/bin`, `~/.cargo/bin`), change their `ExecStart=`.

**From source** (Rust 1.88 or later; SQLite is built in, no system library is needed):

```sh
cargo install --locked --git https://github.com/Le-Syl21/nextcloud-client-rs ncsync ncsyncd
# or, from a clone:
cargo build --release -p ncsync -p ncsyncd   # target/release/ncsync, target/release/ncsyncd
```

### One-shot sync: `ncsync sync`

```sh
ncsync sync [OPTIONS] <source_dir> <server_url>
```

The options are those of `nextcloudcmd` (`-u`, `-p`, `-n`, `--non-interactive`,
`--exclude`, `--unsyncedfolders`, `--path`, `--trust`, `--httpproxy`,
`--max-sync-retries`, `-h` for hidden files, `-s`, `--logdebug`, ...); see
`ncsync sync --help`. Prefer an app password given with `--password-file` or `NC_PASSWORD`
(with `--non-interactive`) to `-p`, which other users can see in the process list.
Extensions: `--new-big-folder-size-limit`, `--confirm-external-storage`,
`--abort-on-mass-deletion` and `--max-deletions`, `--password-file`.

The journal is the official client's `.sync_xxxxxxxxxxxx.db`, named the same way, in the
synchronized folder.

#### Account provisioning (`nextcloudcmd --userid`)

With `--userid`, `ncsync sync` does what `nextcloudcmd`'s provisioning mode does: instead
of syncing, it adds an account (and a folder) to the `ncsyncd` configuration:

```sh
ncsync sync --userid alice --serverurl https://cloud.example.com --apppassword "$APP_PASSWORD" \
    [--localdirpath ~/Nextcloud] [--remotedirpath /Photos] [--isvfsenabled 0] [--confdir DIR]
```

* the app password is checked against the server (`ocs/v1.php/cloud/user`, then a
  PROPFIND) before anything is written: on failure nothing is stored and the exit code is 1;
* the account goes to `~/.config/ncsyncd/ncsyncd.cfg` (`DIR/ncsyncd.cfg` with
  `--confdir DIR`), the app password to the keyring or a password file like
  `ncsync account add` (`--password-file FILE` reads it from a file instead of the command
  line);
* the folder is `--localdirpath` (default `~/Nextcloud`, made unique), which must be
  missing or empty, synced with `--remotedirpath` (default `/`);
* without `--apppassword` the login is done with **Login Flow v2** in the terminal: the
  link (login name prefilled) and its QR code are printed, and the setup goes on once access
  is granted in a browser; with `--non-interactive` an app password is required instead
  (exit 255, nothing written). This differs from upstream, see
  [Divergences from upstream](#divergences-from-upstream);
* `--isvfsenabled 1` is refused (exit 255): virtual files are not supported;
* a rejected command line or setup (missing `--userid`/`--serverurl`, existing account,
  non-empty local folder) exits with 255, like `nextcloudcmd`'s `return -1`;
* `--trust` and `--httpproxy` apply to the setup too (every request, the Login Flow v2
  polling included), unlike upstream.

`--confdir DIR` also applies to the sync mode: the client status reporting database
(below) goes there.

### The daemon: `ncsyncd`

`ncsyncd` syncs every folder of its configuration continuously, like the official desktop
client without its GUI:

* local changes are seen by inotify (and a full local scan every hour, as upstream);
  remote changes by notify_push ("Client Push") when the server has it, otherwise by
  checking the remote etag every 30 s;
* one folder syncs at a time, failed syncs are retried with upstream's back-off, a folder
  is fully synced every 2 hours anyway;
* the configuration file has the format of the official client's `nextcloud.cfg` (same
  groups and keys), so folders can move between the two clients without losing their
  journal (`ncsync takeover` / `ncsync handback`);
* a folder that the official client's configuration also lists is never synced (never run
  both clients on one folder): take it over first;
* folders in virtual files mode are refused;
* when the server has file locking (the `files_lock` app), a document opened in an office
  application (LibreOffice/Office `.~lock.*#` and `~$*` lock files, AutoCAD, Adobe
  InDesign/InCopy/Premiere and Affinity lock files) is locked on the server while its lock
  file exists, and unlocked when the application removes it; a file locked by someone else
  is made read-only locally;
* a request refused for its credentials (HTTP 401) signs the account out, like the official
  client, and asks the server whether this device was wiped (`index.php/core/wipe/check`).
  When the server's administrator asked for a **remote wipe**, every folder of the account
  is deleted, local files included, the account and its stored app password are removed,
  and the server is told (`index.php/core/wipe/success`). Otherwise the account stays
  signed out until new credentials are given (`ncsync account add`, then
  `systemctl --user reload ncsyncd`);
* when the server enables the `security_guard` diagnostics, the counts of some sync
  failures (conflicts, server errors, viruses detected) are sent to it once a day, like the
  official client; they are kept meanwhile in `.userdata_<hash>.db` next to the
  configuration.

#### Setting it up (user service)

```sh
ncsync account add https://cloud.example.com          # Login Flow v2: open the URL (or scan the QR code)
ncsync account add https://cloud.example.com -u alice --app-password-file ~/app-password   # or an app password
ncsync folder add ~/Nextcloud                         # --remote /Photos to sync a subfolder; --account ID
ncsync folder list
install -Dm644 contrib/systemd/ncsyncd.service ~/.config/systemd/user/ncsyncd.service
systemctl --user daemon-reload && systemctl --user enable --now ncsyncd
loginctl enable-linger "$USER"                        # keep syncing when logged out
```

(In a release archive the units are in `systemd/`, not `contrib/systemd/`.)

The configuration is `~/.config/ncsyncd/ncsyncd.cfg`; the app password goes to the Secret
Service (service `ncsyncd`, key `<login>:<url>/:<account id>`, the official client's key
layout), or, when there is no keyring (a headless server), to
`~/.local/state/ncsyncd/credentials/ncsyncd-<id>` (mode 0600), referenced by the account's
`ncsyncd_passwordFile` key.

A configuration made by these commands looks like this (any key of the official client's
`nextcloud.cfg` is understood, e.g. `[Nextcloud] remotePollInterval`, `forceSyncInterval`,
`fullLocalDiscoveryInterval`, `newBigFolderSizeLimit`, `moveToTrash`, the per-account
bandwidth limits):

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

(`networkUploadLimitSetting=1` with `networkUploadLimit=500`: 500 KB/s.) Edit the file
while the daemon is stopped, or reload it with `systemctl --user restart ncsyncd`;
`systemctl --user reload ncsyncd` (SIGHUP) re-reads the credentials only.

#### Setting it up (system service, one per user)

The template `ncsyncd@.service` runs one daemon per user, as that user (`User=%i`, never as
root): `ncsyncd@alice` runs as `alice`, with its configuration and state in
`/var/lib/ncsyncd/alice/` (owned by `alice`). It needs no login session and no keyring.
Set it up as root:

```sh
install -Dm644 contrib/systemd/ncsyncd@.service /etc/systemd/system/ncsyncd@.service
ncsync account add https://cloud.example.com -u alice --app-password-file /root/app-password --instance alice
ncsync folder add /srv/data --remote /Server --instance alice
systemctl daemon-reload && systemctl enable --now ncsyncd@alice
```

The synchronized folders must belong to the user (`folder add` gives a folder it creates to
the user; everything `ncsync ... --instance alice` writes in `/var/lib/ncsyncd/alice/` is
given to `alice` too).

For a system instance the app password is never put in a keyring. It is read, in this
order, from the systemd credential `ncsyncd-<user>-<account id>` (`$CREDENTIALS_DIRECTORY`),
then from the account's `ncsyncd_passwordFile` (`account add --instance alice` writes
`/var/lib/ncsyncd/alice/credentials/ncsyncd-<id>`, mode 0600). The unit imports the user's
credentials from the systemd credential store (`ImportCredential=ncsyncd-%i-*`, systemd
254 or later), so with a credential encrypted at rest by `systemd-creds` the password file
is not needed:

```sh
systemd-creds encrypt --name=ncsyncd-alice-0 /root/app-password /etc/credstore.encrypted/ncsyncd-alice-0
# then remove the account's ncsyncd_passwordFile key and the file
systemctl restart ncsyncd@alice
```

The user name is part of the credential name to keep users apart:
`ImportCredential=ncsyncd-%i-*` makes systemd hand `ncsyncd@alice` only the credentials
whose name starts with `ncsyncd-alice-`, so one user's instance never receives another
user's app passwords, although all of them sit in the same credential store. The account id
then picks the account within the user's configuration.

#### Controlling it

```sh
ncsync status                   # accounts and folders (--json for the daemon's JSON answer)
ncsync pause [ALIAS|PATH]       # all folders without an argument
ncsync resume [ALIAS|PATH]
ncsync sync-now [ALIAS|PATH]    # like the tray's "Sync now"
ncsync status --instance alice  # a system instance (as root or alice)
```

The control socket is `$XDG_RUNTIME_DIR/ncsyncd/control.sock` (user) or
`/run/ncsyncd/<user>/control.sock` (system), mode 0600. The daemon reports `READY`, a
one-line `STATUS` and the watchdog to systemd (`Type=notify`, `WatchdogSec=120`); the
watchdog is fed from the event loop, so a stuck loop gets the service restarted.

#### Moving a folder from the official client

With the official client **stopped** (it rewrites its configuration when it quits):

```sh
ncsync takeover ~/Nextcloud   # or the folder's alias in the official client
ncsync handback 1             # gives it back, restoring its original entry
```

`takeover` copies the account (with its app password from the official client's keyring
entry when it can be read, otherwise Login Flow v2 or `--app-password-file`) and the folder
entry, keeping the local path and the journal, and removes the folder from the official
configuration (a backup of the official file is kept as `nextcloud.cfg.ncsyncd-bak`). Both
commands refuse while the official client runs.

### Divergences from upstream

Deliberate behavioural differences with the official client v34.0.5:

* **inotify queue overflow.** When the kernel's inotify queue overflows (`IN_Q_OVERFLOW`),
  events are lost. Upstream's Linux folder watcher ignores that event, so the changes made
  meanwhile wait for the next full local discovery (one hour by default). `ncsyncd` treats
  the overflow as lost changes: the next sync does a full local discovery, and it is
  scheduled right away.
* **Provisioning without an app password.** `nextcloudcmd --userid ... --serverurl ...`
  without `--apppassword` stores the account without credentials, for the desktop client's
  GUI to ask for a login later. A command line has no such later, and an account without
  credentials would only be signed out by `ncsyncd`; so `ncsync sync --userid ...` logs in
  right away with Login Flow v2 (link and QR code in the terminal), and with
  `--non-interactive` refuses the setup (exit 255) and asks for an app password.
* **`--trust` in provisioning mode.** Upstream parses `--trust` but does not apply it to the
  account setup, so a server with a self-signed certificate cannot be provisioned from the
  command line; `ncsync` applies it to every request of the setup.
* **`--httpproxy` in provisioning mode.** Upstream parses `--httpproxy` but does not apply
  it to the account setup; `ncsync` sends every request of the setup through it
  (`status.php`, Login Flow v2 and its polling, `ocs/v1.php/cloud/user`, the PROPFIND).

### Limitations

- **Linux only.** The daemon is built on Linux facilities: inotify to watch the folders,
  systemd for the services, the credential store and the watchdog, the Secret Service
  (GNOME Keyring, KWallet) for app passwords. The one-shot `ncsync sync` is not offered on
  Windows or macOS either: the official client has code paths of its own on those systems
  (file names in Unicode NFC on macOS, file attributes, long paths and case-insensitive
  names on Windows), which are not ported, so "the same behaviour as the official client"
  could not be claimed there.
- **No end-to-end encryption.** Encrypted folders are skipped, as the official client does
  when E2EE is not set up.
- **No virtual files** (files that are downloaded on first use). Folders in virtual files
  mode are refused, by `ncsyncd` and by `--isvfsenabled 1`.
- **No GUI, no tray icon, no file manager integration.** `ncsync status` and the
  systemd journal are the way to see what it does.
- **Bulk upload is off**, as in v34.0.5, which disables it: it is ported (one
  `POST /remote.php/dav/bulk` for many small files) but not exposed.
- **Not verified on a real desktop yet** (everything below is covered by tests and
  containers, not by everyday use):
  - storing and reading app passwords in a real Secret Service keyring (GNOME Keyring,
    KWallet);
  - reading the official client's app password from its keyring entry (QtKeychain) during
    `ncsync takeover`; if it cannot be read, Login Flow v2 or `--app-password-file` is used
    instead;
  - a long run under a real systemd (user service with linger, `ncsyncd@` with
    `ImportCredential=` and `systemd-creds`).
- **Beta.** The version is `0.x`: the command line and the files under
  `~/.config/ncsyncd` may still change. Keep another copy of what matters, and please
  report what you find (see [Help and feedback](#help-and-feedback)).

### Building and testing

```sh
cargo build --release -p ncsync -p ncsyncd
cargo test --workspace
tools/itest/run.sh          # ncsync sync end to end against a throw-away Nextcloud container (Docker)
tools/itest/run-daemon.sh   # ncsyncd end to end, with redis and notify_push
tools/itest/run.sh --down
```

The integration stack is its own compose project (`ncrs-itest`, loopback port 18080) and
never touches other containers. The work directories default to `~/.cache/ncrs-itest*`
(first argument of the scripts).

The side-by-side bench runs the official `nextcloudcmd` of the pinned tag and `ncsync`
through the same scripted scenarios against the test server (each with its own user) and
compares the local trees, the server trees and a normalized dump of the journals after
every step; `--roundtrip` alternates the two clients on one folder and journal (a takeover
and a hand-back at every step) and checks that the other client then has nothing to do:

```sh
tools/bench/build-oracle.sh          # once: nextcloudcmd v34.0.5 in the upstream CI image (Docker)
docker compose -f tools/itest/compose.yml -p ncrs-itest up -d
tools/bench/bench.py                 # side by side
tools/bench/bench.py --roundtrip     # one folder, alternating clients
```

The oracle goes to `~/.cache/ncrs-oracle/nccmd-34.0.5` (`build-oracle.sh DIR`, or
`NCRS_ORACLE` for the bench), the bench work to `~/.cache/ncrs-bench` (`NCRS_BENCH_WORK`).

Layout of the workspace:

| Crate | Mirrors upstream | Content |
|---|---|---|
| `nc-journal` | `src/common`, `src/csync` | journal (`SyncJournalDb`), `c_jhash64`, exclude engine, checksums, remote permissions |
| `nc-dav` | `src/libsync` (network layer) | HTTP transport (reqwest), `QNetworkReply` error model, PROPFIND parser, account, capabilities, network jobs |
| `nc-sync` | `src/libsync` (engine) | discovery, reconciliation, propagator (downloads with resume, uploads v1 and chunked v2, bulk upload, remote and local operations, conflicts, file locking), sync engine, sync file status tracker |
| `nc-daemon` | `src/gui` (sync logic, no GUI) | folder manager, folders, inotify folder watcher, account state and connection validator, `nextcloud.cfg` settings, credentials, Login Flow v2, takeover, remote wipe, control socket, event loop |
| `ncsync` | `src/cmd` | `ncsync sync` (the `nextcloudcmd` equivalent), configuration commands, daemon control |
| `ncsyncd` | `src/gui/application.cpp` | the daemon |
| `nc-testutils` | `test/syncenginetestutils.*` | FakeFolder harness (in-memory server behind the transport trait) and the ported FakeFolder tests |

### Help and feedback

- Project: <https://github.com/Le-Syl21/nextcloud-client-rs> (issues welcome)
- Discord: <https://discord.gg/T37DYHmt2j>

Please report problems here, not to Nextcloud: the official client's developers do not
support this port.

### License

**GPL-2.0-or-later** (see [LICENSE](LICENSE)), like the upstream client, and every crate
declares it (`nc-testutils`, never published, is CC0-1.0 like upstream `test/`). Every
ported file keeps the copyright lines and SPDX header of the upstream file it was ported
from:

* files ported from upstream `src/common` and `src/csync` that are LGPL-2.1-or-later keep
  their LGPL-2.1-or-later header (most of `nc-journal`); LGPL-2.1-or-later code may be
  distributed under the GPL, so the crate as a whole is GPL-2.0-or-later;
* a few upstream files in `src/common` and `src/csync` are GPL-2.0-or-later (`c_jhash.h`,
  `checksumcalculator.*`, `checksumconsts.h`) and so are their ports;
* files ported from `src/libsync`, `src/gui` and `src/cmd` are GPL-2.0-or-later;
* ported tests and test utilities are CC0-1.0 like upstream `test/`, except the few
  upstream test files with another header (`testcapabilities.cpp`,
  `testpushnotifications.cpp`, `pushnotificationstestutils.*` and
  `testclientstatusreporting.cpp` GPL-2.0-or-later, `testlongpath.cpp`
  LGPL-2.1-or-later), whose ports keep it.

The project follows [REUSE](https://reuse.software/): see `REUSE.toml` and `LICENSES/`.

### Credits

This is a port: the design, the decisions and the tests are those of the
[Nextcloud desktop client](https://github.com/nextcloud/desktop), by Nextcloud GmbH and the
Nextcloud contributors, built on the ownCloud client by ownCloud GmbH and its contributors,
and on csync. Their copyright lines are kept in every ported file. Thanks to them; any bug
here is the port's.

"Nextcloud" is a trademark of Nextcloud GmbH. nextcloud-client-rs is an independent
project, neither affiliated with nor endorsed by Nextcloud GmbH.

---

## <a name="français"></a><img src=".github/flags/fr.svg" height="14" alt="FR"> Français

**Un portage non officiel en Rust du moteur de synchronisation du client de bureau
Nextcloud : un outil en ligne de commande, `ncsync`, et un service sans interface,
`ncsyncd`.**

> **Bêta.** Il synchronise, et il est comparé au client officiel (voir plus bas), mais il
> n'a pas encore tourné longtemps sur de vrais postes : lisez les [Limites](#limites) avant
> de lui confier des données dont vous n'avez pas d'autre copie. Ce qu'il contient :
> [CHANGELOG.md](CHANGELOG.md).

Ce projet n'est ni affilié à Nextcloud GmbH, ni approuvé ni pris en charge par elle.
« Nextcloud » est une marque de Nextcloud GmbH, citée ici uniquement pour nommer le serveur
auquel ce logiciel parle ; le projet n'utilise pas le logo Nextcloud.

### Ce que c'est

- **`ncsync sync`** synchronise un dossier une fois, comme `nextcloudcmd`, avec les mêmes
  options.
- **`ncsyncd`** synchronise en continu, comme le client de bureau officiel sans sa
  fenêtre : il surveille les dossiers (inotify), apprend les changements distants par
  notify_push (ou en interrogeant le serveur), réessaie avec les mêmes délais, gère
  plusieurs comptes et dossiers, et tourne en service systemd utilisateur ou en un service
  système par utilisateur (`ncsyncd@alice`).
- **`ncsync account|folder|takeover|handback|status|pause|resume|sync-now`** le configurent
  et le pilotent.

Le but est de **se comporter exactement comme le client officiel** : mêmes décisions de
découverte, de réconciliation et de propagation, même journal sur le disque
(`.sync_xxxxxxxxxxxx.db`), même format de configuration (`nextcloud.cfg`). La référence est
le tag **v34.0.5** de [nextcloud/desktop](https://github.com/nextcloud/desktop) (commit
`62ebad6043b1e7c8e319f41be25d41f5a2c733e5`). Comme le journal et la configuration sont ceux
du client officiel, un dossier peut passer du client officiel à `ncsyncd` et revenir
(**reprise / restitution**, `takeover` / `handback`) sans tout resynchroniser.

À quel point il est proche :

- **384 des 472 fonctions de test amont** des fichiers qui couvrent le code porté sont
  portées une à une, sous le même nom, et passent ; les 88 autres ne s'appliquent pas
  (interface graphique, Windows, macOS, mécanique interne de Qt), aucune n'est en attente.
  Toutes sont listées dans [docs/test-parity.md](docs/test-parity.md).
- **0 différence sur 19 scénarios** joués côte à côte avec le `nextcloudcmd` officiel
  v34.0.5 contre le même serveur (arborescences locales, arborescences du serveur et
  contenu de chaque table des journaux comparés après chaque étape), et dans l'aller-retour
  où les deux clients se relaient sur un même dossier et un même journal : après chaque
  reprise, l'autre client n'avait rien à faire (19/19). Détails à la fin de
  [docs/test-parity.md](docs/test-parity.md).
- Quand une bibliothèque Rust reconnue donne exactement le comportement amont, elle est
  utilisée plutôt qu'un portage (SQLite, HTTP, TLS, XML, empreintes...) : voir
  [docs/crates-vs-port.md](docs/crates-vs-port.md).

### Installation

**Binaires des versions publiées** (Linux x86_64 et aarch64) : téléchargez l'archive sur la
[page des versions](https://github.com/Le-Syl21/nextcloud-client-rs/releases), vérifiez-la
avec `SHA256SUMS`, puis installez les deux programmes là où les unités systemd les
attendent :

```sh
tar xzf nextcloud-client-rs-linux-x86_64.tar.gz
cd nextcloud-client-rs-linux-x86_64
sudo install -m755 ncsync ncsyncd /usr/bin/
```

Les unités du dossier `systemd/` lancent `/usr/bin/ncsyncd` ; si vous le placez ailleurs
(`/usr/local/bin`, `~/.cargo/bin`), modifiez leur `ExecStart=`.

**Depuis les sources** (Rust 1.88 ou plus récent ; SQLite est intégré, aucune bibliothèque
système n'est nécessaire) :

```sh
cargo install --locked --git https://github.com/Le-Syl21/nextcloud-client-rs ncsync ncsyncd
# ou, depuis un clone :
cargo build --release -p ncsync -p ncsyncd   # target/release/ncsync, target/release/ncsyncd
```

### Synchronisation ponctuelle : `ncsync sync`

```sh
ncsync sync [OPTIONS] <dossier_local> <url_du_serveur>
```

Les options sont celles de `nextcloudcmd` (`-u`, `-p`, `-n`, `--non-interactive`,
`--exclude`, `--unsyncedfolders`, `--path`, `--trust`, `--httpproxy`,
`--max-sync-retries`, `-h` pour les fichiers cachés, `-s`, `--logdebug`...) ; voir
`ncsync sync --help`. Préférez un mot de passe d'application passé par `--password-file`
ou `NC_PASSWORD` (avec `--non-interactive`) à `-p`, visible des autres utilisateurs dans la
liste des processus. Ajouts : `--new-big-folder-size-limit`, `--confirm-external-storage`,
`--abort-on-mass-deletion` et `--max-deletions`, `--password-file`.

Le journal est le `.sync_xxxxxxxxxxxx.db` du client officiel, nommé de la même façon, dans
le dossier synchronisé.

#### Création de compte (`nextcloudcmd --userid`)

Avec `--userid`, `ncsync sync` fait ce que fait le mode de création de compte de
`nextcloudcmd` : au lieu de synchroniser, il ajoute un compte (et un dossier) à la
configuration de `ncsyncd` :

```sh
ncsync sync --userid alice --serverurl https://cloud.example.com --apppassword "$APP_PASSWORD" \
    [--localdirpath ~/Nextcloud] [--remotedirpath /Photos] [--isvfsenabled 0] [--confdir DIR]
```

* le mot de passe d'application est vérifié auprès du serveur (`ocs/v1.php/cloud/user`,
  puis un PROPFIND) avant toute écriture : en cas d'échec rien n'est enregistré et le code
  de sortie est 1 ;
* le compte va dans `~/.config/ncsyncd/ncsyncd.cfg` (`DIR/ncsyncd.cfg` avec
  `--confdir DIR`), le mot de passe dans le trousseau ou dans un fichier, comme avec
  `ncsync account add` (`--password-file FICHIER` le lit dans un fichier plutôt que sur la
  ligne de commande) ;
* le dossier est `--localdirpath` (par défaut `~/Nextcloud`, rendu unique), qui doit être
  absent ou vide, synchronisé avec `--remotedirpath` (par défaut `/`) ;
* sans `--apppassword`, la connexion se fait par **Login Flow v2** dans le terminal : le
  lien (identifiant prérempli) et son QR code s'affichent, et la création continue une fois
  l'accès accordé dans un navigateur ; avec `--non-interactive`, un mot de passe
  d'application est exigé (sortie 255, rien n'est écrit). C'est un écart avec l'amont, voir
  [Écarts avec l'amont](#écarts-avec-lamont) ;
* `--isvfsenabled 1` est refusé (sortie 255) : les fichiers virtuels ne sont pas pris en
  charge ;
* une ligne de commande ou une création refusée (`--userid`/`--serverurl` manquant, compte
  existant, dossier local non vide) sort avec 255, comme le `return -1` de `nextcloudcmd` ;
* `--trust` et `--httpproxy` s'appliquent aussi à la création (chaque requête, y compris
  l'attente du Login Flow v2), contrairement à l'amont.

`--confdir DIR` vaut aussi pour la synchronisation : la base des rapports d'état du client
(voir plus bas) y est rangée.

### Le service : `ncsyncd`

`ncsyncd` synchronise en continu tous les dossiers de sa configuration, comme le client de
bureau officiel sans son interface :

* les changements locaux sont vus par inotify (plus un parcours local complet toutes les
  heures, comme l'amont) ; les changements distants par notify_push (« Client Push ») quand
  le serveur l'a, sinon en vérifiant l'etag distant toutes les 30 s ;
* un seul dossier se synchronise à la fois, les échecs sont réessayés avec les délais de
  l'amont, et chaque dossier est de toute façon entièrement resynchronisé toutes les 2 h ;
* le fichier de configuration a le format du `nextcloud.cfg` du client officiel (mêmes
  groupes et clés), ce qui permet de passer un dossier d'un client à l'autre sans perdre son
  journal (`ncsync takeover` / `ncsync handback`) ;
* un dossier que la configuration du client officiel liste aussi n'est jamais synchronisé
  (ne faites jamais tourner les deux clients sur un même dossier) : reprenez-le d'abord ;
* les dossiers en mode fichiers virtuels sont refusés ;
* quand le serveur gère le verrouillage de fichiers (l'application `files_lock`), un
  document ouvert dans une suite bureautique (fichiers de verrou `.~lock.*#` et `~$*` de
  LibreOffice/Office, verrous d'AutoCAD, d'Adobe InDesign/InCopy/Premiere et d'Affinity) est
  verrouillé sur le serveur tant que son fichier de verrou existe, puis déverrouillé quand
  l'application le supprime ; un fichier verrouillé par quelqu'un d'autre passe en lecture
  seule en local ;
* une requête refusée pour ses identifiants (HTTP 401) déconnecte le compte, comme le
  client officiel, et demande au serveur si cet appareil doit être effacé
  (`index.php/core/wipe/check`). Si l'administrateur du serveur a demandé un **effacement à
  distance**, tous les dossiers du compte sont supprimés, fichiers locaux compris, le compte
  et son mot de passe enregistré sont retirés, et le serveur en est informé
  (`index.php/core/wipe/success`). Sinon, le compte reste déconnecté jusqu'à ce que de
  nouveaux identifiants soient fournis (`ncsync account add`, puis
  `systemctl --user reload ncsyncd`) ;
* quand le serveur active les diagnostics `security_guard`, le nombre de certains échecs de
  synchronisation (conflits, erreurs du serveur, virus détectés) lui est envoyé une fois par
  jour, comme le fait le client officiel ; ils sont conservés en attendant dans
  `.userdata_<hash>.db`, à côté de la configuration.

#### Mise en place (service utilisateur)

```sh
ncsync account add https://cloud.example.com          # Login Flow v2 : ouvrez le lien (ou scannez le QR code)
ncsync account add https://cloud.example.com -u alice --app-password-file ~/app-password   # ou un mot de passe d'application
ncsync folder add ~/Nextcloud                         # --remote /Photos pour un sous-dossier ; --account ID
ncsync folder list
install -Dm644 contrib/systemd/ncsyncd.service ~/.config/systemd/user/ncsyncd.service
systemctl --user daemon-reload && systemctl --user enable --now ncsyncd
loginctl enable-linger "$USER"                        # continuer à synchroniser une fois déconnecté
```

(Dans une archive publiée, les unités sont dans `systemd/` et non `contrib/systemd/`.)

La configuration est `~/.config/ncsyncd/ncsyncd.cfg` ; le mot de passe d'application va
dans le Secret Service (service `ncsyncd`, clé `<identifiant>:<url>/:<id du compte>`, la
disposition des clés du client officiel), ou, en l'absence de trousseau (un serveur sans
écran), dans `~/.local/state/ncsyncd/credentials/ncsyncd-<id>` (droits 0600), indiqué par la
clé `ncsyncd_passwordFile` du compte.

Une configuration créée par ces commandes ressemble à ceci (toute clé du `nextcloud.cfg`
officiel est comprise, par exemple `[Nextcloud] remotePollInterval`, `forceSyncInterval`,
`fullLocalDiscoveryInterval`, `newBigFolderSizeLimit`, `moveToTrash`, les limites de débit
par compte) :

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

(`networkUploadLimitSetting=1` avec `networkUploadLimit=500` : 500 Ko/s.) Modifiez le
fichier service arrêté, ou faites-le relire avec `systemctl --user restart ncsyncd` ;
`systemctl --user reload ncsyncd` (SIGHUP) ne relit que les identifiants.

#### Mise en place (service système, un par utilisateur)

Le modèle `ncsyncd@.service` lance un service par utilisateur, sous cet utilisateur
(`User=%i`, jamais root) : `ncsyncd@alice` tourne sous `alice`, avec sa configuration et
son état dans `/var/lib/ncsyncd/alice/` (appartenant à `alice`). Il n'a besoin ni d'une
session ouverte ni d'un trousseau. Mise en place, en root :

```sh
install -Dm644 contrib/systemd/ncsyncd@.service /etc/systemd/system/ncsyncd@.service
ncsync account add https://cloud.example.com -u alice --app-password-file /root/app-password --instance alice
ncsync folder add /srv/data --remote /Server --instance alice
systemctl daemon-reload && systemctl enable --now ncsyncd@alice
```

Les dossiers synchronisés doivent appartenir à l'utilisateur (`folder add` lui donne le
dossier qu'il crée ; tout ce que `ncsync ... --instance alice` écrit dans
`/var/lib/ncsyncd/alice/` est aussi donné à `alice`).

Pour un service système, le mot de passe d'application n'est jamais mis dans un trousseau.
Il est lu, dans cet ordre, dans l'identifiant systemd `ncsyncd-<utilisateur>-<id du compte>`
(`$CREDENTIALS_DIRECTORY`), puis dans le `ncsyncd_passwordFile` du compte
(`account add --instance alice` écrit `/var/lib/ncsyncd/alice/credentials/ncsyncd-<id>`,
droits 0600). L'unité importe les identifiants de l'utilisateur depuis le magasin
d'identifiants de systemd (`ImportCredential=ncsyncd-%i-*`, systemd 254 ou plus récent) :
avec un identifiant chiffré par `systemd-creds`, le fichier de mot de passe devient
inutile :

```sh
systemd-creds encrypt --name=ncsyncd-alice-0 /root/app-password /etc/credstore.encrypted/ncsyncd-alice-0
# puis retirez la clé ncsyncd_passwordFile du compte, et le fichier
systemctl restart ncsyncd@alice
```

Le nom d'utilisateur fait partie du nom de l'identifiant pour séparer les utilisateurs :
`ImportCredential=ncsyncd-%i-*` fait que systemd ne remet à `ncsyncd@alice` que les
identifiants dont le nom commence par `ncsyncd-alice-` ; le service d'un utilisateur ne
reçoit donc jamais les mots de passe d'un autre, bien qu'ils soient tous dans le même
magasin. L'id du compte désigne ensuite le compte dans la configuration de l'utilisateur.

#### Le piloter

```sh
ncsync status                   # comptes et dossiers (--json pour la réponse JSON du service)
ncsync pause [ALIAS|CHEMIN]     # tous les dossiers sans argument
ncsync resume [ALIAS|CHEMIN]
ncsync sync-now [ALIAS|CHEMIN]  # comme « Synchroniser maintenant » de l'icône de notification
ncsync status --instance alice  # un service système (en root ou en alice)
```

La socket de contrôle est `$XDG_RUNTIME_DIR/ncsyncd/control.sock` (utilisateur) ou
`/run/ncsyncd/<utilisateur>/control.sock` (système), droits 0600. Le service signale à
systemd `READY`, un `STATUS` d'une ligne et le chien de garde (`Type=notify`,
`WatchdogSec=120`) ; le chien de garde est nourri par la boucle d'événements elle-même, si
bien qu'une boucle bloquée fait redémarrer le service.

#### Reprendre un dossier au client officiel

Avec le client officiel **arrêté** (il réécrit sa configuration en quittant) :

```sh
ncsync takeover ~/Nextcloud   # ou l'alias du dossier dans le client officiel
ncsync handback 1             # le rend, en restaurant son entrée d'origine
```

`takeover` copie le compte (avec son mot de passe d'application, lu dans l'entrée du
trousseau du client officiel quand c'est possible, sinon par Login Flow v2 ou
`--app-password-file`) et l'entrée du dossier, en gardant le chemin local et le journal, et
retire le dossier de la configuration officielle (une copie du fichier officiel est gardée
sous `nextcloud.cfg.ncsyncd-bak`). Les deux commandes refusent de s'exécuter tant que le
client officiel tourne.

### Écarts avec l'amont

Différences de comportement voulues avec le client officiel v34.0.5 :

* **Débordement de la file inotify.** Quand la file inotify du noyau déborde
  (`IN_Q_OVERFLOW`), des événements sont perdus. Le surveillant de dossiers Linux de l'amont
  ignore cet événement : les changements faits entre-temps attendent le prochain parcours
  local complet (une heure par défaut). `ncsyncd` considère le débordement comme des
  changements perdus : la synchronisation suivante fait un parcours local complet, et elle
  est lancée tout de suite.
* **Création de compte sans mot de passe d'application.** `nextcloudcmd --userid ...
  --serverurl ...` sans `--apppassword` enregistre le compte sans identifiants, pour que
  l'interface du client de bureau demande la connexion plus tard. Une ligne de commande n'a
  pas de « plus tard », et un compte sans identifiants serait seulement déconnecté par
  `ncsyncd` ; `ncsync sync --userid ...` se connecte donc tout de suite par Login Flow v2
  (lien et QR code dans le terminal), et avec `--non-interactive` refuse la création
  (sortie 255) en demandant un mot de passe d'application.
* **`--trust` en création de compte.** L'amont lit `--trust` mais ne l'applique pas à la
  création du compte : un serveur à certificat auto-signé ne peut pas être configuré en
  ligne de commande. `ncsync` l'applique à chaque requête de la création.
* **`--httpproxy` en création de compte.** L'amont lit `--httpproxy` mais ne l'applique pas
  à la création du compte ; `ncsync` fait passer par lui chaque requête de la création
  (`status.php`, Login Flow v2 et son attente, `ocs/v1.php/cloud/user`, le PROPFIND).

### Limites

- **Linux uniquement.** Le service repose sur des briques de Linux : inotify pour surveiller
  les dossiers, systemd pour les services, le magasin d'identifiants et le chien de garde,
  le Secret Service (GNOME Keyring, KWallet) pour les mots de passe d'application.
  `ncsync sync` n'est pas proposé non plus sous Windows ou macOS : le client officiel y a
  des comportements propres (noms de fichiers en Unicode NFC sous macOS, attributs de
  fichiers, chemins longs et noms insensibles à la casse sous Windows), qui ne sont pas
  portés, et « le même comportement que le client officiel » n'y serait pas vrai.
- **Pas de chiffrement de bout en bout.** Les dossiers chiffrés sont ignorés, comme le fait
  le client officiel quand le chiffrement de bout en bout n'est pas configuré.
- **Pas de fichiers virtuels** (fichiers téléchargés seulement à la première ouverture). Les
  dossiers en mode fichiers virtuels sont refusés, par `ncsyncd` comme par
  `--isvfsenabled 1`.
- **Pas d'interface graphique, pas d'icône de notification, pas d'intégration au
  gestionnaire de fichiers.** `ncsync status` et le journal de systemd permettent de voir ce
  qu'il fait.
- **L'envoi groupé (bulk upload) est désactivé**, comme dans la v34.0.5 qui le désactive :
  il est porté (un seul `POST /remote.php/dav/bulk` pour beaucoup de petits fichiers) mais
  pas proposé.
- **Pas encore vérifié sur un vrai poste de travail** (tout ce qui suit est couvert par des
  tests et des conteneurs, pas par un usage quotidien) :
  - l'enregistrement et la lecture des mots de passe dans un vrai trousseau Secret Service
    (GNOME Keyring, KWallet) ;
  - la lecture du mot de passe du client officiel dans son entrée de trousseau
    (QtKeychain) pendant `ncsync takeover` ; si elle échoue, Login Flow v2 ou
    `--app-password-file` prend le relais ;
  - un fonctionnement prolongé sous un vrai systemd (service utilisateur avec linger,
    `ncsyncd@` avec `ImportCredential=` et `systemd-creds`).
- **Bêta.** La version est en `0.x` : la ligne de commande et les fichiers de
  `~/.config/ncsyncd` peuvent encore changer. Gardez une autre copie de ce qui compte, et
  signalez ce que vous trouvez (voir [Aide et retours](#aide-et-retours)).

### Compiler et tester

```sh
cargo build --release -p ncsync -p ncsyncd
cargo test --workspace
tools/itest/run.sh          # ncsync sync de bout en bout contre un conteneur Nextcloud jetable (Docker)
tools/itest/run-daemon.sh   # ncsyncd de bout en bout, avec redis et notify_push
tools/itest/run.sh --down
```

La pile d'intégration est un projet compose à part (`ncrs-itest`, port 18080 sur
l'interface locale) et ne touche jamais aux autres conteneurs. Les dossiers de travail sont
par défaut `~/.cache/ncrs-itest*` (premier argument des scripts).

Le banc côte à côte fait passer le `nextcloudcmd` officiel du tag de référence et `ncsync`
par les mêmes scénarios contre le serveur de test (chacun avec son utilisateur), et compare
après chaque étape les arborescences locales, celles du serveur et le contenu normalisé des
journaux ; `--roundtrip` fait se relayer les deux clients sur un même dossier et un même
journal (une reprise et une restitution à chaque étape) et vérifie que l'autre client n'a
alors rien à faire :

```sh
tools/bench/build-oracle.sh          # une fois : nextcloudcmd v34.0.5 dans l'image de CI amont (Docker)
docker compose -f tools/itest/compose.yml -p ncrs-itest up -d
tools/bench/bench.py                 # côte à côte
tools/bench/bench.py --roundtrip     # un seul dossier, clients en alternance
```

L'oracle est rangé dans `~/.cache/ncrs-oracle/nccmd-34.0.5` (`build-oracle.sh DIR`, ou
`NCRS_ORACLE` pour le banc), le travail du banc dans `~/.cache/ncrs-bench`
(`NCRS_BENCH_WORK`).

L'organisation des crates est décrite dans le tableau de la partie anglaise
([Building and testing](#building-and-testing)).

### Aide et retours

- Projet : <https://github.com/Le-Syl21/nextcloud-client-rs> (les tickets sont bienvenus)
- Discord : <https://discord.gg/T37DYHmt2j>

Signalez les problèmes ici, pas à Nextcloud : les développeurs du client officiel ne
prennent pas en charge ce portage.

### Licence

**GPL-2.0-or-later** (voir [LICENSE](LICENSE)), comme le client amont, et chaque crate la
déclare (`nc-testutils`, jamais publié, est en CC0-1.0 comme le `test/` amont). Chaque
fichier porté garde les lignes de copyright et l'en-tête SPDX du fichier amont dont il est
issu : LGPL-2.1-or-later pour la plupart des fichiers de `src/common` et `src/csync`
(distribuables sous GPL, d'où la GPL-2.0-or-later pour l'ensemble), GPL-2.0-or-later pour
ceux de `src/libsync`, `src/gui` et `src/cmd`, CC0-1.0 pour les tests (sauf les quelques
fichiers de test amont sous une autre licence, dont les portages la gardent). Le détail est
dans la partie anglaise ; le projet suit [REUSE](https://reuse.software/) : voir
`REUSE.toml` et `LICENSES/`.

### Remerciements

Ceci est un portage : la conception, les choix et les tests sont ceux du
[client de bureau Nextcloud](https://github.com/nextcloud/desktop), par Nextcloud GmbH et
les contributeurs de Nextcloud, lui-même issu du client ownCloud d'ownCloud GmbH et de ses
contributeurs, et de csync. Leurs lignes de copyright sont conservées dans chaque fichier
porté. Merci à eux ; les bogues d'ici sont ceux du portage.

« Nextcloud » est une marque de Nextcloud GmbH. nextcloud-client-rs est un projet
indépendant, ni affilié à Nextcloud GmbH ni approuvé par elle.
