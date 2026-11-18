/* Test-only fsync failures, restricted to an explicitly selected case directory. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <limits.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int fsync(int fd) {
    static _Atomic int injected = 0;
    int (*real_fsync)(int) = dlsym(RTLD_NEXT, "fsync");
    const char *root = getenv("CQ_R0_REVIEW_FSYNC_ROOT");
    const char *mode = getenv("CQ_R0_FAULT_MODE");
    struct stat st;
    if (root && mode && fstat(fd, &st) == 0) {
        char link[80], path[PATH_MAX], marker[PATH_MAX], head[PATH_MAX];
        snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
        ssize_t n = readlink(link, path, sizeof(path)-1);
        if (n >= 0) {
            path[n] = '\0';
            snprintf(marker, sizeof(marker), "%s/INJECT_DIRECTORY_FSYNC", root);
            snprintf(head, sizeof(head), "%s/HEAD.tmp", root);
            int selected = strcmp(mode, "head-file") == 0
                ? (S_ISREG(st.st_mode) && strcmp(path, head) == 0)
                : (S_ISDIR(st.st_mode) && strcmp(path, root) == 0);
            int repeated = strcmp(mode, "directory-repeat") == 0;
            if (selected && access(marker, F_OK) == 0 && (repeated || !atomic_exchange(&injected, 1))) {
                fprintf(stderr, "R0_FAULT injected fsync EIO mode=%s path=%s\n", mode, path);
                errno = EIO;
                return -1;
            }
        }
    }
    return real_fsync(fd);
}
