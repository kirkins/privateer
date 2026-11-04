/* Ordinary native protocol fixture. Not a containment qualification. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void write_all(int fd, const void *bytes, size_t count) {
    const char *p = bytes;
    while (count) {
        ssize_t n = write(fd, p, count);
        if (n <= 0) _exit(90);
        p += n;
        count -= (size_t)n;
    }
}
static void read_all(void *bytes, size_t count) {
    char *p = bytes;
    while (count) {
        ssize_t n = read(0, p, count);
        if (n <= 0) _exit(91);
        p += n;
        count -= (size_t)n;
    }
}
static void reply(const char *json, int fragmented, int extra, int truncated) {
    size_t size = strlen(json);
    unsigned char prefix[4] = {0, 0, (unsigned char)(size >> 8), (unsigned char)size};
    if (fragmented) {
        for (size_t i = 0; i < 4; i++) write_all(1, prefix + i, 1);
        for (size_t i = 0; i < size; i++) write_all(1, json + i, 1);
    } else {
        write_all(1, prefix, 4);
        write_all(1, json, size - (truncated ? 1 : 0));
    }
    if (extra) write_all(1, "x", 1);
}
int main(int argc, char **argv) {
    if (argc != 2) return 92;
    const char *mode = argv[1];
    const char *success = "{\"schema_version\":1,\"status\":\"success\"}";
    if (!strcmp(mode, "hang")) { for (;;) pause(); }
    if (!strcmp(mode, "closed-input")) { close(0); return 0; }
    if (!strcmp(mode, "early-success")) { reply(success, 0, 0, 0); return 0; }
    if (!strcmp(mode, "output-limit") || !strcmp(mode, "continuous")) {
        char bytes[4096];
        memset(bytes, 'x', sizeof(bytes));
        for (;;) write_all(2, bytes, !strcmp(mode, "continuous") ? 1 : sizeof(bytes));
    }
    unsigned char prefix[4];
    read_all(prefix, 4);
    uint32_t size = ((uint32_t)prefix[0] << 24) | ((uint32_t)prefix[1] << 16) |
        ((uint32_t)prefix[2] << 8) | prefix[3];
    if (!size || size > 16384) return 93;
    char metadata[16385];
    read_all(metadata, size);
    metadata[size] = 0;
    if (!strstr(metadata, "\"schema_version\":1") || strstr(metadata, "path") ||
        strstr(metadata, "config") || strstr(metadata, "credential")) return 94;
    char bytes[97];
    for (;;) {
        ssize_t n = read(0, bytes, sizeof(bytes));
        if (n < 0) return 95;
        if (!n) break;
        for (ssize_t i = 0; i < n; i++) if (bytes[i] != 'a') return 96;
    }
    if (!strcmp(mode, "empty")) return 0;
    if (!strcmp(mode, "oversize-result")) { write_all(1, "\0\0\40\0", 4); return 0; }
    if (!strcmp(mode, "failure")) reply("{\"schema_version\":1,\"status\":\"failure\"}", 0, 0, 0);
    else if (!strcmp(mode, "malformed")) reply("{\"schema_version\":1,\"status\":\"success\",\"secret\":\"beta\"}", 0, 0, 0);
    else reply(success, !strcmp(mode, "fragmented"), !strcmp(mode, "extra"), !strcmp(mode, "truncated"));
    write_all(2, "beta", 4);
    return !strcmp(mode, "nonzero") ? 42 : 0;
}
