#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static const char expected[] = "before fork";

int main(void) {
    long page_size = sysconf(_SC_PAGESIZE);
    void *ram_page = mmap(NULL, (size_t)page_size, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (ram_page == MAP_FAILED || mlock(ram_page, (size_t)page_size) != 0) {
        dprintf(STDOUT_FILENO, "MINIDOX_RAM_ALLOC_ERROR errno=%d\n", errno);
        for (;;) pause();
    }
    memcpy(ram_page, expected, sizeof(expected) - 1);

    mkdir("/proc", 0555);
    if (mount("proc", "/proc", "proc", 0, NULL) != 0 && errno != EBUSY) {
        dprintf(STDOUT_FILENO, "MINIDOX_RAM_PROC_ERROR errno=%d\n", errno);
        for (;;) pause();
    }
    int pagemap = open("/proc/self/pagemap", O_RDONLY | O_CLOEXEC);
    uint64_t entry = 0;
    off_t entry_offset = (off_t)((uintptr_t)ram_page / (uintptr_t)page_size) *
                         (off_t)sizeof(entry);
    if (pagemap < 0 || pread(pagemap, &entry, sizeof(entry), entry_offset) != sizeof(entry) ||
        (entry & (UINT64_C(1) << 63)) == 0 ||
        (entry & ((UINT64_C(1) << 55) - 1)) == 0) {
        dprintf(STDOUT_FILENO, "MINIDOX_RAM_PAGEMAP_ERROR errno=%d entry=%#llx\n",
                errno, (unsigned long long)entry);
        for (;;) pause();
    }
    close(pagemap);
    uint64_t ram_gpa = (entry & ((UINT64_C(1) << 55) - 1)) * (uint64_t)page_size;
    dprintf(STDOUT_FILENO, "MINIDOX_RAM_GPA=%#llx\n",
            (unsigned long long)ram_gpa);

    int module = open("/virtiofs.ko", O_RDONLY | O_CLOEXEC);
    if (module < 0 ||
        (syscall(SYS_finit_module, module, "", 0) != 0 && errno != EEXIST)) {
        dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_MODULE_ERROR errno=%d\n", errno);
        for (;;) pause();
    }
    close(module);

    mkdir("/mnt", 0755);
    if (mount("minidox", "/mnt", "virtiofs", 0, "dax=always") != 0) {
        dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_MOUNT_ERROR errno=%d\n", errno);
        for (;;) pause();
    }
    dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_MOUNT_OK\n");

    int fd = -1;
    for (int attempt = 0; attempt < 600 && fd < 0; ++attempt) {
        fd = open("/mnt/state", O_RDONLY | O_CLOEXEC);
        if (fd < 0) {
            if (errno != ENOENT) break;
            usleep(100000);
        }
    }
    if (fd < 0) {
        dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_OPEN_ERROR errno=%d\n", errno);
        for (;;) pause();
    }
    dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_OPEN_OK\n");

    void *mapping = mmap(NULL, 4096, PROT_READ, MAP_SHARED, fd, 0);
    if (mapping == MAP_FAILED) {
        dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_MMAP_ERROR errno=%d\n", errno);
        for (;;) pause();
    }
    dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_MMAP_OK\n");
    if (memcmp((const char *)mapping + 128, expected, sizeof(expected) - 1) != 0) {
        dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_CONTENT_ERROR\n");
        for (;;) pause();
    }

    pid_t reader = fork();
    if (reader == 0) {
        munmap(mapping, 4096);
        close(fd);
        int child_fd = open("/mnt/state", O_RDONLY | O_CLOEXEC);
        void *child_mapping = child_fd < 0
            ? MAP_FAILED
            : mmap(NULL, 4096, PROT_READ, MAP_SHARED, child_fd, 0);
        if (child_mapping == MAP_FAILED ||
            memcmp((const char *)child_mapping + 128, expected,
                   sizeof(expected) - 1) != 0) {
            _exit(1);
        }
        _exit(0);
    }
    int reader_status = 0;
    if (reader < 0 || waitpid(reader, &reader_status, 0) != reader ||
        !WIFEXITED(reader_status) || WEXITSTATUS(reader_status) != 0) {
        dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_MULTIPROCESS_ERROR errno=%d\n",
                errno);
        for (;;) pause();
    }
    dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_MULTIPROCESS_OK\n");

    dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_DAX_OK\n");
    for (;;) pause();
}
