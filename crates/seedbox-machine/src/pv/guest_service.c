#define _GNU_SOURCE
#include "guest_service.h"
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#ifdef __linux__
#include <sys/timex.h>
#endif

/* Step CLOCK_REALTIME by a signed offset in one kernel operation. The
 * timekeeper moves only the wall-to-monotonic offset, so monotonic, boot and
 * raw clocks and the machine counter are unaffected; the kernel re-evaluates
 * absolute realtime timers and cancels cancel-on-set timerfds. */
int sbx_step_realtime(int64_t ns) {
#ifdef __linux__
    struct timex adjustment = {.modes = ADJ_SETOFFSET | ADJ_NANO};
    adjustment.time.tv_sec = ns / 1000000000;
    adjustment.time.tv_usec = ns % 1000000000;
    if (adjustment.time.tv_usec < 0) { adjustment.time.tv_usec += 1000000000; adjustment.time.tv_sec--; }
    return adjtimex(&adjustment) < 0 ? -1 : 0;
#else
    (void)ns; errno = ENOSYS; return -1;
#endif
}

void sbx_service_init(struct sbx_service *s, sbx_launch launch, void *recipe,
                      sbx_write write, void *sink) {
    memset(s, 0, sizeof(*s));
    sbx_supervisor_init(&s->supervisor);
    s->launch = launch; s->recipe = recipe; s->write = write; s->sink = sink;
    s->step_realtime = sbx_step_realtime;
}
int sbx_service_start(struct sbx_service *s) {
    if (s->poisoned) { errno = EPIPE; return -1; }
    if (sbx_supervisor_start(&s->supervisor, s->launch, s->recipe)) { s->poisoned = 1; return -1; }
    int result = s->write(s->sink, "SBX_READY1\n", 11);
    if (result) s->poisoned = 1;
    return result;
}
int sbx_service_log(void *opaque, unsigned stream,
                    const unsigned char *bytes, size_t length) {
    struct sbx_service *s = opaque;
    if (stream > 1 || length > 2048) { errno = EINVAL; return -1; }
    char frame[4110];
    memcpy(frame, "SBX_LOG1 ", 9);
    frame[9] = stream ? '2' : '1'; frame[10] = ' ';
    static const char hex[] = "0123456789abcdef";
    for (size_t i = 0; i < length; ++i) {
        frame[11 + 2*i] = hex[bytes[i] >> 4];
        frame[12 + 2*i] = hex[bytes[i] & 15];
    }
    frame[11 + 2*length] = '\n';
    int result = s->write(s->sink, frame, 12 + 2*length);
    if (result) s->poisoned = 1;
    return result;
}
/* Nonzero canonical decimal ending at a space or NUL; advances *text. */
static int decimal(char **text, uint64_t limit, uint64_t *out) {
    char *digit = *text;
    uint64_t value = 0;
    if (*digit < '1' || *digit > '9') { errno = EINVAL; return -1; }
    for (; *digit && *digit != ' '; ++digit) {
        if (*digit < '0' || *digit > '9' ||
            value > (limit - (uint64_t)(*digit - '0')) / 10) {
            errno = EINVAL; return -1;
        }
        value = value*10 + (uint64_t)(*digit - '0');
    }
    *text = digit; *out = value; return 0;
}
static int acknowledge(struct sbx_service *s, uint32_t id, const char *action, int result) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now)) return -1;
    unsigned long long ns = (unsigned long long)now.tv_sec*1000000000ULL + (unsigned long long)now.tv_nsec;
    char frame[192];
    int size = snprintf(frame, sizeof(frame), "SEEDBOX_%s id=%u action=%s guest_ns=%llu\n",
                        result ? "FAILURE" : "EFFECT", id, action, ns);
    if (size < 0 || (size_t)size >= sizeof(frame)) { errno = EOVERFLOW; return -1; }
    return s->write(s->sink, frame, (size_t)size);
}
/* step ID NS: ID is a nonzero management ID, NS a nonzero signed offset. A
 * refused step (for example before the epoch) is acknowledged as a failure. */
static int step(struct sbx_service *s, char *text) {
    uint64_t id, magnitude;
    if (decimal(&text, UINT32_MAX, &id) || *text++ != ' ') { errno = EINVAL; return -1; }
    int negative = *text == '-';
    if (negative) ++text;
    if (decimal(&text, negative ? (uint64_t)INT64_MAX + 1 : (uint64_t)INT64_MAX, &magnitude) || *text) {
        errno = EINVAL; return -1;
    }
    int64_t ns = negative ? (int64_t)(0 - magnitude) : (int64_t)magnitude;
    int result = s->step_realtime(ns);
    return acknowledge(s, (uint32_t)id, "step", result);
}
static int dispatch(struct sbx_service *s) {
    char *action = s->frame;
    if (!strncmp(action, "SBX_REPLY1 ", 11)) {
        if (!s->oracle) { errno = EINVAL; return -1; }
        return sbx_oracle_reply(s->oracle, action);
    }
    if (!strcmp(action, "shutdown")) { s->shutdown_requested = 1; return 0; }
    if (!strncmp(action, "step ", 5)) return step(s, action + 5);
    uint64_t id = 0;
    char *decimal_text = strchr(action, ' ');
    if (decimal_text) {
        *decimal_text++ = 0;
        if (decimal(&decimal_text, UINT32_MAX, &id) || *decimal_text) { errno = EINVAL; return -1; }
    }
    int result;
    if (!strcmp(action, "start")) result = sbx_supervisor_start(&s->supervisor, s->launch, s->recipe);
    else if (!strcmp(action, "stop")) result = sbx_supervisor_stop(&s->supervisor, sbx_service_log, s);
    else if (!strcmp(action, "freeze")) result = sbx_supervisor_pause(&s->supervisor);
    else if (!strcmp(action, "thaw")) result = sbx_supervisor_resume(&s->supervisor);
    else { errno = EINVAL; return -1; }
    if (s->poisoned || s->supervisor.failure == SBX_FAILURE_FATAL) {
        s->poisoned = 1; return -1;
    }
    if (!id && result) { errno = EINVAL; return -1; }
    return acknowledge(s, (uint32_t)id, action, result);
}
int sbx_service_feed(struct sbx_service *s, const void *opaque, size_t length) {
    if (s->poisoned) { errno = EPIPE; return -1; }
    const unsigned char *bytes = opaque;
    for (size_t i = 0; i < length; ++i) {
        if (!bytes[i]) { s->poisoned = 1; errno = EINVAL; return -1; }
        if (bytes[i] == '\n') {
            s->frame[s->used] = 0;
            s->used = 0;
            if (dispatch(s)) { s->poisoned = 1; return -1; }
        } else {
            if (s->used + 1 >= sizeof(s->frame)) { s->poisoned = 1; errno = E2BIG; return -1; }
            s->frame[s->used++] = (char)bytes[i];
        }
    }
    return 0;
}
int sbx_service_collect(struct sbx_service *s, int *exit_code) {
    if (s->poisoned) { errno = EPIPE; return -1; }
    int result = sbx_supervisor_collect(&s->supervisor, exit_code, sbx_service_log, s);
    if (result < 0) s->poisoned = 1;
    if (result <= 0) return result;
    char frame[64];
    int size = snprintf(frame, sizeof(frame), "SBX_EXIT1 status=%d\n", *exit_code);
    if (size < 0 || (size_t)size >= sizeof(frame)) { errno = EOVERFLOW; return -1; }
    if (s->write(s->sink, frame, (size_t)size)) { s->poisoned = 1; return -1; }
    return 1;
}
int sbx_service_output(struct sbx_service *s, unsigned stream) {
    if (s->poisoned) { errno = EPIPE; return -1; }
    int result = sbx_supervisor_output(&s->supervisor, stream, sbx_service_log, s);
    if (result) s->poisoned = 1;
    return result;
}
