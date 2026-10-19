/* Public BB-event ordering adapter for alias_build.c. Scenario from the
 * deterministic process-writer review reproducer, /tmp/dr-alias-review-r1-probe.
 * This client never changes app bytes/instructions, never flushes a fragment,
 * and never reads a DR layout. The application process is the publisher.
 */
#include "dr_api.h"
#include <stdint.h>
#include <string.h>
static bool released;
static int controller_pid;
static dr_emit_flags_t block(void *dc,void *tag,instrlist_t *list,bool trace,bool translating) {
    (void)dc;(void)list;(void)trace;
    if(!released && !translating) {
        byte data[88];size_t read=0;uint64_t magic=0;
        if(!dr_safe_read(tag,sizeof data,data,&read)||read!=sizeof data)return DR_EMIT_DEFAULT;
        memcpy(&magic,data+64,sizeof magic);
#if defined(X86_64)
        bool branch=data[0]==0xe9;
#else
        uint32_t instruction;memcpy(&instruction,data,sizeof instruction);
        bool branch=(instruction&0xfc000000u)==0x14000000u;
#endif
        if(branch&&magic==UINT64_C(0xabbaccdd12344321)) {
            int go,ack;memcpy(&go,data+80,sizeof go);memcpy(&ack,data+84,sizeof ack);char c=1;
            released=true;
            DR_ASSERT(dr_write_file(go,&c,1)==1);
            DR_ASSERT(dr_read_file(ack,&c,1)==1&&c==1);
            dr_fprintf(STDERR,"unix-alias-build: process publication acknowledged after decode\n");
        }
    }
    return DR_EMIT_DEFAULT;
}
static void finished(void) {
    /* The forked publisher never executes the RX branch; only its parent
     * participates in the BB-build handshake. */
    if(dr_get_process_id()==controller_pid)DR_ASSERT(released);
}
DR_EXPORT void dr_client_main(client_id_t id,int argc,const char *argv[]) {
    (void)id;(void)argc;(void)argv;controller_pid=dr_get_process_id();dr_register_bb_event(block);dr_register_exit_event(finished);
}
