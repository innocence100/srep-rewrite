/*
 * Independent XXH3-128 golden-vector generator for the SREP-NG v2 checksum
 * audit (design spec docs/superpowers/specs/2026-08-30-srep-capability-
 * fidelity-design.md section 10.10).
 *
 * This program is deliberately standalone and tiny. It does NOT reimplement
 * XXH3; it links against the *official* upstream C reference implementation
 * and only drives the public oneshot and streaming APIs. Its sole job is to
 * produce the project-owned golden fixture consumed by tests/checksum_vectors.rs.
 *
 * Determinism contract (must be mirrored exactly by the Rust test that
 * regenerates the inputs):
 *
 *   profile "xor":      b[i] = (u8)((u32)i * 131 + 17) XOR (u8)(i >> 3)
 *   profile "zeros":    b[i] = 0x00
 *   profile "repeated": b[i] = 0xAB
 *   profile "descend":  b[i] = (u8)(255 - (i & 0xff))
 *
 * Serialization contract: XXH3-128 is emitted as low64 little-endian followed
 * by high64 little-endian (NOT the big-endian canonical XXH128 form).
 *
 * Output: a self-describing JSON fixture on stdout. No timestamps, host
 * names, paths, or compiler identity are written into the fixture. Provenance
 * is fixed in scripts/checksum-provenance.txt and checked by the wrapper.
 *
 * Licence: this generator is trivial project code (MIT, same as the repository).
 * The linked reference implementation remains BSD-2-Clause, (c) Yann Collet.
 */

#define XXH_STATIC_LINKING_ONLY
#include "xxhash.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define MAX_LEN 262144

static unsigned char buf[MAX_LEN];

/* Mirror of the Rust input generator. Keep byte-identical. */
static void fill_profile(const char *profile, size_t len) {
    for (size_t i = 0; i < len; i++) {
        unsigned char v;
        if (strcmp(profile, "xor") == 0) {
            v = (unsigned char)((unsigned)(i * 131u + 17u) ^ (unsigned)(i >> 3));
        } else if (strcmp(profile, "zeros") == 0) {
            v = 0x00;
        } else if (strcmp(profile, "repeated") == 0) {
            v = 0xAB;
        } else {
            v = (unsigned char)(255u - (unsigned)(i & 0xffu));
        }
        buf[i] = v;
    }
}

/* Numeric value in canonical (big-endian textual) hex. This is the low/high
 * 64-bit WORD value, not a byte order. The wire byte order is the separate
 * "serialized_hex" field, which is low64 little-endian then high64
 * little-endian. */
static void print_value_hex64(unsigned long long v) {
    printf("%016llx", v);
}

static int emit_hex16(const unsigned char *p) {
    for (int i = 0; i < 16; i++) printf("%02x", (unsigned)p[i]);
    return 0;
}

/* Portable, dependency-free FNV-1a-64 over the input bytes. This is NOT a
 * cryptographic anchor; it exists so the Rust test can prove it regenerated
 * byte-identical input and fail loudly (rather than silently comparing a
 * different message) if the two input generators ever drift. */
static unsigned long long fnv1a64(const unsigned char *p, size_t len) {
    unsigned long long h = 0xcbf29ce484222325ULL;
    for (size_t i = 0; i < len; i++) {
        h ^= (unsigned long long)p[i];
        h *= 1099511628211ULL;
    }
    return h;
}

/* Streaming over a fixed chunk size. Returns 0 iff digest == oneshot. */
static int stream_value(size_t len, size_t chunk, XXH128_hash_t *out) {
    XXH3_state_t state;
    if (XXH3_128bits_reset(&state) != XXH_OK) return -1;
    size_t off = 0;
    while (off < len) {
        size_t n = len - off;
        if (n > chunk) n = chunk;
        if (XXH3_128bits_update(&state, buf + off, n) != XXH_OK) return -1;
        off += n;
    }
    *out = XXH3_128bits_digest(&state);
    return 0;
}

static int check_streaming(size_t len, XXH128_hash_t oneshot) {
    static const size_t chunks[] = {1, 2, 3, 7, 8, 15, 16, 31, 63, 64, 127,
                                    128, 240, 241, 255, 256, 257, 512, 1024,
                                    4096, 65536};
    for (size_t i = 0; i < sizeof(chunks) / sizeof(chunks[0]); i++) {
        XXH128_hash_t got;
        if (stream_value(len, chunks[i], &got) != 0) return -1;
        if (got.low64 != oneshot.low64 || got.high64 != oneshot.high64) {
            fprintf(stderr,
                    "MISMATCH len=%zu chunk=%zu oneshot=%016llx:%016llx "
                    "stream=%016llx:%016llx\n",
                    len, chunks[i], (unsigned long long)oneshot.low64,
                    (unsigned long long)oneshot.high64,
                    (unsigned long long)got.low64,
                    (unsigned long long)got.high64);
            return -1;
        }
    }
    return 0;
}

struct prof_len {
    const char *profile;
    size_t len;
};

/* Boundary lengths chosen to cover the XXH3 secret/stripe cutovers, the
 * 32-bit size_t range as far as is practical, and a large multi-block case. */
static const struct prof_len cases[] = {
    {"xor", 0},      {"xor", 1},      {"xor", 2},      {"xor", 3},
    {"xor", 4},      {"xor", 8},      {"xor", 15},     {"xor", 16},
    {"xor", 17},     {"xor", 31},     {"xor", 32},     {"xor", 33},
    {"xor", 63},     {"xor", 64},     {"xor", 65},     {"xor", 127},
    {"xor", 128},    {"xor", 129},    {"xor", 239},    {"xor", 240},
    {"xor", 241},    {"xor", 255},    {"xor", 256},    {"xor", 257},
    {"xor", 511},    {"xor", 512},    {"xor", 1023},   {"xor", 1024},
    {"xor", 4096},   {"xor", 9973},   {"xor", 65537},  {"xor", 262144},
    {"zeros", 0},    {"zeros", 1},    {"zeros", 16},   {"zeros", 240},
    {"zeros", 241},  {"zeros", 1024}, {"zeros", 65537},
    {"repeated", 0}, {"repeated", 1}, {"repeated", 16}, {"repeated", 241},
    {"repeated", 1024}, {"repeated", 65537},
    {"descend", 0},  {"descend", 1},  {"descend", 16}, {"descend", 240},
    {"descend", 241}, {"descend", 1024},
};

int main(void) {
    printf("{\n");
    printf("  \"schema\": \"srep-checksum-xxh3-v1\",\n");
    printf("  \"algorithm\": \"XXH3-128\",\n");
    printf("  \"seed\": 0,\n");
    printf("  \"secret\": \"official-default\",\n");
    printf("  \"serialization\": \"low64-le-then-high64-le\",\n");
    printf("  \"reference\": {\n");
    printf("    \"project\": \"Cyan4973/xxHash\",\n");
    printf("    \"tag\": \"v0.8.2\",\n");
    printf("    \"tag_commit\": \"bbb27a5efb85b92a0486cf361a8635715a53f6ba\",\n");
    printf("    \"xxhash_h_sha256\": "
           "\"be275e9db21a503c37f24683cdb4908f2370a3e35ab96e02c4ea73dc8e399c43\",\n");
    printf("    \"xxhash_c_sha256\": "
           "\"685ac6e9e32d1b5800ccb67cf3698c7f3f30130c159d385b5de34bf4d07a8e70\"\n");
    printf("  },\n");
    printf("  \"vectors\": [\n");

    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
        const char *profile = cases[i].profile;
        size_t len = cases[i].len;
        fill_profile(profile, len);
        XXH128_hash_t h = XXH3_128bits(buf, len);
        if (check_streaming(len, h) != 0) {
            return 1;
        }
        unsigned char ser[16];
        for (int k = 0; k < 8; k++) {
            ser[k] = (unsigned char)((h.low64 >> (8 * k)) & 0xffu);
            ser[8 + k] = (unsigned char)((h.high64 >> (8 * k)) & 0xffu);
        }
        printf("    {\"profile\": \"%s\", \"len\": %zu, ", profile, len);
        printf("\"input_fnv1a64\": \"%016llx\", ",
               fnv1a64(buf, len));
        printf("\"low64\": \"");
        print_value_hex64((unsigned long long)h.low64);
        printf("\", \"high64\": \"");
        print_value_hex64((unsigned long long)h.high64);
        printf("\", \"serialized_hex\": \"");
        emit_hex16(ser);
        printf("\"}%s\n", (i + 1 < sizeof(cases) / sizeof(cases[0])) ? "," : "");
    }

    printf("  ]\n");
    printf("}\n");
    return 0;
}
