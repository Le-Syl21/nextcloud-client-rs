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
| HTTP client (`AbstractNetworkJob`, QNAM, `AccessManager`) | **reqwest** (`rustls`, `http2`, `stream`, `cookies`, `system-proxy`; no default features; `gzip`/`deflate` dropped in Phase 2, see below) | 0.13.5 | Implements `nc_dav::Transport` in `http_client.rs`, with streaming request and response bodies (downloads are written while received, chunk uploads stream from the file). Upstream's error model (`QNetworkReply::NetworkError`, `statusCodeFromHttp`, `networkReplyErrorString`, `OC-ErrorString`, timeouts) is ported in `reply.rs` on top: reqwest only supplies bytes and status. Cookies are kept like QNAM's cookie jar; `--trust` maps to accepting invalid certificates. |
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
| inotify watcher (`folderwatcher_linux.cpp`) | **inotify** (no default features), read through tokio `AsyncFd` (tokio `net`) | 0.11.5 | `inotify_init1` and `inotify_add_watch` / `inotify_rm_watch` with upstream's exact mask (`IN_CLOSE_WRITE \| IN_ATTRIB \| IN_MOVE \| IN_CREATE \| IN_DELETE \| IN_DELETE_SELF \| IN_MOVE_SELF \| IN_UNMOUNT \| IN_ONLYDIR`). The `read(2)` loop (2048-byte buffer doubled on `EINVAL`) and the `inotify_event` decoding are ported (over `rustix::io::read`) rather than the crate's `EventStream`, so `testinotifywatcher` can feed a pipe like upstream. Divergence: `IN_Q_OVERFLOW` sends `LostChanges` (upstream's Linux watcher ignores it). `notify` 9.0.0 is still an RC and hides the raw mask. |
| `FolderWatcher` (`folderwatcher.cpp`): change aggregation, lock-file debouncing, notification and permission self-tests | ported | — | Signals become events on a tokio mpsc channel; `QTimer`s are `spawn_local` tasks (single-threaded like Qt). |
| Lock file detection (`FileSystem::filePathLockFilePatternMatch`, `lockFileTargetFilePath`, `src/libsync/filesystem.cpp`) | ported, `Adobe` name patterns with **regex** | 1.13.1 | Upstream-specific rules (Office/LibreOffice prefixes, AutoCAD pairs, Adobe sibling lookup, Affinity suffix). Lives in `nc-daemon` until the sync engine needs it. |
| notify_push websocket (`pushnotifications.cpp`, `QWebSocket`) | **tokio-tungstenite** (`handshake` only, no default features) | 0.30.0 (tungstenite 0.30.0) | Websocket framing, the HTTP upgrade, ping/pong and the closing handshake. The client logic (authentication, messages, the ping, ping-timeout and reconnect timers, the error/SSL-error split, the account's reconnect timer and `isPushNotificationsWebSocketUrlAllowed`) is ported in `push_notifications.rs` and `account.rs`. The TCP connection, the TLS layer and the proxy tunnel are set up by the port (below) so that they follow the HTTP client's options. |
| TLS of `wss://` | **tokio-rustls** (defaults: aws-lc-rs, TLS 1.2) + **rustls-platform-verifier** | 0.26.6 (rustls 0.23), 0.7.1 | Built like reqwest's rustls client in `HttpTransport`: the process default crypto provider or aws-lc-rs, TLS 1.2 and 1.3, the platform verifier, and an accept-all verifier with `--trust` (`trust_invalid_certificates`). A rejected certificate is upstream's `sslErrors` (`authenticationFailed`), any other TLS failure its `errorOccurred` (`connectionLost`). |
| Proxy for the websocket | ported (HTTP `CONNECT` tunnel, Basic proxy credentials) | — | tokio-tungstenite has no proxy support. The explicit proxy option (`--httpproxy`) is honoured; the system/environment proxies that reqwest's `system-proxy` feature reads (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`) are not, nor are SOCKS proxies (the websocket then fails with `connectionLost`). |
| Self-signed certificate of the `wss` fake server (tests only) | **rcgen** (`crypto`, `aws_lc_rs`; no default features) | 0.14.10 | Dev-dependency of `nc-dav`. |
| `nextcloud.cfg` (`QSettings`, `IniFormat`) | **ini-preserve** (our crate) + ported `QSettings` layer (`nc-daemon/src/settings.rs`) | 0.1.3 | ini-preserve keeps every untouched line (comments, order, `@Variant` blobs such as `serverColor`), which a takeover of the official client's file needs; `rust-ini` 0.21.3 rewrites the file. On top of it, written from the documented `IniFormat` behaviour: `\`-separated subkeys, `[General]`/`[%General]`, `%XX`/`%UXXXX` key escaping, value escaping and quoting, string lists, `@ByteArray`/`@Variant`/`@Invalid`/`@@`, `childGroups`/`childKeys`/`remove`, a `QLockFile`-style `<file>.lock` and atomic writes. Two ini-preserve 0.1.3 traits show in new keys only: they are written `key = value` (QSettings writes `key=value`, and reads both) and appended after the section's trailing blank line. |
| `ConfigFile`, `AccountManager` load/save, `FolderDefinition`, `FolderMan` setup and path checks | ported | — | The keys, groups, defaults and migrations are the interface with the official client. |
| Credentials, user daemon (QtKeychain) | **keyring-core** + **zbus-secret-service-keyring-store** (`crypto-rust`) | 1.0.0, 1.0.1 | `keyring` 4.2.0 is now a thin wrapper: its `v1` feature is keyring-core plus this same store on Linux behind a global default store and `Entry::new(service, user)` only, and its README tells applications to depend on keyring-core and the stores directly. We need an explicit store instance (tests use an in-memory store, never the session keyring) and the store's attribute search, to read the official client's QtKeychain item (`server`=`Nextcloud`, `user`=keychain key). Pure Rust D-Bus (zbus), no libsecret. |
| Credentials, system instance | systemd credentials (`$CREDENTIALS_DIRECTORY/ncsyncd-<accountId>`) and password files | — | `LoadCredential=`/`LoadCredentialEncrypted=` need no library: the secret is a file. |
| QtKeychain binary items (`type`=`base64`) | **base64** | 0.23.1 | — |
| Login Flow v2 (`flow2auth.cpp`) | ported, on the nc-dav jobs | — | Same TLS/proxy options as the sync. The login link is printed instead of opened. |
| QR code of the login link | **qrcode** (no default features: no `image`) | 0.14.1 | Unicode `Dense1x2` rendering for terminals. |
| Official client detection | std (`/proc`) | — | `comm`/`exe`/`argv[0]` of the configuration owner's processes. |
| Paused clock in the Login Flow tests | **tokio** `test-util` (dev only) | 1.53.2 | — |
| Transparent gzip/deflate decoding of replies with Qt's decompression safety check (`QNetworkRequest::setDecompressedSafetyCheckThreshold`, `QDecompressHelper`; used by `GETFileJob`) | **zlib-rs** (`std`; no default features), check ported in `nc-dav/src/decompress.rs` | 0.6.8 | reqwest's `gzip`/`deflate` decoding has no ratio check, so it is disabled and the transport decodes itself. zlib-rs is a memory-safe port of zlib with the same `inflateInit2(MAX_WBITS + 32)` zlib/gzip auto detection Qt uses (flate2 does not expose it). The Qt rules are reproduced: `Accept-Encoding: gzip, deflate` unless the caller set one, raw deflate retry, concatenated streams, eager decoding with the compressed/decompressed byte counts, ratio > 40 above the threshold (default 10 MiB, `-1` disables) → `UnknownContentError`, `Content-Length` removed for HTTP/1 only (QTBUG-73364). |
| Bandwidth limits (`bandwidthmanager.cpp`, `UploadDevice`, `GETFileJob` quota) | ported (`nc-sync/src/propagator/bandwidth.rs`) on tokio timers and `Notify` | — | Upstream-specific quota scheme (limit split per registered transfer every second, 10 s switching timer). No crate does that; a token-bucket crate (`governor`, ...) would pace differently. |
| Local HTTP servers of the `HAVE_QHTTPSERVER` tests (`QHttpServer`) | hand-written std `TcpListener` server in `nc-testutils/tests/common/http_server.rs` | — | About 150 lines for routes, recorded requests and fixed responses; a server framework would add a large dev-dependency for nothing the tests need. |
| Folder, FolderMan, AccountState, ConnectionValidator, SyncResult (`folder.cpp`, `folderman.cpp`, `accountstate.cpp`, `connectionvalidator.cpp`, `syncresult.cpp`) | ported | — | The scheduling (queue, pause after the last sync, etag polling one request at a time, time scheduler, failure back-off, follow-up syncs), the connection state machine and its retry back-off are the behaviour under test. |
| Qt event loop, `QTimer` | **tokio** current-thread runtime + `LocalSet`; timers are local tasks posting generation-stamped events (`nc-daemon/src/timer.rs`) | 1.53.2 | One thread like the GUI thread: events are handled one at a time, in posting order; a stopped timer's late timeout is dropped like `QTimer::stop()` guarantees. |
| Scheduled sync run timers for expiring server locks, touched files (`syncengine.cpp`) | ported (`nc-sync/src/scheduled_sync.rs`, `touched_files.rs`) as deadlines | — | The daemon arms one tokio timer on the earliest deadline. |
| systemd `Type=notify`, watchdog, status | **sd-notify** | 0.5.0 | `READY=1`, `STATUS=`, `STOPPING=1`, and `WATCHDOG=1` sent from the event loop itself (half of `WatchdogSec`), so a stuck loop gets the service restarted. |
| Control socket (no upstream counterpart: the GUI) | tokio `UnixListener`, one JSON line per request, **serde** (`derive`) + **serde_json** | 1.0.229, 1.0.151 | Mode 0600 in a 0700 runtime directory. |

## Planned (later phases), from the study

All of these are to be re-checked for the latest version when they are added
(`cargo add`, no old pins).

| Component (upstream) | Planned crate | Latest seen 2026-10-07 | Notes |
|---|---|---|---|
| NFC/NFD (macOS `getPHash`, server names) | **unicode-normalization** | 0.1.25 | Only where upstream normalizes. |
