/* Kernel OOM evidence from the supported Linux /dev/kmsg format.
 * SBX_OOM1 PID ANON_RSS APPLICATION COMMAND_HEX\n
 * APPLICATION is true only for the supervised leader, never inferred from 137.
 * Parsing is pure so host tests can exercise real kernel record fixtures. */
#ifndef SEEDBOX_OOM_REPORT_H
#define SEEDBOX_OOM_REPORT_H
#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>
#include <string.h>

enum {
    /* CONSOLE_EXT_LOG_MAX: the longest record /dev/kmsg returns. */
    OOM_REPORT_RECORD_BYTES = 8192,
    /* TASK_COMM_LEN less its NUL, each byte possibly escaped as \xNN. */
    OOM_REPORT_COMMAND_BYTES = 15 * 4,
    OOM_REPORT_FRAME_BYTES = 256
};

static int oom_report_literal(const char **at, const char *end, const char *literal) {
    size_t size = strlen(literal);
    if ((size_t)(end - *at) < size || memcmp(*at, literal, size)) return 0;
    *at += size;
    return 1;
}

static int oom_report_number(const char **at, const char *end, uint64_t *value) {
    const char *start = *at;
    *value = 0;
    while (*at < end && **at >= '0' && **at <= '9') {
        unsigned digit = (unsigned)(**at - '0');
        if (*value > (UINT64_MAX - digit) / 10) return 0;
        *value = *value * 10 + digit;
        ++*at;
    }
    return *at > start;
}

static size_t oom_report_append(char *frame, size_t used, uint64_t value) {
    char digits[20];
    size_t count = 0;
    do { digits[count++] = (char)('0' + value % 10); value /= 10; } while (value);
    frame[used++] = ' ';
    while (count) frame[used++] = digits[--count];
    return used;
}

/* Frame one /dev/kmsg record, "PRIO,SEQ,USEC,FLAGS[,...];MESSAGE\n[ KEY=VALUE\n]...",
 * if it is the kernel's own kill report. Returns the frame size, or zero for
 * every other record, including text that userspace wrote to /dev/kmsg. */
static size_t oom_report_frame(const char *record, size_t size, pid_t application, char frame[OOM_REPORT_FRAME_BYTES]) {
    const char *at = record, *end = record + size;
    uint64_t priority;
    /* Userspace records carry a nonzero facility; the kernel's are bare levels. */
    if (!oom_report_number(&at, end, &priority) || priority > 7 || at == end || *at != ',') return 0;
    const char *message = memchr(record, ';', size);
    if (!message) return 0;
    at = message + 1;
    const char *line_end = memchr(at, '\n', (size_t)(end - at));
    if (line_end) end = line_end;
    if (!oom_report_literal(&at, end, "Out of memory: Killed process ")
        && !oom_report_literal(&at, end, "Memory cgroup out of memory: Killed process ")) return 0;
    uint64_t pid, sizes[4];
    if (!oom_report_number(&at, end, &pid) || pid > UINT32_MAX || !oom_report_literal(&at, end, " (")) return 0;
    /* The task name may itself contain spaces and parentheses. */
    static const char after[] = ") total-vm:";
    const char *command = at, *command_end = NULL;
    for (const char *p = at; (size_t)(end - p) >= sizeof(after) - 1; ++p) {
        if (!memcmp(p, after, sizeof(after) - 1)) { command_end = p; break; }
    }
    if (!command_end || command_end == command || command_end - command > OOM_REPORT_COMMAND_BYTES) return 0;
    at = command_end + sizeof(after) - 1;
    static const char *const fields[] = {"", "kB, anon-rss:", "kB, file-rss:", "kB, shmem-rss:"};
    for (unsigned i = 0; i < 4; ++i) {
        if (!oom_report_literal(&at, end, fields[i]) || !oom_report_number(&at, end, &sizes[i])) return 0;
    }
    if (!oom_report_literal(&at, end, "kB")) return 0;
    static const char prefix[] = "SBX_OOM1", hex[] = "0123456789abcdef";
    size_t used = sizeof(prefix) - 1;
    memcpy(frame, prefix, used);
    used = oom_report_append(frame, used, pid);
    used = oom_report_append(frame, used, sizes[1]);
    used = oom_report_append(frame, used, application > 0 && pid == (uint64_t)application);
    frame[used++] = ' ';
    for (const char *p = command; p < command_end; ++p) {
        frame[used++] = hex[(unsigned char)*p >> 4];
        frame[used++] = hex[(unsigned char)*p & 15];
    }
    frame[used++] = '\n';
    return used;
}

#endif
