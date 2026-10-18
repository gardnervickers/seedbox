/* Run inside an ordinary Compose guest, including as a numeric non-root user.
 * These checks need real kernel interfaces, not host syscall substitutes. */
#define _GNU_SOURCE
#include <assert.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/filter.h>
#include <linux/io_uring.h>
#include <linux/seccomp.h>
#include <mqueue.h>
#include <netdb.h>
#include <semaphore.h>
#include <sched.h>
#include <sys/wait.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/shm.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <termios.h>
#include <unistd.h>

static void io_uring_nop(void) {
    struct io_uring_params params = {0};
    int fd = syscall(SYS_io_uring_setup, 2, &params); assert(fd >= 0);
    size_t sq_size = params.sq_off.array + params.sq_entries * sizeof(unsigned);
    size_t cq_size = params.cq_off.cqes + params.cq_entries * sizeof(struct io_uring_cqe);
    int shared = params.features & IORING_FEAT_SINGLE_MMAP;
    if (shared && cq_size > sq_size) sq_size = cq_size;
    char *sq = mmap(NULL, sq_size, PROT_READ|PROT_WRITE, MAP_SHARED, fd, IORING_OFF_SQ_RING);
    assert(sq != MAP_FAILED);
    char *cq = shared ? sq : mmap(NULL, cq_size, PROT_READ|PROT_WRITE, MAP_SHARED, fd, IORING_OFF_CQ_RING);
    assert(cq != MAP_FAILED);
    size_t entries_size = params.sq_entries * sizeof(struct io_uring_sqe);
    struct io_uring_sqe *entries = mmap(NULL, entries_size, PROT_READ|PROT_WRITE, MAP_SHARED, fd, IORING_OFF_SQES);
    assert(entries != MAP_FAILED);
    entries[0] = (struct io_uring_sqe){.opcode = IORING_OP_NOP, .user_data = 172};
    *(unsigned *)(sq + params.sq_off.array) = 0;
    __atomic_store_n((unsigned *)(sq + params.sq_off.tail), 1, __ATOMIC_RELEASE);
    assert(syscall(SYS_io_uring_enter, fd, 1, 1, IORING_ENTER_GETEVENTS, NULL, 0) == 1);
    assert(__atomic_load_n((unsigned *)(cq + params.cq_off.tail), __ATOMIC_ACQUIRE) == 1);
    struct io_uring_cqe *result = (void *)(cq + params.cq_off.cqes);
    assert(result->res == 0 && result->user_data == 172);
    assert(!munmap(entries, entries_size));
    if (!shared) assert(!munmap(cq, cq_size));
    assert(!munmap(sq, sq_size) && !close(fd));
}
static void namespace_map(const char *path, const char *value) {
    int fd = open(path, O_WRONLY); assert(fd >= 0);
    assert(write(fd, value, strlen(value)) == (ssize_t)strlen(value));
    assert(!close(fd));
}
static void user_namespace(void) {
    uid_t uid = getuid(); gid_t gid = getgid();
    assert(uid != 0);
    pid_t child = fork(); assert(child >= 0);
    if (!child) {
        assert(!unshare(CLONE_NEWUSER));
        char mapping[64];
        namespace_map("/proc/self/setgroups", "deny\n");
        snprintf(mapping, sizeof(mapping), "0 %u 1\n", uid);
        namespace_map("/proc/self/uid_map", mapping);
        snprintf(mapping, sizeof(mapping), "0 %u 1\n", gid);
        namespace_map("/proc/self/gid_map", mapping);
        assert(getuid() == 0 && getgid() == 0);
        // Chromium's namespace sandbox needs these after entering a user NS.
        assert(!unshare(CLONE_NEWNS|CLONE_NEWPID|CLONE_NEWNET|CLONE_NEWIPC|CLONE_NEWUTS));
        pid_t inner = fork(); assert(inner >= 0);
        if (!inner) { assert(getpid() == 1); _exit(0); }
        int status; assert(waitpid(inner, &status, 0) == inner);
        assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
        _exit(0);
    }
    int status; assert(waitpid(child, &status, 0) == child);
    assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}
/* Progress checkpoints leave through the translator's dispatcher on ARM, which
 * restores every register from the saved context. Run enough blocks for
 * hundreds of checkpoints with all vector registers live. */
static void simd_registers_survive_progress(void) {
#if defined(__aarch64__)
    uint64_t before[64], after[64];
    for (unsigned i = 0; i < 64; ++i)
        before[i] = 0x5eedb0c500000000ull | (i * 0x01010101u);
    uint64_t *in = before, *out = after;
    __asm__ volatile(
        "ld1 {v0.2d-v3.2d}, [%[in]], #64\n"
        "ld1 {v4.2d-v7.2d}, [%[in]], #64\n"
        "ld1 {v8.2d-v11.2d}, [%[in]], #64\n"
        "ld1 {v12.2d-v15.2d}, [%[in]], #64\n"
        "ld1 {v16.2d-v19.2d}, [%[in]], #64\n"
        "ld1 {v20.2d-v23.2d}, [%[in]], #64\n"
        "ld1 {v24.2d-v27.2d}, [%[in]], #64\n"
        "ld1 {v28.2d-v31.2d}, [%[in]], #64\n"
        "mov x9, %[iterations]\n"
        "1: subs x9, x9, #1\n"
        "b 2f\n"
        "2: b 3f\n"
        "3: cbnz x9, 1b\n"
        "st1 {v0.2d-v3.2d}, [%[out]], #64\n"
        "st1 {v4.2d-v7.2d}, [%[out]], #64\n"
        "st1 {v8.2d-v11.2d}, [%[out]], #64\n"
        "st1 {v12.2d-v15.2d}, [%[out]], #64\n"
        "st1 {v16.2d-v19.2d}, [%[out]], #64\n"
        "st1 {v20.2d-v23.2d}, [%[out]], #64\n"
        "st1 {v24.2d-v27.2d}, [%[out]], #64\n"
        "st1 {v28.2d-v31.2d}, [%[out]], #64\n"
        : [in] "+r"(in), [out] "+r"(out)
        : [iterations] "r"(3000000ul)
        : "x9", "cc", "memory", "v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8",
          "v9", "v10", "v11", "v12", "v13", "v14", "v15", "v16", "v17", "v18", "v19",
          "v20", "v21", "v22", "v23", "v24", "v25", "v26", "v27", "v28", "v29", "v30",
          "v31");
    assert(!memcmp(before, after, sizeof(before)));
    puts("vector registers survived progress");
#endif
}
int main(int argc, char **argv) {
    assert(argc == 2);
    struct stat info;
    assert(!stat("/dev/shm", &info) && (info.st_mode & 07777) == 01777);
    const struct { const char *path; unsigned major, minor; } nodes[] = {
        {"/dev/null", 1, 3}, {"/dev/zero", 1, 5}, {"/dev/full", 1, 7},
        {"/dev/random", 1, 8}, {"/dev/urandom", 1, 9}, {"/dev/tty", 5, 0}
    };
    for (size_t i = 0; i < sizeof(nodes)/sizeof(*nodes); i++) {
        assert(!stat(nodes[i].path, &info) && S_ISCHR(info.st_mode));
        assert((info.st_mode & 0777) == 0666);
        assert(major(info.st_rdev) == nodes[i].major && minor(info.st_rdev) == nodes[i].minor);
    }
    DIR *devices = opendir("/dev"); assert(devices);
    struct dirent *entry;
    while ((entry = readdir(devices))) {
        assert(strncmp(entry->d_name, "vd", 2) && strncmp(entry->d_name, "vport", 5));
        assert(strcmp(entry->d_name, "kmsg"));
        assert(strcmp(entry->d_name, "seedbox-io"));
    }
    assert(!closedir(devices));
    assert(!stat("/dev/ptmx", &info) && S_ISCHR(info.st_mode) && minor(info.st_rdev) == 2);
    int pty = posix_openpt(O_RDWR|O_NOCTTY); assert(pty >= 0);
    assert(!grantpt(pty) && !unlockpt(pty));
    int terminal = open(ptsname(pty), O_RDWR|O_NOCTTY); assert(terminal >= 0);
    struct termios settings; assert(!tcgetattr(terminal, &settings));
    cfmakeraw(&settings); assert(!tcsetattr(terminal, TCSANOW, &settings));
    assert(write(pty, "pty", 3) == 3);
    char bytes[256] = {0}; assert(read(terminal, bytes, sizeof(bytes)) == 3 && !strcmp(bytes, "pty"));
    assert(!close(terminal) && !close(pty));
    int fd = open("/dev/stdout", O_WRONLY); assert(fd >= 0);
    const char *message = "runtime stdout through /dev/stdout\n";
    assert(write(fd, message, strlen(message)) == (ssize_t)strlen(message));
    assert(!close(fd));
    int pipe_fds[2]; assert(!pipe(pipe_fds));
    snprintf(bytes, sizeof(bytes), "/dev/fd/%d", pipe_fds[0]);
    fd = open(bytes, O_RDONLY); assert(fd >= 0);
    assert(write(pipe_fds[1], "fd", 2) == 2);
    memset(bytes, 0, sizeof(bytes)); assert(read(fd, bytes, sizeof(bytes)) == 2 && !strcmp(bytes, "fd"));
    assert(!close(fd) && !close(pipe_fds[0]) && !close(pipe_fds[1]));
    sem_t *sem = sem_open("/seedbox-runtime", O_CREAT|O_EXCL, 0600, 0);
    assert(sem != SEM_FAILED && !sem_post(sem) && !sem_wait(sem));
    assert(!sem_close(sem) && !sem_unlink("/seedbox-runtime"));
    fd = shm_open("/seedbox-runtime", O_CREAT|O_EXCL|O_RDWR, 0600); assert(fd >= 0);
    assert(!ftruncate(fd, 4096));
    char *memory = mmap(NULL, 4096, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0); assert(memory != MAP_FAILED);
    strcpy(memory, "shared"); assert(!strcmp(memory, "shared"));
    assert(!munmap(memory, 4096) && !close(fd) && !shm_unlink("/seedbox-runtime"));
    int segment = shmget(IPC_PRIVATE, 4096, IPC_CREAT|0600); assert(segment >= 0);
    memory = shmat(segment, NULL, 0); assert(memory != (void *)-1);
    strcpy(memory, "sysv"); assert(!strcmp(memory, "sysv"));
    assert(!shmdt(memory) && !shmctl(segment, IPC_RMID, NULL));
    struct mq_attr attributes = {.mq_maxmsg = 2, .mq_msgsize = 16};
    mqd_t queue = mq_open("/seedbox-runtime", O_CREAT|O_EXCL|O_RDWR, 0600, &attributes);
    assert(queue != (mqd_t)-1 && !mq_send(queue, "mq", 3, 0));
    assert(mq_receive(queue, bytes, sizeof(bytes), NULL) == 3 && !strcmp(bytes, "mq"));
    assert(!mq_close(queue) && !mq_unlink("/seedbox-runtime"));
    char hostname[65]; assert(!gethostname(hostname, sizeof(hostname)) && !strcmp(hostname, argv[1]));
    FILE *file = fopen("/etc/hostname", "r"); assert(file);
    assert(fgets(bytes, sizeof(bytes), file)); bytes[strcspn(bytes, "\n")] = 0;
    assert(!strcmp(bytes, hostname) && !fclose(file));
    struct addrinfo hints = {.ai_family = AF_INET}, *addresses;
    assert(!getaddrinfo(hostname, NULL, &hints, &addresses)); freeaddrinfo(addresses);
    struct rlimit limit; assert(!getrlimit(RLIMIT_NOFILE, &limit));
    assert(limit.rlim_cur == 1048576 && limit.rlim_max == 1048576);
    /* Check the limit has effect beyond the former 4096 hard ceiling. */
    fd = fcntl(STDOUT_FILENO, F_DUPFD_CLOEXEC, 65535); assert(fd == 65535 && !close(fd));
    file = fopen("/proc/sys/vm/max_map_count", "r"); assert(file);
    unsigned max_map_count; assert(fscanf(file, "%u", &max_map_count) == 1 && max_map_count == 262144);
    assert(!fclose(file));
    fd = open("/data/probe", O_WRONLY|O_CREAT|O_TRUNC, 0600); assert(fd >= 0);
    assert(write(fd, "disk", 4) == 4 && !fsync(fd) && !close(fd));
    user_namespace();
    io_uring_nop();
    simd_registers_survive_progress();
    assert(!prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0));
    struct sock_filter allow = BPF_STMT(BPF_RET|BPF_K, SECCOMP_RET_ALLOW);
    struct sock_fprog filter = {.len = 1, .filter = &allow};
    assert(!syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &filter));
    puts("runtime interfaces passed");
}
