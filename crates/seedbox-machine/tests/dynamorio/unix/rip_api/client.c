/* X86-only public contract regression authored from source owner's minimal
 * rip_client.c for LOCAL5ce91299fcf77ad6e5864aa0849bff6f8e246990
 * (parent e4bb6a8f5101cdbaf3066a1f2bb1ecd7c1fde992), fixing original-PC vs
 * copied-raw-PC target precedence. Source context: DR instr_get_rel_target,
 * copied selfmod emission, Seedboxf32472b7 cfz.c:t_rwrx. No private DR fields,
 * unsupported decoder entrypoints, code-cache layouts or application patching.
 * Load/store/LEA and signed-negative-displacement controls are authored extensions. */
#include "dr_api.h"
#if !defined(X86_64)
#error This is an x86-64 RIP-relative API contract, not an A64 equivalent.
#endif
static int errors;
#define EXPECT(test) do {if(!(test)){errors++;dr_fprintf(STDERR,"unix-rip-api: assertion line%d\n",__LINE__);}}while(0)
static void semantic_target(instr_t *in,app_pc expected,bool store) {
    int src=instr_get_rel_addr_src_idx(in),dst=instr_get_rel_addr_dst_idx(in);
    EXPECT(store?(dst>=0&&src==-1):(src>=0&&dst==-1));
    if(src>=0||dst>=0) {
        opnd_t operand=src>=0?instr_get_src(in,(uint)src):instr_get_dst(in,(uint)dst);
        EXPECT(opnd_is_rel_addr(operand));EXPECT(opnd_get_addr(operand)==expected);
    }
}
DR_EXPORT void dr_client_main(client_id_t id,int argc,const char *argv[]) {
    (void)id;(void)argc;(void)argv;void *dc=dr_get_current_drcontext();
    for(unsigned c=0;c<4;c++) {
        byte original[128]={0},copy[16]={0},encoded[16]={0},raw_encoded[16]={0};
        unsigned len=c==2?7:6,disp_pos=c==2?3:2,origin=c==3?64:0;
        int displacement=c==3?-62:c==2?1:2;
        copy[0]=c==2?0x48:c==1?0x89:0x8b;
        if(c==2){copy[1]=0x8d;copy[2]=0x05;}else copy[1]=0x05;
        for(unsigned i=0;i<4;i++)copy[disp_pos+i]=(byte)((uint)displacement>>(8*i));
        for(unsigned i=0;i<len;i++)original[origin+i]=copy[i];
        app_pc pc=original+origin,target=original+8,got=NULL;
        instr_t decoded,roundtrip,raw,raw_roundtrip;
        instr_init(dc,&decoded);instr_init(dc,&roundtrip);instr_init(dc,&raw);instr_init(dc,&raw_roundtrip);
        byte *next=decode_from_copy(dc,copy,pc,&decoded);
        EXPECT(next==copy+len);EXPECT(instr_operands_valid(&decoded));
        EXPECT(instr_raw_bits_valid(&decoded)&&instr_get_raw_bits(&decoded)==copy);
        semantic_target(&decoded,target,c==1);
        EXPECT(instr_get_rel_addr_target(&decoded,&got)&&got==target);
        byte *end=instr_encode_to_copy(dc,&decoded,encoded,pc);
        EXPECT(end==encoded+len);
        EXPECT(decode_from_copy(dc,encoded,pc,&roundtrip)==end);
        semantic_target(&roundtrip,target,c==1);
        EXPECT(instr_same(&decoded,&roundtrip));
        /* Public setters recreate an explicitly raw/operands-undecoded object
         * from the same valid bytes at their REAL location. This keeps the raw
         * target path meaningful without private flags or decode_cti APIs. */
        EXPECT(decode(dc,pc,&raw)==pc+len);
        instr_set_operands_valid(&raw,false);instr_set_opcode(&raw,OP_UNDECODED);
        instr_set_raw_bits_valid(&raw,true);
        EXPECT(!instr_operands_valid(&raw)&&instr_raw_bits_valid(&raw));
        got=NULL;EXPECT(instr_get_rel_addr_target(&raw,&got)&&got==target);
        end=instr_encode_to_copy(dc,&raw,raw_encoded,pc);
        EXPECT(end==raw_encoded+len);
        EXPECT(decode_from_copy(dc,raw_encoded,pc,&raw_roundtrip)==end);
        semantic_target(&raw_roundtrip,target,c==1);
        EXPECT(instr_same(&decoded,&raw_roundtrip));
        instr_free(dc,&decoded);instr_free(dc,&roundtrip);instr_free(dc,&raw);instr_free(dc,&raw_roundtrip);
    }
    dr_fprintf(STDERR,"unix-rip-api: cases4 decoded4 raw4 roundtrips8 errors%d\n",errors);
    if(errors)dr_abort_with_code(1);
}
