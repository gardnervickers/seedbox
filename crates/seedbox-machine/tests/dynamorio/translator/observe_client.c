/* Public diagnostic client, derived from the verified-path cost probe.
 * Matches this prepared JDK's exact
 * public module path and filename. Diagnostic clean-call counter, not the
 * source ARM progress/noHWCC/drreg client. */
#include "dr_api.h"
#include "drmgr.h"
#include "drreg.h"

static const char *vm_path;
typedef struct { app_pc lo, hi; } region_t;
static region_t app_regions[64], vm_regions[64];
static uint n_app, n_vm;
static volatile uint64 app_executed, vm_executed;

static bool equal(const char *a, const char *b)
{
    if (a == NULL || b == NULL)
        return false;
    while (*a != '\0' && *a == *b) { ++a; ++b; }
    return *a == *b;
}

static uint copy_regions(const module_data_t *module, region_t *regions)
{
    uint n = 0;
    for (uint i = 0; i < module->num_segments; ++i) {
        if ((module->segments[i].prot & DR_MEMPROT_EXEC) != 0) {
            DR_ASSERT(n < 64);
            regions[n].lo = module->segments[i].start;
            regions[n].hi = module->segments[i].end;
            ++n;
        }
    }
    DR_ASSERT(n > 0);
    return n;
}

static bool contains(const region_t *regions, uint n, app_pc pc)
{
    for (uint i = 0; i < n; ++i)
        if (pc >= regions[i].lo && pc < regions[i].hi)
            return true;
    return false;
}

#ifndef X86_64
static void count(int kind)
{
    if (kind == 1)
        __atomic_add_fetch(&app_executed, 1, __ATOMIC_RELAXED);
    else if (__atomic_add_fetch(&vm_executed, 1, __ATOMIC_RELAXED) == 1)
        dr_fprintf(STDERR, "path-cost: first-executed-vm-clean-call\n");
}

#endif

static dr_emit_flags_t block(void *dc, void *tag, instrlist_t *bb, instr_t *where,
                             bool for_trace, bool translating, void *user_data)
{
    (void)tag; (void)for_trace; (void)translating; (void)user_data;
    if (where != instrlist_first_app(bb)) return DR_EMIT_DEFAULT;
    if (where != NULL) {
        app_pc pc = instr_get_app_pc(where);
        uint n = __atomic_load_n(&n_vm, __ATOMIC_ACQUIRE);
        int kind = contains(app_regions, n_app, pc) ? 1
            : contains(vm_regions, n, pc) ? 2 : 0;
        if (kind != 0) {
#ifdef X86_64
            // Preserve every executed-block observation without a C call per
            // loop iteration. These are ordinary public drreg/IR APIs.
            reg_id_t scratch;
            DR_ASSERT(drreg_reserve_register(dc, bb, where, NULL, &scratch) == DRREG_SUCCESS);
            DR_ASSERT(drreg_reserve_aflags(dc, bb, where) == DRREG_SUCCESS);
            instrlist_meta_preinsert(bb, where, XINST_CREATE_load_int(dc,
                opnd_create_reg(scratch), OPND_CREATE_INTPTR(kind == 1 ? &app_executed : &vm_executed)));
            instr_t *inc = INSTR_CREATE_inc(dc, OPND_CREATE_MEMPTR(scratch, 0));
            instr_set_prefix_flag(inc, PREFIX_LOCK);
            instrlist_meta_preinsert(bb, where, inc);
            DR_ASSERT(drreg_unreserve_aflags(dc, bb, where) == DRREG_SUCCESS);
            DR_ASSERT(drreg_unreserve_register(dc, bb, where, scratch) == DRREG_SUCCESS);
#else
            dr_insert_clean_call(dc, bb, where, (void *)count, false, 1,
                                 OPND_CREATE_INT32(kind));
#endif
        }
    }
    return DR_EMIT_DEFAULT;
}

static void module_loaded(void *dc, const module_data_t *module, bool loaded)
{
    (void)dc;
    if (!equal(module->full_path, vm_path))
        return;
    DR_ASSERT(equal(module->names.file_name, "libjvm.so"));
    DR_ASSERT(__atomic_load_n(&n_vm, __ATOMIC_ACQUIRE) == 0);
    uint n = copy_regions(module, vm_regions);
    const char *preferred = dr_module_preferred_name(module);
    dr_fprintf(STDERR, "path-cost: verified-vm full_path=%s file_name=%s preferred_name=[%s] module_name=[%s] start=" PFX " end=" PFX " loaded=%d exec_regions=%u\n",
               module->full_path, module->names.file_name,
               preferred == NULL ? "<NULL>" : preferred,
               module->names.module_name == NULL ? "<NULL>" : module->names.module_name,
               module->start, module->end, loaded, n);
    for (uint i = 0; i < n; ++i)
        dr_fprintf(STDERR, "path-cost: vm-exec-segment start=" PFX " end=" PFX "\n",
                   vm_regions[i].lo, vm_regions[i].hi);
    __atomic_store_n(&n_vm, n, __ATOMIC_RELEASE);
}

static void finish(void)
{
    uint64 app = __atomic_load_n(&app_executed, __ATOMIC_RELAXED);
    uint64 vm = __atomic_load_n(&vm_executed, __ATOMIC_RELAXED);
    dr_fprintf(STDERR, "path-cost: exit app=" UINT64_FORMAT_STRING
               " vm=" UINT64_FORMAT_STRING "\n", app, vm);
    DR_ASSERT(app > 0);
    if (vm_path) DR_ASSERT(vm > 0);
    DR_ASSERT(drreg_exit() == DRREG_SUCCESS);
    drmgr_exit();
}

DR_EXPORT void dr_client_main(client_id_t id, int argc, const char *argv[])
{
    (void)id;
    DR_ASSERT(argc == 1 || argc == 2);
    vm_path = argc == 2 ? argv[1] : NULL;
    drreg_options_t options = { sizeof(options), 3, true };
    DR_ASSERT(drmgr_init());
    DR_ASSERT(drreg_init(&options) == DRREG_SUCCESS);
    module_data_t *module = dr_get_main_module();
    n_app = copy_regions(module, app_regions);
    dr_fprintf(STDERR, "path-cost: main full_path=%s start=" PFX " end=" PFX " exec_regions=%u\n",
               module->full_path, module->start, module->end, n_app);
    dr_free_module_data(module);
    DR_ASSERT(drmgr_register_module_load_event(module_loaded));
    DR_ASSERT(drmgr_register_bb_instrumentation_event(NULL, block, NULL));
    dr_register_exit_event(finish);
}
