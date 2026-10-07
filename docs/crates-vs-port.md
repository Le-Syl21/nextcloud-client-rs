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

## Planned (later phases), from the study

All of these are to be re-checked for the latest version when they are added
(`cargo add`, no old pins).

| Component (upstream) | Planned crate | Latest seen 2026-10-07 | Notes |
|---|---|---|---|
| HTTP client (`AbstractNetworkJob`, QNAM) | **reqwest** (rustls) | 0.13.5 | Implements `nc_dav::Transport`. Streaming bodies are needed for chunked uploads. |
| Async runtime, timers, cancellation | **tokio** (+ tokio-util `CancellationToken`) | 1.53.2 | Propagator scheduling (`scheduleNextJob`, composite jobs) stays a port; only the primitives come from tokio. |
| PROPFIND / XML (`LsColJob` parser, OCS) | **quick-xml** | 0.42.0 | The parser logic (which props, how errors are handled) is ported on top. Existing WebDAV client crates do not expose `oc:`/`nc:` props or the error semantics. |
| inotify watcher (`folderwatcher_linux.cpp`) | **inotify** | 0.11.5 | Exact upstream mask and `IN_Q_OVERFLOW` → full local discovery. `notify` 9.0.0 is still an RC and hides the raw mask. |
| notify_push websocket (`pushnotifications.cpp`) | **tokio-tungstenite** | 0.30.0 | — |
| `nextcloud.cfg` takeover / hand-back (QSettings ini) | **ini-preserve** (our crate) | 0.1.3 | Format-preserving, needed to write the official client's config back. `rust-ini` 0.21.3 loses formatting. |
| Credentials | **keyring** | 4.2.0 | Plus systemd `LoadCredential` for the root daemon. |
| systemd `Type=notify` and watchdog | **sd-notify** | 0.5.0 | — |
| Conflict file timestamps (`yyyy-MM-dd hhmmss`, local time), RFC 1123 dates | **jiff** | 0.2.38 | Preferred over chrono 0.4.45 for correct time zone handling. To be validated against upstream conflict-name tests. |
| NFC/NFD (macOS `getPHash`, server names) | **unicode-normalization** | 0.1.25 | Only where upstream normalizes. |
| URLs (`QUrl`) | **url** | 2.5.8 | `makeDbName` needs upstream's `QUrl::toString()` rendering exactly. That needs a check (or a small adapter) before it is relied on. |
| File locking / free disk space (`Utility::freeDiskSpace`, used by `openOrCreateReadWrite`) | **fs4** | 1.1.0 | The free-space check is a TODO in `journal/mod.rs`. |
