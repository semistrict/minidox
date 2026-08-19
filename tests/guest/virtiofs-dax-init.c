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
static const char cold_fault_command[] = "cold fault";

static int load_module(const char *path, int optional) {
    int module = open(path, O_RDONLY | O_CLOEXEC);
    if (module < 0) {
        return optional && errno == ENOENT ? 0 : -1;
    }
    int result = syscall(SYS_finit_module, module, "", 0);
    int saved_errno = errno;
    close(module);
    errno = saved_errno;
    return result == 0 || errno == EEXIST ? 0 : -1;
}

static int load_modules(void) {
    FILE *modules = fopen("/modules", "r");
    if (modules == NULL) {
        return load_module("/fuse.ko", 1) == 0 &&
               load_module("/virtiofs.ko", 0) == 0 ? 0 : -1;
    }

    char path[128];
    while (fgets(path, sizeof(path), modules) != NULL) {
        path[strcspn(path, "\r\n")] = '\0';
        if (path[0] != '\0' && load_module(path, 0) != 0) {
            int saved_errno = errno;
            fclose(modules);
            errno = saved_errno;
            dprintf(STDOUT_FILENO,
                    "MINIDOX_VIRTIOFS_MODULE_ERROR path=%s errno=%d\n",
                    path, errno);
            return -1;
        }
    }
    fclose(modules);
    return 0;
}

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

    if (load_modules() != 0) {
        dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_MODULE_ERROR errno=%d\n", errno);
        for (;;) pause();
    }

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

    while (memcmp((const char *)ram_page + 64, cold_fault_command,
                  sizeof(cold_fault_command) - 1) != 0) {
        usleep(1000);
    }
    int cold_fd = open("/mnt/cold", O_RDONLY | O_CLOEXEC);
    void *cold_mapping = cold_fd < 0
        ? MAP_FAILED
        : mmap(NULL, 4096, PROT_READ, MAP_SHARED, cold_fd, 0);
    if (cold_mapping == MAP_FAILED || *(volatile unsigned char *)cold_mapping != 0) {
        dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_COLD_FAULT_ERROR errno=%d\n",
                errno);
        for (;;) pause();
    }
    dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_COLD_FAULT_OK\n");
    for (;;) pause();
}
