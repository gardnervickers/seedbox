#include "dr_api.h"
static uint64 executions, translations;
static void counted(void) { executions++; }
static dr_emit_flags_t bb(void *dc, void *tag, instrlist_t *list, bool trace, bool translating) {
 for(instr_t *in=instrlist_first_app(list);in;in=instr_get_next_app(in)) {
  if(instr_get_opcode(in)==OP_ic_ivau) {
   translations++;
   dr_insert_clean_call(dc,list,in,(void*)counted,false,0);
  }
 }
 return DR_EMIT_DEFAULT;
}
static void end(void) { dr_fprintf(STDERR,"IC_IVAU translated=%llu executed=%llu\n",translations,executions); }
DR_EXPORT void dr_client_main(client_id_t id,int argc,const char **argv) {
 dr_register_bb_event(bb); dr_register_exit_event(end);
}
