#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
# SPDX-License-Identifier: GPL-2.0-or-later
#
# Builds the oracle once: nextcloudcmd and the upstream test binaries of the
# pinned tag, inside the upstream CI image (Qt 6, all dependencies), as the
# calling user. The official AppImage does not ship nextcloudcmd.
#
#   tools/bench/build-oracle.sh [ORACLE_DIR] [TARGETS...]
#
# ORACLE_DIR (default ~/.cache/ncrs-oracle/nccmd-34.0.5) gets src/ (a copy
# of the tag), build/ (build/bin/nextcloudcmd, build/bin/*Test).
# TARGETS default to `nextcloudcmd SyncMoveTest`; e.g. add SyncEngineTest.
set -euo pipefail
tag=v34.0.5
commit=62ebad6043b1e7c8e319f41be25d41f5a2c733e5
image=ghcr.io/nextcloud/continuous-integration-client-qt6:client-sid-6.10.2-4
dir="${1:-$HOME/.cache/ncrs-oracle/nccmd-34.0.5}"
shift || true
targets=("$@")
[[ ${#targets[@]} -gt 0 ]] || targets=(nextcloudcmd SyncMoveTest)
mkdir -p "$dir/build"
if [[ ! -d "$dir/src" ]]; then
    git clone --quiet --depth 1 --branch "$tag" https://github.com/nextcloud/desktop.git "$dir/src"
    [[ "$(git -C "$dir/src" rev-parse HEAD)" == "$commit" ]] || { echo "unexpected commit" >&2; exit 1; }
fi
cat > "$dir/build.sh" <<SH
#!/bin/sh
set -e
cd /work/build
[ -f build.ninja ] || cmake ../src -G Ninja -DCMAKE_BUILD_TYPE=Release -DQT_MAJOR_VERSION=6 -DBUILD_UPDATER=OFF -DBUILD_TESTING=1
ninja -j2 ${targets[*]}
SH
chmod +x "$dir/build.sh"
docker run --rm --name ncrs-oracle-build --user "$(id -u):$(id -g)" -e HOME=/work -v "$dir:/work" "$image" /work/build.sh
