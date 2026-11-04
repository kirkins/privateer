/* Static, authority-free exec-policy fixture. Never a real deployment adapter. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

extern char **environ;
int main(int argc, char **argv) {
    if (argc != 2 || !environ[0] || environ[1] ||
        strcmp(environ[0], "PRIVATEER_ADAPTER_PROTOCOL=1")) return 10;
    if (umask(077) != 077) return 11;
    for (int fd = 3; fd < 1024; fd++)
        if (fcntl(fd, F_GETFD) != -1 || errno != EBADF) return 12;
    struct stat null_stat, actual;
    if (stat("/dev/null", &null_stat)) return 13;
    for (int fd = 0; fd < 3; fd++)
        if (fstat(fd, &actual) || actual.st_rdev != null_stat.st_rdev ||
            !S_ISCHR(actual.st_mode)) return 14;
    if (stat(".", &actual) || (actual.st_mode & 0777) != 0700) return 15;
    if (getsid(0) != getpid() || getpgrp() != getpid()) return 16;
    int tty = open("/dev/tty", O_RDONLY | O_NOCTTY);
    if (tty >= 0 || (errno != ENXIO && errno != ENOENT)) return 17;
    uint64_t blocked = UINT64_MAX;
    if (syscall(SYS_rt_sigprocmask, SIG_SETMASK, NULL, &blocked, 8) || blocked)
        return 18;
    for (int sig = 1; sig <= 64; sig++) {
        if (sig == SIGKILL || sig == SIGSTOP) continue;
        uint64_t action[4] = {0};
        if (syscall(SYS_rt_sigaction, sig, NULL, action, 8) || action[0])
            return 19;
    }
    FILE *status = fopen("/proc/self/status", "re");
    if (!status) return 20;
    char line[256];
    int facts = 0;
    while (fgets(line, sizeof(line), status)) {
        if (!strcmp(line, "CapEff:\t0000000000000000\n")) facts |= 1;
        if (!strcmp(line, "CapPrm:\t0000000000000000\n")) facts |= 2;
        if (!strcmp(line, "NoNewPrivs:\t1\n")) facts |= 4;
    }
    fclose(status);
    if (facts != 7) return 21;
    int marker = open(argv[1], O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (marker < 0 || write(marker, "executed", 8) != 8 || close(marker)) return 22;
    return 0;
}
