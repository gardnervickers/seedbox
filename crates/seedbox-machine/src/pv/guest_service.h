/* Private cross-architecture guest management adapter. */
#ifndef SBX_GUEST_SERVICE_H
#define SBX_GUEST_SERVICE_H
#include "guest_supervisor.h"
#include "guest_oracle.h"
#include <stdint.h>
typedef int (*sbx_write)(void *sink, const void *bytes, size_t length);
struct sbx_service {
    struct sbx_supervisor supervisor;
    sbx_launch launch;
    void *recipe;
    sbx_write write;
    void *sink;
    char frame[SBX_MANAGEMENT_FRAME_MAX];
    struct sbx_oracle *oracle;
    size_t used;
    int shutdown_requested;
    int poisoned;
    /* Realtime step mechanism; sbx_service_init installs sbx_step_realtime.
     * Host tests replace it because stepping the host clock needs privilege. */
    int (*step_realtime)(int64_t ns);
};
/* adjtimex(ADJ_SETOFFSET): steps CLOCK_REALTIME only. 0, or -1 with errno. */
int sbx_step_realtime(int64_t ns);
void sbx_service_init(struct sbx_service *s, sbx_launch launch, void *recipe,
                      sbx_write write, void *sink);
/* Successful initial exec emits SBX_READY1, which is no health/readiness probe.
 * Initial failure is an error to the owning init, unlike later Start failure. */
int sbx_service_start(struct sbx_service *s);
/* Incremental management input. shutdown\n latches a separate machine-lifecycle
 * request. step ID NS steps realtime by signed nonzero NS nanoseconds and is
 * acknowledged as action=step (SEEDBOX_FAILURE when the kernel refuses it).
 * start/stop/freeze/thaw use optional nonzero decimal management IDs;
 * these IDs are not oracle RPC IDs. SBX_REPLY1 shares this sole frame parser
 * and routes to the configured oracle relay. Malformed input fails closed. */
int sbx_service_feed(struct sbx_service *s, const void *bytes, size_t length);
/* Natural exit emits SBX_EXIT1 status=N after draining logs; caller powers off.
 * Commanded Stop leaves init running and never emits SBX_EXIT1. */
int sbx_service_collect(struct sbx_service *s, int *exit_code);
int sbx_service_output(struct sbx_service *s, unsigned stream);
/* Output sink for supervisor Stop/collect and graceful machine shutdown. */
int sbx_service_log(void *service, unsigned stream,
                    const unsigned char *bytes, size_t length);
#endif
