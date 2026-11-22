// Linux fault fixture adapted from the I46 review. Fail only the named
// directory barrier, and only after the publication destination exists.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int fsync(int fd) {
    int (*real_fsync)(int) = dlsym(RTLD_NEXT, "fsync");
    const char *target = getenv("I46_SYNC_DIRECTORY");
    const char *destination = getenv("I46_SYNC_AFTER");
    char link[64], path[PATH_MAX + 1];
    snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
    ssize_t size = readlink(link, path, PATH_MAX);
    if (size >= 0 && target && destination) {
        path[size] = 0;
        if (strcmp(target, path) == 0 && access(destination, F_OK) == 0) {
            errno = EIO;
            return -1;
        }
    }
    return real_fsync(fd);
}
