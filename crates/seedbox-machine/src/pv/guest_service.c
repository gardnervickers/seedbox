#define _POSIX_C_SOURCE 200809L
#include "guest_service.h"
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

void sbx_service_init(struct sbx_service *s, sbx_launch launch, void *recipe,
                      sbx_write write, void *sink) {
    memset(s, 0, sizeof(*s));
    sbx_supervisor_init(&s->supervisor);
    s->launch = launch; s->recipe = recipe; s->write = write; s->sink = sink;
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
static int dispatch(struct sbx_service *s) {
    char *action = s->frame;
    if (!strncmp(action, "SBX_REPLY1 ", 11)) {
        if (!s->oracle) { errno = EINVAL; return -1; }
        return sbx_oracle_reply(s->oracle, action);
    }
    if (!strcmp(action, "shutdown")) { s->shutdown_requested = 1; return 0; }
    uint32_t id = 0;
    char *decimal = strchr(action, ' ');
    if (decimal) {
        *decimal++ = 0;
        if (!*decimal || *decimal == '0') { errno = EINVAL; return -1; }
        for (; *decimal; ++decimal) {
            if (*decimal < '0' || *decimal > '9' ||
                id > (UINT32_MAX - (unsigned)(*decimal - '0')) / 10) {
                errno = EINVAL; return -1;
            }
            id = id*10 + (unsigned)(*decimal - '0');
        }
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
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now)) return -1;
    unsigned long long ns = (unsigned long long)now.tv_sec*1000000000ULL + (unsigned long long)now.tv_nsec;
    char frame[192];
    int size = snprintf(frame, sizeof(frame), "SEEDBOX_%s id=%u action=%s guest_ns=%llu\n",
                        result ? "FAILURE" : "EFFECT", id, action, ns);
    if (size < 0 || (size_t)size >= sizeof(frame)) { errno = EOVERFLOW; return -1; }
    return s->write(s->sink, frame, (size_t)size);
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
