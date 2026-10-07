/*
 * SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
 * SPDX-License-Identifier: GPL-2.0-or-later
 *
 * Oracle for the Rust port of c_jhash: compiled against the *upstream*
 * src/common/c_jhash.h (see run.sh). Prints one line per vector:
 *   <key hex or "-" for empty> <initval64 hex> <c_jhash64 hex> <initval32 hex> <c_jhash hex>
 */
#include <stdio.h>
#include <string.h>
#include "common/c_jhash.h"

static void emit(const uint8_t *k, size_t len, uint64_t i64, uint32_t i32)
{
    size_t n;
    if (len == 0) {
        printf("-");
    }
    for (n = 0; n < len; ++n) {
        printf("%02x", k[n]);
    }
    printf(" %016llx %016llx %08x %08x\n", (unsigned long long)i64,
           (unsigned long long)c_jhash64(k, len, i64), i32, c_jhash(k, (uint32_t)len, i32));
}

int main(void)
{
    static const uint64_t inits64[] = {0ULL, 1ULL, 0xdeadbeefcafebabeULL, 0xffffffffffffffffULL};
    static const uint32_t inits32[] = {0U, 1U, 0xdeadbeefU, 0xffffffffU};
    static const char *paths[] = {
        "", "A", "A/a1", "A/a2", "B/b1", "foo/bar/baz.txt", "Документы/отчёт.pdf",
        "日本語/ファイル.txt", "e\xcc\x81" "cole/caf\xc3\xa9", "\xc3\xa9" "cole/caf\xc3\xa9",
        "a/very/long/path/that/goes/well/beyond/twenty/four/bytes/and/more/than/one/block.bin",
        ".sync-exclude.lst", "Photos/2026/IMG_0001.HEIC", "with space/and (parens) [brackets]",
        NULL};
    uint8_t buf[256];
    size_t len, i;
    int v;

    for (len = 0; len <= 100; ++len) {
        for (i = 0; i < len; ++i) {
            buf[i] = (uint8_t)(i * 7 + len * 13 + 0x80);
        }
        for (v = 0; v < 4; ++v) {
            emit(buf, len, inits64[v], inits32[v]);
        }
    }
    for (v = 0; paths[v]; ++v) {
        emit((const uint8_t *)paths[v], strlen(paths[v]), 0, 0);
    }
    return 0;
}
