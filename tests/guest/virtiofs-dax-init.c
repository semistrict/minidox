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

#define COMMAND_OFFSET 64
#define EXPECTED_OFFSET 80
#define RESPONSE_OFFSET 96

static const char initial_contents[] = "before fork";
static const char check_state_command[] = "check state";
static const char cold_fault_command[] = "cold fault";
static const char state_ok_response[] = "state ok";
static const char state_bad_response[] = "state bad";
static const char cold_ok_response[] = "cold ok";
static const char cold_bad_response[] = "cold bad";

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

static void *allocate_control_page(long page_size) {
    void *page = mmap(NULL, (size_t)page_size, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED || mlock(page, (size_t)page_size) != 0) {
        return MAP_FAILED;
    }
    memcpy(page, initial_contents, sizeof(initial_contents) - 1);
    return page;
}

static int page_gpa(void *page, long page_size, uint64_t *gpa) {
    int pagemap = open("/proc/self/pagemap", O_RDONLY | O_CLOEXEC);
    uint64_t entry = 0;
    off_t entry_offset = (off_t)((uintptr_t)page / (uintptr_t)page_size) *
                         (off_t)sizeof(entry);
    if (pagemap < 0 ||
        pread(pagemap, &entry, sizeof(entry), entry_offset) != sizeof(entry) ||
        (entry & (UINT64_C(1) << 63)) == 0 ||
        (entry & ((UINT64_C(1) << 55) - 1)) == 0) {
        if (pagemap >= 0) {
            close(pagemap);
        }
        errno = EFAULT;
        return -1;
    }
    close(pagemap);
    *gpa = (entry & ((UINT64_C(1) << 55) - 1)) * (uint64_t)page_size;
    return 0;
}

static int volatile_equal(const void *address, const char *value, size_t len) {
    const volatile unsigned char *bytes = address;
    for (size_t index = 0; index < len; ++index) {
        if (bytes[index] != (unsigned char)value[index]) {
            return 0;
        }
    }
    return 1;
}

static void publish_response(void *control_page, const char *response) {
    volatile unsigned char *target =
        (volatile unsigned char *)control_page + RESPONSE_OFFSET;
    size_t len = strlen(response);
    for (size_t index = 0; index < len; ++index) {
        target[index] = (unsigned char)response[index];
    }
}

static void command_loop(void *control_page, const void *state_mapping) {
    int cold_fd = -1;
    void *cold_mapping = MAP_FAILED;
    for (;;) {
        const void *command = (const char *)control_page + COMMAND_OFFSET;
        if (volatile_equal(command, check_state_command,
                           sizeof(check_state_command) - 1)) {
            const void *wanted = (const char *)control_page + EXPECTED_OFFSET;
            int matches = 1;
            for (size_t index = 0; index < sizeof(initial_contents) - 1; ++index) {
                if (*((const volatile unsigned char *)state_mapping + 128 + index) !=
                    *((const volatile unsigned char *)wanted + index)) {
                    matches = 0;
                    break;
                }
            }
            publish_response(control_page,
                             matches ? state_ok_response : state_bad_response);
        } else if (volatile_equal(command, cold_fault_command,
                                  sizeof(cold_fault_command) - 1)) {
            if (cold_mapping == MAP_FAILED) {
                cold_fd = open("/mnt/cold", O_RDONLY | O_CLOEXEC);
                cold_mapping = cold_fd < 0
                    ? MAP_FAILED
                    : mmap(NULL, 4096, PROT_READ, MAP_SHARED, cold_fd, 0);
            }
            int matches = cold_mapping != MAP_FAILED &&
                          *(const volatile unsigned char *)cold_mapping == 0;
            publish_response(control_page,
                             matches ? cold_ok_response : cold_bad_response);
        }
        usleep(1000);
    }
}

int main(void) {
    long page_size = sysconf(_SC_PAGESIZE);
    void *parent_control = allocate_control_page(page_size);
    if (parent_control == MAP_FAILED ||
        madvise(parent_control, (size_t)page_size, MADV_DONTFORK) != 0) {
        dprintf(STDOUT_FILENO, "MINIDOX_RAM_ALLOC_ERROR errno=%d\n", errno);
        for (;;) pause();
    }

    mkdir("/proc", 0555);
    if (mount("proc", "/proc", "proc", 0, NULL) != 0 && errno != EBUSY) {
        dprintf(STDOUT_FILENO, "MINIDOX_RAM_PROC_ERROR errno=%d\n", errno);
        for (;;) pause();
    }
    uint64_t parent_gpa = 0;
    if (page_gpa(parent_control, page_size, &parent_gpa) != 0) {
        dprintf(STDOUT_FILENO, "MINIDOX_RAM_PAGEMAP_ERROR errno=%d\n", errno);
        for (;;) pause();
    }
    dprintf(STDOUT_FILENO, "MINIDOX_RAM_GPA=%#llx\n",
            (unsigned long long)parent_gpa);

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
    if (memcmp((const char *)mapping + 128, initial_contents,
               sizeof(initial_contents) - 1) != 0) {
        dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_CONTENT_ERROR\n");
        for (;;) pause();
    }

    int ready[2];
    if (pipe(ready) != 0) {
        dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_MULTIPROCESS_ERROR errno=%d\n",
                errno);
        for (;;) pause();
    }
    pid_t worker = fork();
    if (worker == 0) {
        close(ready[0]);
        munmap(mapping, 4096);
        close(fd);
        int worker_fd = open("/mnt/state", O_RDONLY | O_CLOEXEC);
        void *worker_mapping = worker_fd < 0
            ? MAP_FAILED
            : mmap(NULL, 4096, PROT_READ, MAP_SHARED, worker_fd, 0);
        void *worker_control = allocate_control_page(page_size);
        uint64_t worker_gpa = 0;
        if (worker_mapping == MAP_FAILED || worker_control == MAP_FAILED ||
            memcmp((const char *)worker_mapping + 128, initial_contents,
                   sizeof(initial_contents) - 1) != 0 ||
            page_gpa(worker_control, page_size, &worker_gpa) != 0) {
            dprintf(STDOUT_FILENO,
                    "MINIDOX_VIRTIOFS_WORKER_ERROR errno=%d\n", errno);
            _exit(1);
        }
        dprintf(STDOUT_FILENO, "MINIDOX_WORKER_GPA=%#llx\n",
                (unsigned long long)worker_gpa);
        if (write(ready[1], "R", 1) != 1) {
            dprintf(STDOUT_FILENO,
                    "MINIDOX_VIRTIOFS_WORKER_ERROR errno=%d\n", errno);
            _exit(1);
        }
        close(ready[1]);
        command_loop(worker_control, worker_mapping);
    }

    close(ready[1]);
    char ready_byte = 0;
    if (worker < 0 || read(ready[0], &ready_byte, 1) != 1 || ready_byte != 'R') {
        dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_MULTIPROCESS_ERROR errno=%d\n",
                errno);
        for (;;) pause();
    }
    close(ready[0]);
    dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_MULTIPROCESS_OK\n");
    dprintf(STDOUT_FILENO, "MINIDOX_VIRTIOFS_DAX_OK\n");

    command_loop(parent_control, mapping);
}
