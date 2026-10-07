#!/bin/sh
# SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
# SPDX-License-Identifier: GPL-2.0-or-later
#
# Regenerate crates/nc-journal/tests/fixtures/jhash_vectors.txt from the
# upstream C implementation.
# Usage: tools/jhash-oracle/run.sh /path/to/nextcloud-desktop-checkout
set -eu
UPSTREAM=${1:?usage: run.sh <nextcloud-desktop checkout>}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
WORK=$(mktemp -d "${TMPDIR:-$ROOT/target}/jhash-oracle.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
# c_jhash.h includes <QtCore/qglobal.h> only for Q_FALLTHROUGH: stub it.
mkdir -p "$WORK/QtCore"
: > "$WORK/QtCore/qglobal.h"
cc -O2 -Wall -I"$WORK" -I"$UPSTREAM/src" -o "$WORK/oracle" "$HERE/oracle.c"
"$WORK/oracle" > "$ROOT/crates/nc-journal/tests/fixtures/jhash_vectors.txt"
echo "wrote $(wc -l < "$ROOT/crates/nc-journal/tests/fixtures/jhash_vectors.txt") vectors"
