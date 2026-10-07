# Crates vs port

Rule: when a reputable, maintained crate gives the exact upstream behaviour,
use it, at its latest released version. Parity is proven with the ported
upstream tests. When a crate differs on behaviour the upstream tests cover,
or nothing exists, the upstream code is ported and the reason is given here.

Versions were checked on crates.io on 2026-10-07. The toolchain is Rust
stable 1.99 with edition 2024. `rust-version = "1.88"` is the minimum the code
needs (let-chains, `slice::as_chunks`).

## Phase 0 (in the tree)

| Component (upstream) | Choice | Version | Why |
|---|---|---|---|
| SQLite access (`ownsql.cpp`, `SqlDatabase`/`SqlQuery`) | **rusqlite** (`bundled`, `functions`) | 0.40.2 (libsqlite3-sys 0.38.2) | Replaces the wrapper outright. Upstream's binding and getter rules (QByteArray as TEXT, null QString as NULL, `sqlite3_column_*` conversions, `exec()` not stepping PRAGMA) are reproduced by small helpers in `journal/mod.rs`. The `bundled` feature ships a recent SQLite with JSON1, which `hasFileIds` needs. |
| Journal (`syncjournaldb.cpp`) | ported, on top of rusqlite | — | The on-disk format, queries, migrations and quirks are the interface with the official client. No crate offers them. |
| `c_jhash64` (`c_jhash.h`, Bob Jenkins lookup8) | ported (about 60 lines) | — | Checked `jenkins_hash` 0.2.0 (lookup2, 32-bit only) and `hashers` 1.0.1 (OAAT, lookup3, Spooky). Neither has lookup8 `hash()` 64-bit, which is what `phash` uses. The port is bit-exact against 418 vectors from upstream C and the 110 rows of upstream `test_journal.db`. |
| MD5 / SHA-1 / SHA-256 / SHA3-256 (`checksumcalculator.cpp`) | **md-5**, **sha1**, **sha2**, **sha3** (RustCrypto) | 0.11.0, 0.11.0, 0.11.0, 0.12.0 (digest 0.11.3) | Standard algorithms, so the output is identical by definition. Verified against `md5sum` and `sha1sum` in the ported tests. |
| Adler-32 | **adler2** | 2.0.1 | Same algorithm as zlib's `adler32()`. The upstream text encoding (`QByteArray::number(x, 16)`: lowercase, no leading zeros) is applied on top. |
| `md5("user@url:path")` for the journal name (`makeDbName`) | **md-5** | 0.11.0 | — |
| Checksum header helpers (`checksums.cpp`) | ported | — | Small string rules (preference order, case sensitivity) with upstream-specific quirks. No crate covers them. |
| Exclude engine (`csync_exclude.cpp`) | ported; matching uses **regex** | 1.13.1 | Checked `globset` 0.4.20, `ignore` 0.4.33 and `wax` 0.7.0. They implement glob/gitignore semantics (`!` negation, `**`, anchoring rules), which differ from upstream's. Upstream adds `]` "keep deleted" patterns, bname triggers, `#!version` directives, per-directory `.sync-exclude.lst`, separate traversal and full-path matching, `wildcardsMatchSlash` and Windows reserved names, then turns everything into one PCRE. The ported tests (22/22) compare that generated regex text. The PCRE dialect is mapped onto `regex` (documented in `exclude/mod.rs`). |
| Remote permissions (`remotepermissions.cpp`) | ported | — | A bitfield over a letter string with upstream quirks (UTF-16 truncation, null vs empty). No crate covers it. |
| Share-attributes JSON in remote permissions | **serde_json** | 1.0.151 | — |
| Conflict-name and path helpers (`utility.cpp`) | ported | — | Upstream-specific string rules. |
| UTF-16 ordering (`QString::operator<`) | std (`encode_utf16`) | — | No dependency needed. |
| Error types | **thiserror** | 2.0.21 | — |
| Logging facade | **log** | 0.4.34 | The backend (with targets mirroring Qt logging categories) is chosen with the binaries. |
| HTTP types for the in-process transport (`nc-dav`) | **http**, **bytes** | 1.5.0, 1.12.1 | The same types reqwest uses, so the real transport is a thin adapter. |
| URL percent-encoding (fake server hrefs) | **percent-encoding** | 2.3.2 | — |
| HTTP dates in the fake server | **httpdate** | 1.0.3 | IMF-fixdate, identical to the `ddd, dd MMM yyyy HH:mm:ss 'GMT'` format upstream uses. Replaced a hand-written formatter. |
| `Utility::rand()` in the fake server (etags, file ids, request ids) | **fastrand** | 2.5.0 | Replaced a hand-written xorshift. |
| Temporary directories in tests | **tempfile** | 3.27.0 | — |

## Phase 1 (in the tree)

All versions are the latest on crates.io on 2026-10-07 (`cargo add`).

| Component (upstream) | Choice | Version | Why |
|---|---|---|---|
| HTTP client (`AbstractNetworkJob`, QNAM, `AccessManager`) | **reqwest** (`rustls`, `http2`, `stream`, `cookies`, `gzip`, `deflate`, `system-proxy`; no default features) | 0.13.5 | Implements `nc_dav::Transport` in `http_client.rs`, with streaming request and response bodies (downloads are written while received, chunk uploads stream from the file). Upstream's error model (`QNetworkReply::NetworkError`, `statusCodeFromHttp`, `networkReplyErrorString`, `OC-ErrorString`, timeouts) is ported in `reply.rs` on top: reqwest only supplies bytes and status. Cookies are kept like QNAM's cookie jar; `--trust` maps to accepting invalid certificates. |
| Event loop, timers, cancellation | **tokio** (current-thread runtime), **tokio-util** (`CancellationToken`), **futures-util** (`FuturesUnordered`) | 1.53.2, 0.7.19, 0.3.34 | Only the primitives. Upstream's single-threaded Qt event loop ordering is ported: job arenas, posted-event queues, completions handled one at a time, `scheduleNextJob` as a flag drained after the ready completions, item jobs polled once at start so they register in `_activeJobList` synchronously like `start()`. Abort is a hard/soft token pair with upstream's 5 s timer. |
| PROPFIND / XML (`LsColXMLParser`, `PropfindJob`, error bodies) | **quick-xml** (`NsReader`) | 0.42.0 | Tokenizer only; the parser (which props, href normalisation, the "expected path" check, `<s:message>` / `<s:exception>` extraction) is ported in `xml.rs`. |
| Request ids (`X-Request-ID`), transfer ids | **uuid** (v4) | 1.27.0 | Upstream uses `QUuid::createUuid()`. |
| Basic auth header, `OC-Checksum` / share attribute decoding | **base64** | 0.23.1 | — |
| Conflict and case-clash file names (local time `yyyy-MM-dd hhmmss`), RFC 2822 `Last-Modified`/`Date` parsing | **jiff** | 0.2.38 | Preferred over chrono for time zones. Checked against the conflict-name tests. |
| `utimensat`, `lstat`-based local discovery, `statvfs` (`Utility::freeDiskSpace`), umask | **rustix** (`fs`, `process`) | 1.1.5 | Safe wrappers (the workspace forbids `unsafe`). Replaces the planned fs4 for free disk space. |
| Move to trash (`FileSystem::moveToTrash`, `SyncOptions::_moveFilesToTrash`) | **trash** (no default features) | 5.2.9 | freedesktop.org trash spec, the same as upstream's Linux implementation. Used only where upstream calls it (the `MoveToClientTrashBin` list). |
| CLI options (`src/cmd/cmd.cpp`, hand-written `parseOptions`) | **clap** (`derive`, `env`) | 4.6.7 | The nextcloudcmd option names and semantics are kept (`-s`, `--httpproxy`, `--trust`, `--exclude`, `--unsyncedfolders`, `-u`, `-p`, `-n`, `--non-interactive`, `--max-sync-retries`, `-h`, `--logdebug`, `--path`, ...). Upstream's own `-h` (sync hidden files) is kept, so clap's help is `--help` only. |
| Log backend (Qt message pattern) | **env_logger** | 0.11.11 | Output format mirrors upstream's `[ level category ]:\tmessage` lines; targets are the upstream logging categories (`nextcloud.sync.propagator`, ...). |
| Password prompt | **rpassword** | 7.5.4 | Upstream reads stdin with echo off (`EchoDisabler`). |
| `~/.netrc` (`netrcparser.cpp`) | ported | — | Upstream's parser has its own quirks (no quoting support, `default` entry, whitespace splitting) covered by its test, ported 1:1. The `netrc` crates differ on those. |
| Server URLs (`QUrl`) | ported subset (`nc_dav::account::ServerUrl`) | — | Only what the client needs: scheme/host/port/path, credentials in the URL, lower-cased host, percent-encoding of DAV paths like `QUrl::toPercentEncoding(path, "/")`, and `toString()` without credentials for `makeDbName`. The `url` crate normalises differently (e.g. IDNA, path dot segments), which would change journal names. |
| Engine (discovery, reconcile, propagator, jobs) | ported | — | The behaviour under test. |
| Progress (`ProgressInfo`, `progressdispatcher.cpp`) | ported (totals and per-item progress; no estimates) | — | Drives `transmissionProgress`, which several upstream tests use as their hook. |

Build note: `[profile.dev.package."*"] opt-level = 3` optimises the
dependencies (checksums, SQLite, TLS) in debug builds; the big-file chunking
tests went from minutes to seconds. The first debug build takes longer.

## Phase 2 (in the tree)

All versions are the latest on crates.io on 2026-10-07 (`cargo add`).

| Component (upstream) | Choice | Version | Why |
|---|---|---|---|
| `nextcloud.cfg` (`QSettings`, `IniFormat`) | **ini-preserve** (our crate) + ported `QSettings` layer (`nc-daemon/src/settings.rs`) | 0.1.3 | ini-preserve keeps every untouched line (comments, order, `@Variant` blobs such as `serverColor`), which a takeover of the official client's file needs; `rust-ini` 0.21.3 rewrites the file. On top of it, written from the documented `IniFormat` behaviour: `\`-separated subkeys, `[General]`/`[%General]`, `%XX`/`%UXXXX` key escaping, value escaping and quoting, string lists, `@ByteArray`/`@Variant`/`@Invalid`/`@@`, `childGroups`/`childKeys`/`remove`, a `QLockFile`-style `<file>.lock` and atomic writes. Two ini-preserve 0.1.3 traits show in new keys only: they are written `key = value` (QSettings writes `key=value`, and reads both) and appended after the section's trailing blank line. |
| `ConfigFile`, `AccountManager` load/save, `FolderDefinition`, `FolderMan` setup and path checks | ported | — | The keys, groups, defaults and migrations are the interface with the official client. |
| Credentials, user daemon (QtKeychain) | **keyring-core** + **zbus-secret-service-keyring-store** (`crypto-rust`) | 1.0.0, 1.0.1 | `keyring` 4.2.0 is now a thin wrapper: its `v1` feature is keyring-core plus this same store on Linux behind a global default store and `Entry::new(service, user)` only, and its README tells applications to depend on keyring-core and the stores directly. We need an explicit store instance (tests use an in-memory store, never the session keyring) and the store's attribute search, to read the official client's QtKeychain item (`server`=`Nextcloud`, `user`=keychain key). Pure Rust D-Bus (zbus), no libsecret. |
| Credentials, system instance | systemd credentials (`$CREDENTIALS_DIRECTORY/ncsyncd-<accountId>`) and password files | — | `LoadCredential=`/`LoadCredentialEncrypted=` need no library: the secret is a file. |
| QtKeychain binary items (`type`=`base64`) | **base64** | 0.23.1 | — |
| Login Flow v2 (`flow2auth.cpp`) | ported, on the nc-dav jobs | — | Same TLS/proxy options as the sync. The login link is printed instead of opened. |
| QR code of the login link | **qrcode** (no default features: no `image`) | 0.14.1 | Unicode `Dense1x2` rendering for terminals. |
| Official client detection | std (`/proc`) | — | `comm`/`exe`/`argv[0]` of the configuration owner's processes. |
| Paused clock in the Login Flow tests | **tokio** `test-util` (dev only) | 1.53.2 | — |

## Planned (later phases), from the study

All of these are to be re-checked for the latest version when they are added
(`cargo add`, no old pins).

| Component (upstream) | Planned crate | Latest seen 2026-10-07 | Notes |
|---|---|---|---|
| inotify watcher (`folderwatcher_linux.cpp`) | **inotify** | 0.11.5 | Exact upstream mask and `IN_Q_OVERFLOW` → full local discovery. `notify` 9.0.0 is still an RC and hides the raw mask. |
| notify_push websocket (`pushnotifications.cpp`) | **tokio-tungstenite** | 0.30.0 | — |
| systemd `Type=notify` and watchdog | **sd-notify** | 0.5.0 | — |
| NFC/NFD (macOS `getPHash`, server names) | **unicode-normalization** | 0.1.25 | Only where upstream normalizes. |
