#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
# SPDX-License-Identifier: GPL-2.0-or-later
#
# End-to-end check of `ncsyncd` against a throw-away Nextcloud with redis
# and notify_push (the `ncrs-itest` compose project: its own network,
# volume and the loopback-only port 127.0.0.1:18080; it never touches any
# other container).
#
#   tools/itest/run-daemon.sh [workdir]   # (re)creates the stack and runs the scenario
#   tools/itest/run.sh --down             # removes the stack and its volume
#
# The remote poll interval is set to 10 minutes, so a remote change seen
# within seconds came through notify_push; a local change seen within
# seconds came through inotify.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
compose=(docker compose -f "$here/compose.yml" -p ncrs-itest)
root="$(cd "$here/../.." && pwd)"
work="${1:-$HOME/.cache/ncrs-itest-daemon}"
url=http://127.0.0.1:18080
user=admin
pass=ncrs-itest-admin-pw
dav="$url/remote.php/dav/files/$user"
auth=(-u "$user:$pass")
ncsync="$root/target/debug/ncsync"
ncsyncd="$root/target/debug/ncsyncd"
occ() { docker exec -u www-data ncrs-itest-nc php occ "$@"; }
fail() { echo "FAIL: $*" >&2; [[ -f "$work/daemon.log" ]] && tail -40 "$work/daemon.log" >&2; exit 1; }
remote_has() { [[ "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" -X PROPFIND -H 'Depth: 0' "$dav/$1")" == 207 ]]; }
# wait_for SECONDS DESCRIPTION COMMAND...: prints the time it took
wait_for() {
    local limit=$1 what=$2 start=$SECONDS
    shift 2
    while ! "$@"; do
        (( SECONDS - start < limit )) || fail "$what not within ${limit}s"
        sleep 0.5
    done
    echo "   $what after $(( SECONDS - start ))s"
}

(cd "$root" && cargo build -q -p ncsync -p ncsyncd)

echo "0. fresh server with redis and notify_push"
"${compose[@]}" --profile push down -v 2>/dev/null || true
"${compose[@]}" up -d
for _ in $(seq 120); do
    curl -s "$url/status.php" | grep -q '"installed":true' && break
    sleep 5
done
curl -s "$url/status.php" | grep -q '"installed":true' || fail "server not ready"
occ app:install notify_push >/dev/null
subnet="$(docker network inspect ncrs-itest-net -f '{{(index .IPAM.Config 0).Subnet}}')"
occ config:system:set trusted_proxies 0 --value="$subnet" >/dev/null
"${compose[@]}" --profile push up -d push
for _ in $(seq 30); do
    occ notify_push:setup "$url/push" >/dev/null 2>&1 && break
    sleep 2
done
occ notify_push:setup "$url/push" | grep -q "configuration saved" || fail "notify_push setup"
app_password="$(docker exec -e NC_PASS="$pass" -u www-data ncrs-itest-nc php occ user:auth-tokens:add --password-from-env --name=ncsyncd-itest "$user" | tail -1)"
[[ -n "$app_password" ]] || fail "no app password"

echo "1. configuration: account and folder"
pkill_daemon() { [[ -f "$work/daemon.pid" ]] && kill "$(cat "$work/daemon.pid")" 2>/dev/null || true; }
pkill_daemon
rm -rf "$work" && mkdir -p "$work/local" "$work/config" "$work/state" "$work/run"
export XDG_CONFIG_HOME="$work/config" XDG_STATE_HOME="$work/state" XDG_RUNTIME_DIR="$work/run"
printf '%s\n' "$app_password" > "$work/app-password"
"$ncsync" account add "$url" -u "$user" --app-password-file "$work/app-password" \
    || fail "account add"
"$ncsync" folder add "$work/local" || fail "folder add"
cfg="$XDG_CONFIG_HOME/ncsyncd/ncsyncd.cfg"
# no etag polling to speak of: remote changes must come through notify_push
printf '\n[Nextcloud]\nremotePollInterval=600000\n' >> "$cfg"
"$ncsync" account list
"$ncsync" folder list

echo "2. daemon start and initial sync"
"$ncsyncd" --official-config /nonexistent > "$work/daemon.log" 2>&1 &
echo $! > "$work/daemon.pid"
trap pkill_daemon EXIT
wait_for 60 "control socket" test -S "$XDG_RUNTIME_DIR/ncsyncd/control.sock"
wait_for 90 "initial download" test -f "$work/local/Readme.md"
status() { "$ncsync" status --json; }
wait_for 60 "push ready" grep -q "Push notifications ready" "$work/daemon.log"
idle() { status | python3 -c 'import json,sys; d=json.load(sys.stdin); f=d["folders"][0]; sys.exit(0 if f["status"]=="Success" and not f["syncing"] and not f["scheduled"] else 1)'; }
wait_for 60 "folder idle after the initial sync" idle

echo "3. remote change -> notify_push"
echo from-server | curl -s "${auth[@]}" -T - "$dav/pushed.txt"
wait_for 15 "remote file via notify_push" test -f "$work/local/pushed.txt"
grep -q "reason=notify_file" "$work/daemon.log" || fail "the sync was not triggered by notify_push"

echo "4. local change -> inotify"
wait_for 60 "idle" idle
echo local-change > "$work/local/fromlocal.txt"
wait_for 20 "local file uploaded via inotify" remote_has fromlocal.txt

echo "5. control: pause, resume, sync-now"
"$ncsync" pause | grep -q paused || fail "pause"
[[ "$(status | python3 -c 'import json,sys; print(json.load(sys.stdin)["folders"][0]["paused"])')" == True ]] || fail "not paused"
echo while-paused | curl -s "${auth[@]}" -T - "$dav/paused.txt"
sleep 5
[[ -e "$work/local/paused.txt" ]] && fail "synced while paused"
"$ncsync" resume | grep -q resumed || fail "resume"
"$ncsync" sync-now 1 | grep -q scheduled || "$ncsync" sync-now | grep -q scheduled || fail "sync-now"
wait_for 30 "sync after resume" test -f "$work/local/paused.txt"
"$ncsync" status

echo "6. clean stop"
kill -TERM "$(cat "$work/daemon.pid")"
for _ in $(seq 60); do kill -0 "$(cat "$work/daemon.pid")" 2>/dev/null || break; sleep 0.5; done
kill -0 "$(cat "$work/daemon.pid")" 2>/dev/null && fail "daemon did not stop"
grep -q "shutting down" "$work/daemon.log" || fail "no clean shutdown"
trap - EXIT

echo "all good (tear down with: $here/run.sh --down)"
