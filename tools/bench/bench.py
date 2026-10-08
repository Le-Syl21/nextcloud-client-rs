#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
# SPDX-License-Identifier: GPL-2.0-or-later
"""Side-by-side bench: `ncsync sync` against the official `nextcloudcmd`.

Both clients run the same scripted scenario against the throw-away test
Nextcloud of tools/itest (compose project `ncrs-itest`, 127.0.0.1:18080),
each with its own user, so that both start from the same server state (the
skeleton files). After every step the local tree, the server tree and a
normalized dump of the journal (`.sync_*.db`) are compared; anything that
differs is reported.

A second mode, `--roundtrip`, alternates the two clients on ONE folder and
journal (step 1 by nextcloudcmd, step 2 by ncsync, ...), which is what a
takeover / hand-back does, and checks that the result matches the
single-client runs and that an idle sync by the other client propagates
nothing.

nextcloudcmd is the one built from the pinned tag (v34.0.5) inside the
upstream CI image; see tools/bench/build-oracle.sh. It runs in a throw-away
container with the host network. Usage:

    docker compose -f tools/itest/compose.yml -p ncrs-itest up -d
    tools/bench/bench.py [--roundtrip]

Environment: NCRS_NCSYNC (default target/debug/ncsync), NCRS_ORACLE
(directory with build/bin/nextcloudcmd, default
~/.cache/claude-work/nccmd-34.0.5), NCRS_ORACLE_IMAGE.
"""

import argparse
import base64
import calendar
import hashlib
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import time
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
import functools

print = functools.partial(print, flush=True)  # noqa: A001
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
URL = "http://127.0.0.1:18080"
NC = "ncrs-itest-nc"
PASSWORD = "ncrs-bench-pw-0123"
NCSYNC = Path(os.environ.get("NCRS_NCSYNC", ROOT / "target/debug/ncsync"))
ORACLE = Path(
    os.environ.get("NCRS_ORACLE", Path.home() / ".cache/claude-work/nccmd-34.0.5")
)
ORACLE_IMAGE = os.environ.get(
    "NCRS_ORACLE_IMAGE",
    "ghcr.io/nextcloud/continuous-integration-client-qt6:client-sid-6.10.2-4",
)
WORK = Path(os.environ.get("NCRS_BENCH_WORK", Path.home() / ".cache/ncrs-bench"))
# A fixed base time for every mtime the scenario sets (2026-01-02 03:04:05 UTC).
T0 = 1767323045

# ---------------------------------------------------------------- server ---


def occ(*args, env=None):
    cmd = ["docker", "exec", "-u", "www-data"]
    for k, v in (env or {}).items():
        cmd += ["-e", f"{k}={v}"]
    cmd += [NC, "php", "occ", *args]
    return subprocess.run(cmd, check=True, capture_output=True, text=True).stdout


class Dav:
    def __init__(self, user):
        self.user = user
        self.base = f"{URL}/remote.php/dav/files/{user}"
        tok = base64.b64encode(f"{user}:{PASSWORD}".encode()).decode()
        self.auth = {"Authorization": f"Basic {tok}", "OCS-APIRequest": "true"}

    def url(self, path):
        return self.base + "/" + urllib.parse.quote(path.strip("/"))

    def req(self, method, path, data=None, headers=None, ok=(200, 201, 204, 207)):
        h = dict(self.auth)
        h.update(headers or {})
        r = urllib.request.Request(self.url(path), data=data, method=method, headers=h)
        try:
            with urllib.request.urlopen(r) as resp:
                body = resp.read()
                status = resp.status
        except urllib.error.HTTPError as e:
            body, status = e.read(), e.code
        if status not in ok:
            raise RuntimeError(f"{method} {path}: HTTP {status} {body[:300]!r}")
        return status, body

    def put(self, path, content, mtime=None):
        h = {}
        if mtime is not None:
            h["X-OC-Mtime"] = str(mtime)
        self.req("PUT", path, content if isinstance(content, bytes) else content.encode(), h)

    def mkcol(self, path):
        self.req("MKCOL", path)

    def move(self, src, dst):
        self.req("MOVE", src, headers={"Destination": self.url(dst), "Overwrite": "T"})

    def delete(self, path):
        self.req("DELETE", path)

    def get(self, path):
        return self.req("GET", path)[1]

    PROPS = """<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
<d:prop><d:resourcetype/><d:getlastmodified/><d:getcontentlength/><d:getetag/>
<oc:fileid/><oc:permissions/><oc:checksums/><oc:size/></d:prop></d:propfind>"""

    def tree(self):
        """{relative path: {type, size, mtime, etag, fileid, perms, sha1}}"""
        out = {}
        todo = [""]
        while todo:
            d = todo.pop()
            _, body = self.req(
                "PROPFIND", d, self.PROPS.encode(), {"Depth": "1", "Content-Type": "application/xml"}
            )
            root = ET.fromstring(body)
            ns = {"d": "DAV:", "oc": "http://owncloud.org/ns"}
            prefix = urllib.parse.urlparse(self.base).path + "/"
            for r in root.findall("d:response", ns):
                href = urllib.parse.unquote(r.find("d:href", ns).text)
                rel = href[len(prefix):].strip("/") if href.startswith(prefix) else href
                p = r.find("d:propstat/d:prop", ns)
                is_dir = p.find("d:resourcetype/d:collection", ns) is not None
                etag = (p.findtext("d:getetag", "", ns) or "").strip('"')
                lm = p.findtext("d:getlastmodified", "", ns)
                mtime = calendar.timegm(time.strptime(lm, "%a, %d %b %Y %H:%M:%S GMT")) if lm else None
                if rel == d.strip("/"):
                    if rel == "":
                        out[""] = {"type": "dir", "etag": etag, "fileid": p.findtext("oc:fileid", "", ns)}
                    continue
                e = {
                    "type": "dir" if is_dir else "file",
                    "mtime": mtime,
                    "etag": etag,
                    "fileid": p.findtext("oc:fileid", "", ns),
                    "perms": p.findtext("oc:permissions", "", ns),
                }
                if is_dir:
                    todo.append(rel)
                else:
                    e["size"] = int(p.findtext("d:getcontentlength", "0", ns) or 0)
                    e["sha1"] = hashlib.sha1(self.get(rel)).hexdigest()
                out[rel] = e
        return out


def reset_user(user):
    try:
        occ("user:delete", user)
    except subprocess.CalledProcessError:
        pass
    occ("user:add", "--password-from-env", user, env={"OC_PASS": PASSWORD})
    # The skeleton files get the creation time as mtime, which differs per
    # user: replace them with a seed of fixed mtimes, same names.
    dav = Dav(user)
    for rel, e in dav.tree().items():
        if rel and "/" not in rel:
            dav.delete(rel)
    for i, (rel, content) in enumerate(SEED):
        parent = rel.rsplit("/", 1)[0] if "/" in rel else ""
        if parent:
            try:
                dav.mkcol(parent)
            except RuntimeError:
                pass
        dav.put(rel, content, T0 - 1000 + i)


SEED = [
    ("Readme.md", "# Welcome\n"),
    ("Nextcloud.png", b"\x89PNG fake" * 500),
    ("Reasons to use Nextcloud.pdf", b"%PDF-1.4 fake" * 3000),
    ("Templates credits.md", "credits\n"),
    ("Documents/Welcome.md", "welcome\n"),
    ("Documents/Example.odt", b"PK odt" * 100),
    ("Photos/Birdie.jpg", b"\xff\xd8 jpg" * 2000),
    ("Photos/Library.jpg", b"\xff\xd8 lib" * 2000),
]


# ----------------------------------------------------------------- local ---


def write(base, rel, content, mtime_offset=0):
    p = base / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(content if isinstance(content, bytes) else content.encode())
    os.utime(p, (T0 + mtime_offset, T0 + mtime_offset))


CONFLICT_RE = re.compile(r" \(conflicted copy \d{4}-\d{2}-\d{2} \d{6}\)")
CASECLASH_RE = re.compile(r" \(case clash from \d{4}-\d{2}-\d{2} \d{6}\)")


def norm_name(rel):
    rel = CONFLICT_RE.sub(" (conflicted copy <date>)", rel)
    return CASECLASH_RE.sub(" (case clash from <date>)", rel)


def local_tree(base):
    out = {}
    for dirpath, dirnames, filenames in os.walk(base):
        dirnames.sort()
        for n in sorted(dirnames + filenames):
            full = Path(dirpath) / n
            rel = str(full.relative_to(base))
            if rel.startswith(".sync_") or n.startswith(".sync_") or rel.startswith(".nextcloudsync.log"):
                continue
            st = full.lstat()
            if full.is_dir():
                e = {"type": "dir", "mode": oct(st.st_mode & 0o777)}
            else:
                e = {
                    "type": "file",
                    "size": st.st_size,
                    "mtime": int(st.st_mtime),
                    "sha1": hashlib.sha1(full.read_bytes()).hexdigest(),
                    "mode": oct(st.st_mode & 0o777),
                }
            e["inode"] = st.st_ino
            out[rel] = e
    return out


def journal_path(base):
    dbs = sorted(base.glob(".sync_*.db"))
    return dbs[0] if dbs else None


def journal_dump(base, server, local):
    """Normalized journal: the values that are random per server (etags,
    file ids) or per disk (inodes) are replaced by whether they match the
    server or the disk."""
    path = journal_path(base)
    if path is None:
        return {"missing": True}
    con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    out = {"name_ok": bool(re.fullmatch(r"\.sync_[0-9a-f]{12}\.db", path.name))}
    out["schema"] = sorted(
        (r["type"], r["name"], re.sub(r"\s+", " ", r["sql"] or ""))
        for r in con.execute("SELECT type, name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'")
    )
    meta = {}
    for r in con.execute("SELECT * FROM metadata"):
        d = dict(r)
        p = d["path"]
        srv = server.get(p, {})
        loc = local.get(p, {})
        etag = d["md5"]
        d["md5"] = "=server" if etag and etag == srv.get("etag") else ("other" if etag else etag)
        fid = d["fileid"]
        if fid:
            num = fid.decode() if isinstance(fid, bytes) else str(fid)
            num = num[:8].lstrip("0") or "0"
            d["fileid"] = "=server" if num == str(srv.get("fileid", "")).lstrip("0") else "other"
        if d.get("type") == 2:  # directories: mtimes are the creation time
            d["modtime"] = "=server" if d["modtime"] == srv.get("mtime") else "other"
        d["inode"] = "=disk" if d["inode"] and d["inode"] == loc.get("inode") else ("other" if d["inode"] else d["inode"])
        for k in ("phash", "uid", "gid", "lastShareStateFetchedTimestmap"):
            d.pop(k, None)
        meta[norm_name(p)] = d
    out["metadata"] = meta
    for table, drop in (
        ("blacklist", ("lastTryTime", "requestId", "ignoreDuration")),
        ("conflicts", ("baseFileId", "baseEtag")),
        ("caseconflicts", ()),
        ("selectivesync", ()),
        ("downloadinfo", ("tmpfile", "etag")),
        ("uploadinfo", ("transferid", "modtime")),
        ("datafingerprint", ()),
        ("checksumtype", ()),
        ("flags", ()),
        ("key_value_store", ()),
        ("version", ()),
        ("async_poll", ()),
        ("e2EeLockedFolders", ()),
    ):
        try:
            rows = [dict(r) for r in con.execute(f"SELECT * FROM {table}")]
        except sqlite3.OperationalError:
            out[table] = "missing"
            continue
        for d in rows:
            for k in drop:
                d.pop(k, None)
            if table == "key_value_store" and d.get("key") == "last_sync":
                d["value"] = "<time>"
            # The local mtime of a directory the sync created is the time
            # it ran (the scenario's own mtimes are near T0).
            if isinstance(d.get("baseModtime"), int) and d["baseModtime"] > T0 + 10**6:
                d["baseModtime"] = "<sync time>"
            for k in ("path", "baseFile", "conflictPath", "basePath"):
                if isinstance(d.get(k), str):
                    d[k] = norm_name(d[k])
        out[table] = sorted(json.dumps(d, sort_keys=True, default=str) for d in rows)
    con.close()
    return out


def normalize_tree(tree, drop):
    out = {}
    for p, e in tree.items():
        e = {k: v for k, v in e.items() if k not in drop}
        if e.get("type") == "dir":
            e.pop("mtime", None)
        out[norm_name(p)] = e
    return out


# --------------------------------------------------------------- clients ---


def run_ncsync(local, user, extra_env=None):
    env = dict(os.environ, **(extra_env or {}))
    cmd = [str(NCSYNC), "sync", "-s", "--non-interactive", "-u", user, "-p", PASSWORD, str(local), URL]
    # The oracle's container runs with umask 022; same here, so that the
    # modes of created files compare.
    r = subprocess.run(cmd, env=env, capture_output=True, text=True, umask=0o022)
    return r.returncode, r.stdout + r.stderr


def run_nextcloudcmd(local, user, extra_env=None):
    cmd = [
        "docker", "run", "--rm", "--network", "host", "--user", f"{os.getuid()}:{os.getgid()}",
        "-e", "HOME=/tmp", "-v", f"{local}:{local}", "-v", f"{ORACLE}:/work:ro",
    ]
    for k, v in (extra_env or {}).items():
        cmd += ["-e", f"{k}={v}"]
    cmd += [
        ORACLE_IMAGE, "/work/build/bin/nextcloudcmd", "-s", "--non-interactive",
        "-u", user, "-p", PASSWORD, str(local), URL,
    ]
    r = subprocess.run(cmd, capture_output=True, text=True)
    return r.returncode, r.stdout + r.stderr


CLIENTS = {"oracle": run_nextcloudcmd, "ncsync": run_ncsync}


def propagated_items(log):
    """Item lines both clients print at the end of a sync are not
    comparable; the count of completed transfers is a rough signal only."""
    return len(re.findall(r"(?i)completed|Starting ", log))


# -------------------------------------------------------------- scenario ---
# Each step: (name, local actions, server actions, env). Actions take
# (local base Path, Dav).


def s_initial(local, dav):
    write(local, "hello.txt", "hello\n", 10)
    write(local, "sub/random.bin", hashlib.sha256(b"r").digest() * 9000, 11)
    write(local, "sub/deep/a.txt", "a\n", 12)
    write(local, "empty.txt", "", 13)
    (local / "emptydir").mkdir(exist_ok=True)
    write(local, ".hidden", "h\n", 14)
    write(local, "excluded.part", "x\n", 15)
    write(local, "~$lock.docx", "x\n", 16)


def s_local_changes(local, dav):
    write(local, "hello.txt", "changed\n", 20)
    os.remove(local / "Readme.md")
    os.rename(local / "Nextcloud.png", local / "sub/moved.png")
    os.rename(local / "sub/deep", local / "sub/deeper")
    write(local, "sub/deeper/b.txt", "b\n", 21)


def s_remote_changes(local, dav):
    dav.put("fromserver.txt", "from-server\n", T0 + 30)
    dav.mkcol("srvdir")
    dav.move("Reasons to use Nextcloud.pdf", "srvdir/renamed.pdf")
    dav.delete("Templates credits.md")
    dav.move("sub/deeper", "deepest")
    dav.put("hello.txt", "server-edit\n", T0 + 31)


def s_chunked(local, dav):
    write(local, "big.bin", hashlib.sha256(b"big").digest() * 160000, 40)


def s_conflicts(local, dav):
    write(local, "hello.txt", "local-version\n", 50)
    dav.put("hello.txt", "server-version-longer\n", T0 + 51)
    # Same content on both sides: no conflict copy.
    write(local, "fromserver.txt", "same\n", 52)
    dav.put("fromserver.txt", "same\n", T0 + 53)
    # Local delete vs remote edit: the remote edit wins.
    os.remove(local / "empty.txt")
    dav.put("empty.txt", "now not empty\n", T0 + 54)
    # Remote delete vs local edit: the local edit is uploaded again.
    write(local, "deepest/a.txt", "local edit\n", 55)
    dav.delete("deepest/a.txt")
    # New on both sides, different content.
    write(local, "both.txt", "local both\n", 56)
    dav.put("both.txt", "server both\n", T0 + 57)


def s_type_conflict(local, dav):
    (local / "typeclash").mkdir()
    write(local, "typeclash/inner.txt", "inner\n", 60)
    os.utime(local / "typeclash", (T0 + 59, T0 + 59))
    dav.put("typeclash", "a file on the server\n", T0 + 61)


def s_moves_both_sides(local, dav):
    os.rename(local / "srvdir/renamed.pdf", local / "srvdir/local-name.pdf")
    dav.move("srvdir", "srvdir-renamed")
    os.rename(local / "sub/random.bin", local / "random-moved.bin")
    dav.move("sub/moved.png", "moved-on-server.png")


def s_case(local, dav):
    dav.put("Case.txt", "upper\n", T0 + 70)
    dav.put("case.txt", "lower\n", T0 + 71)
    os.rename(local / "hello.txt", local / "Hello.txt")


def s_nothing(local, dav):
    pass


def s_mtime_only(local, dav):
    os.utime(local / "Hello.txt", (T0 + 80, T0 + 80))
    dav.put("both.txt", dav.get("both.txt"), T0 + 81)


def s_delete_dir(local, dav):
    shutil.rmtree(local / "deepest")
    dav.delete("srvdir-renamed")


def s_names(local, dav):
    # NFC and NFD forms of "café", leading/trailing spaces, unicode.
    write(local, "caf\u00e9-nfc.txt", "nfc\n", 90)
    write(local, "cafe\u0301-nfd.txt", "nfd\n", 91)
    write(local, " leading.txt", "lead\n", 92)
    write(local, "trailing.txt ", "trail\n", 93)
    write(local, "日本語/ファイル.txt", "jp\n", 94)
    os.utime(local / "日本語", (T0 + 94, T0 + 94))
    dav.put("srv-\u00fcml\u00e4ut.txt", "umlaut\n", T0 + 95)
    dav.put("server name with spaces .txt", "spaces\n", T0 + 96)


def s_many(local, dav):
    for i in range(150):
        write(local, f"many/f{i:03}.txt", f"{i}\n" * (i + 1), 100 + i)
    os.utime(local / "many", (T0 + 99, T0 + 99))
    dav.mkcol("srvmany")
    for i in range(40):
        dav.put(f"srvmany/s{i:02}.txt", f"s{i}\n", T0 + 300 + i)


def s_same_dir_both(local, dav):
    write(local, "both-dir/local.txt", "l\n", 400)
    write(local, "both-dir/same.txt", "same\n", 401)
    os.utime(local / "both-dir", (T0 + 400, T0 + 400))
    dav.mkcol("both-dir")
    dav.put("both-dir/remote.txt", "r\n", T0 + 402)
    dav.put("both-dir/same.txt", "same\n", T0 + 401)


def s_remote_type_change(local, dav):
    dav.delete("many/f000.txt")
    dav.mkcol("many/f000.txt")
    dav.put("many/f000.txt/inside.txt", "in\n", T0 + 410)
    dav.delete("srvmany")
    dav.put("srvmany", "now a file\n", T0 + 411)


def s_case_dir(local, dav):
    os.rename(local / "many", local / "Many")
    write(local, "Many/new.txt", "n\n", 420)


def s_deletes_both(local, dav):
    shutil.rmtree(local / "Many")
    dav.delete("both-dir")
    os.remove(local / "Case.txt")
    dav.delete("case.txt")


STEPS = [
    ("initial", s_initial, None),
    ("local-changes", s_local_changes, None),
    ("remote-changes", s_remote_changes, None),
    ("chunked", s_chunked, {"OWNCLOUD_CHUNK_SIZE": "1000000", "OWNCLOUD_MIN_CHUNK_SIZE": "1000000", "OWNCLOUD_MAX_CHUNK_SIZE": "1000000"}),
    ("conflicts", s_conflicts, None),
    ("type-conflict", s_type_conflict, None),
    ("moves-both-sides", s_moves_both_sides, None),
    ("case", s_case, None),
    ("idle", s_nothing, None),
    ("mtime-only", s_mtime_only, None),
    ("delete-dir", s_delete_dir, None),
    ("idle-2", s_nothing, None),
    ("names", s_names, None),
    ("many", s_many, None),
    ("same-dir-both", s_same_dir_both, None),
    ("remote-type-change", s_remote_type_change, None),
    ("case-dir", s_case_dir, None),
    ("deletes-both", s_deletes_both, None),
    ("idle-3", s_nothing, None),
]


def snapshot(local, dav):
    server = dav.tree()
    loc = local_tree(local)
    j = journal_dump(local, server, loc)
    return {
        "local": normalize_tree(loc, ("inode",)),
        "server": normalize_tree(server, ("etag", "fileid")),
        "journal": j,
    }


def diff(a, b, path=""):
    out = []
    if isinstance(a, dict) and isinstance(b, dict):
        for k in sorted(set(a) | set(b), key=str):
            if k not in a:
                out.append(f"{path}/{k}: only ncsync: {json.dumps(b[k], default=str)[:400]}")
            elif k not in b:
                out.append(f"{path}/{k}: only oracle: {json.dumps(a[k], default=str)[:400]}")
            else:
                out += diff(a[k], b[k], f"{path}/{k}")
    elif isinstance(a, list) and isinstance(b, list) and a != b:
        sa, sb = set(map(str, a)), set(map(str, b))
        for x in sorted(sa - sb):
            out.append(f"{path}: only oracle: {x[:400]}")
        for x in sorted(sb - sa):
            out.append(f"{path}: only ncsync: {x[:400]}")
        if sa == sb:
            out.append(f"{path}: same items, different order")
    elif a != b:
        out.append(f"{path}: oracle={json.dumps(a, default=str)[:300]} ncsync={json.dumps(b, default=str)[:300]}")
    return out


def run_side_by_side(args):
    users = {"oracle": "bench-oracle", "ncsync": "bench-ncsync"}
    locals_ = {}
    for c, u in users.items():
        reset_user(u)
        locals_[c] = WORK / c
        shutil.rmtree(locals_[c], ignore_errors=True)
        locals_[c].mkdir(parents=True)
    report = []
    for name, action, env in STEPS:
        snaps = {}
        for c, u in users.items():
            dav = Dav(u)
            try:
                action(locals_[c], dav)
            except (OSError, RuntimeError) as e:
                print(f"   ({c}: step action failed: {e})")
            code, log = CLIENTS[c](locals_[c], u, env)
            (WORK / f"{name}.{c}.log").write_text(log)
            # A second sync settles what the first left for later (conflict
            # uploads, ...): both clients get it.
            code2, log2 = CLIENTS[c](locals_[c], u, env)
            (WORK / f"{name}.{c}.2.log").write_text(log2)
            snaps[c] = snapshot(locals_[c], dav)
            snaps[c]["exit"] = [code, code2]
        d = diff(snaps["oracle"], snaps["ncsync"])
        (WORK / f"{name}.json").write_text(json.dumps(snaps, indent=1, sort_keys=True, default=str))
        print(f"== {name}: {'same' if not d else str(len(d)) + ' difference(s)'}")
        for line in d:
            print("   " + line)
        report.append((name, d))
    return report


def run_roundtrip(args):
    """One user, one folder: the clients alternate, step by step."""
    user = "bench-roundtrip"
    reset_user(user)
    local = WORK / "roundtrip"
    shutil.rmtree(local, ignore_errors=True)
    local.mkdir(parents=True)
    dav = Dav(user)
    order = ["oracle", "ncsync"]
    problems = []
    for i, (name, action, env) in enumerate(STEPS):
        c = order[i % 2]
        other = order[(i + 1) % 2]
        action(local, dav)
        CLIENTS[c](local, user, env)
        CLIENTS[c](local, user, env)
        before = snapshot(local, dav)
        # The other client takes the folder over: nothing to propagate.
        code, log = CLIENTS[other](local, user, env)
        (WORK / f"roundtrip.{name}.{other}.log").write_text(log)
        after = snapshot(local, dav)
        d = diff(before, after)
        print(f"== {name}: synced by {c}, idle takeover by {other}: {'same' if not d else str(len(d)) + ' change(s)'}")
        for line in d:
            print("   " + line)
        problems.append((name, d))
    return problems


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--roundtrip", action="store_true")
    args = ap.parse_args()
    os.umask(0o022)
    WORK.mkdir(parents=True, exist_ok=True)
    if not (ORACLE / "build/bin/nextcloudcmd").exists():
        sys.exit(f"no nextcloudcmd in {ORACLE}/build/bin (tools/bench/build-oracle.sh)")
    if not NCSYNC.exists():
        sys.exit(f"no ncsync at {NCSYNC} (cargo build -p ncsync)")
    report = run_roundtrip(args) if args.roundtrip else run_side_by_side(args)
    n = sum(len(d) for _, d in report)
    print(f"\n{n} difference(s) in {sum(1 for _, d in report if d)} step(s); details in {WORK}")
    return 1 if n else 0


if __name__ == "__main__":
    sys.exit(main())
