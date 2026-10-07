#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
# SPDX-License-Identifier: GPL-2.0-or-later
#
# End-to-end check of `ncsync sync` against a throw-away Nextcloud container.
#
# The stack is its own compose project (`ncrs-itest`): its own network,
# volume and a loopback-only port (127.0.0.1:18080). It never touches any
# other container. Usage:
#
#   tools/itest/run.sh [workdir]     # starts the stack, runs the scenario
#   tools/itest/run.sh --down        # removes the stack and its volume
#
# The work directory (default: ~/.cache/ncrs-itest) holds the local folder.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
compose=(docker compose -f "$here/compose.yml" -p ncrs-itest)

if [[ "${1:-}" == "--down" ]]; then
    "${compose[@]}" down -v
    exit 0
fi

work="${1:-$HOME/.cache/ncrs-itest}"
root="$(cd "$here/../.." && pwd)"
ncsync="$root/target/debug/ncsync"
url=http://127.0.0.1:18080
user=admin
pass=ncrs-itest-admin-pw
dav="$url/remote.php/dav/files/$user"
auth=(-u "$user:$pass")

(cd "$root" && cargo build -q -p ncsync)
"${compose[@]}" down -v 2>/dev/null || true # start from a fresh installation (skeleton files)
"${compose[@]}" up -d
echo "waiting for the server installation..."
for _ in $(seq 120); do
    curl -s "$url/status.php" | grep -q '"installed":true' && break
    sleep 5
done
curl -s "$url/status.php" | grep -q '"installed":true' || { echo "server not ready" >&2; exit 1; }

rm -rf "$work" && mkdir -p "$work/local"
local="$work/local"
sync() { "$ncsync" sync -s --non-interactive -u "$user" -p "$pass" "$@" "$local" "$url"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
remote_has() { [[ "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" -X PROPFIND -H 'Depth: 0' "$dav/$1")" == 207 ]]; }

echo "1. initial sync: local upload + skeleton download"
echo hello > "$local/hello.txt"
mkdir -p "$local/sub"
head -c 300000 /dev/urandom > "$local/sub/random.bin"
sync
remote_has hello.txt || fail "hello.txt not uploaded"
remote_has sub/random.bin || fail "sub/random.bin not uploaded"
[[ -f "$local/Readme.md" ]] || fail "skeleton not downloaded"

echo "2. local edit, delete and move"
echo changed > "$local/hello.txt"
rm "$local/Readme.md"
mv "$local/Nextcloud.png" "$local/sub/moved.png"
sync
[[ "$(curl -s "${auth[@]}" "$dav/hello.txt")" == changed ]] || fail "edit not uploaded"
remote_has Readme.md && fail "Readme.md not deleted remotely"
remote_has sub/moved.png || fail "move not propagated"
remote_has Nextcloud.png && fail "move source still there"

echo "3. remote new file, folder, move, delete; chunked upload (1 MB chunks)"
echo from-server | curl -s "${auth[@]}" -T - "$dav/fromserver.txt"
curl -s "${auth[@]}" -X MKCOL "$dav/srvdir"
curl -s "${auth[@]}" -X MOVE -H "Destination: $dav/srvdir/renamed.pdf" "$dav/Reasons%20to%20use%20Nextcloud.pdf"
curl -s "${auth[@]}" -X DELETE "$dav/Templates%20credits.md"
head -c 5000000 /dev/urandom > "$local/big.bin"
OWNCLOUD_CHUNK_SIZE=1000000 OWNCLOUD_MIN_CHUNK_SIZE=1000000 OWNCLOUD_MAX_CHUNK_SIZE=1000000 sync
[[ -f "$local/fromserver.txt" ]] || fail "fromserver.txt not downloaded"
[[ -f "$local/srvdir/renamed.pdf" ]] || fail "remote move not applied"
[[ -e "$local/Reasons to use Nextcloud.pdf" ]] && fail "remote move source still there"
[[ -e "$local/Templates credits.md" ]] && fail "remote delete not applied"
cmp -s "$local/big.bin" <(curl -s "${auth[@]}" "$dav/big.bin") || fail "chunked upload differs"

echo "4. conflict"
echo local-version > "$local/hello.txt"
echo server-version-longer | curl -s "${auth[@]}" -T - "$dav/hello.txt"
sync || true # a conflict is reported as a sync error
[[ "$(cat "$local/hello.txt")" == server-version-longer ]] || fail "server version not downloaded"
compgen -G "$local/hello (conflicted copy *).txt" > /dev/null || fail "no conflict copy"

echo "5. idle sync propagates nothing"
out="$("$ncsync" sync --non-interactive -u "$user" -p "$pass" "$local" "$url" 2>&1)"
n="$(grep -c 'Starting ' <<< "$out" || true)"
[[ "$n" -le 1 ]] || fail "idle sync propagated $n items"

echo "all good (tear down with: $0 --down)"
