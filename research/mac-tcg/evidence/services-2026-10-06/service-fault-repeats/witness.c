/* Mac-only research plugin: hash executed translation-block entry records.
 * This is a control-flow witness, NOT retired-instruction or full-state proof.
 * A block can fault before all of its translated instructions execute. */
#include <CommonCrypto/CommonDigest.h>
#include <qemu-plugin.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

QEMU_PLUGIN_EXPORT int qemu_plugin_version = QEMU_PLUGIN_VERSION;
struct entry { unsigned char bytes[48]; struct entry *next; };
static struct entry *allocated;
static CC_SHA256_CTX state;
static unsigned char buffer[32736]; /* multiple of the 48-byte record */
static size_t used;
static uint64_t blocks;
static const char *output;

static void little64(unsigned char *p, uint64_t value) {
    for (unsigned i = 0; i < 8; ++i) p[i] = value >> (i * 8);
}

static void execute(unsigned cpu, void *data) {
    (void)cpu;
    struct entry *entry = data;
    memcpy(buffer + used, entry->bytes, sizeof entry->bytes);
    used += sizeof entry->bytes;
    ++blocks;
    if (used == sizeof buffer) {
        CC_SHA256_Update(&state, buffer, sizeof buffer);
        used = 0;
    }
}

static void translate(struct qemu_plugin_tb *tb, void *data) {
    (void)data;
    struct entry *entry = malloc(sizeof *entry);
    if (!entry) abort();
    entry->next = allocated;
    allocated = entry;
    little64(entry->bytes, qemu_plugin_tb_vaddr(tb));
    size_t count = qemu_plugin_tb_n_insns(tb);
    little64(entry->bytes + 8, count);
    CC_SHA256_CTX instructions;
    CC_SHA256_Init(&instructions);
    for (size_t i = 0; i < count; ++i) {
        struct qemu_plugin_insn *insn = qemu_plugin_tb_get_insn(tb, i);
        unsigned char bytes[16];
        size_t size = qemu_plugin_insn_data(insn, bytes, sizeof bytes);
        if (!size || size > 15) abort();
        unsigned char length = size;
        CC_SHA256_Update(&instructions, &length, 1);
        CC_SHA256_Update(&instructions, bytes, (CC_LONG)size);
    }
    CC_SHA256_Final(entry->bytes + 16, &instructions);
    qemu_plugin_register_vcpu_tb_exec_cb(tb, execute, QEMU_PLUGIN_CB_NO_REGS, entry);
}

static void flush(void *data) {
    (void)data;
    while (allocated) {
        struct entry *next = allocated->next;
        free(allocated);
        allocated = next;
    }
}

static void finish(void *data) {
    (void)data;
    if (qemu_plugin_num_vcpus() != 1) return;
    unsigned char hash[CC_SHA256_DIGEST_LENGTH];
    CC_SHA256_Update(&state, buffer, (CC_LONG)used);
    CC_SHA256_Final(hash, &state);
    FILE *file = fopen(output, "w");
    if (!file) return; /* host runner requires this output to accept a run */
    fprintf(file, "{\"schema\":\"seedbox.tcg.tb-entry.v1\",\"blocks\":%llu,\"sha256\":\"", (unsigned long long)blocks);
    for (unsigned i = 0; i < sizeof hash; ++i) fprintf(file, "%02x", hash[i]);
    fprintf(file, "\"}\n");
    fclose(file);
    flush(NULL);
}

QEMU_PLUGIN_EXPORT int qemu_plugin_install(qemu_plugin_id_t id, const qemu_info_t *info, int argc, char **argv) {
    if (!info->system_emulation || info->system.smp_vcpus != 1 ||
        strcmp(info->target_name, "x86_64") || argc != 1 || strncmp(argv[0], "output=", 7)) {
        fprintf(stderr, "witness admission: system=%d target=%s vcpus=%d argc=%d\n",
                info->system_emulation, info->target_name, info->system.smp_vcpus, argc);
        return -1;
    }
    output = argv[0] + 7;
    CC_SHA256_Init(&state);
    qemu_plugin_register_vcpu_tb_trans_cb(id, translate, NULL);
    qemu_plugin_register_flush_cb(id, flush, NULL);
    qemu_plugin_register_atexit_cb(id, finish, NULL);
    return 0;
}
