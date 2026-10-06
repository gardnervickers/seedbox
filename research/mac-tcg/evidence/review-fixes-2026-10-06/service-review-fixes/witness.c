/* Mac-only research plugin: hash selected guest boundary instructions and
 * architectural discontinuities. Dispatch is observed before completion.
 * This is a scoped execution witness, not a full instruction/state digest. */
#include <CommonCrypto/CommonDigest.h>
#include <qemu-plugin.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "witness-decode.h"

QEMU_PLUGIN_EXPORT int qemu_plugin_version = QEMU_PLUGIN_VERSION;
struct entry { unsigned char bytes[48]; struct entry *next; int port_out; };
static struct entry *allocated;
static CC_SHA256_CTX state;
static unsigned char buffer[32736]; /* multiple of the 48-byte record */
static size_t used;
static uint64_t events, instruction_events, discontinuities;
static uint64_t post_terminal_events;
static int terminal, terminal_code;
static struct qemu_plugin_register *reg_dx, *reg_ax;
static GByteArray *register_value;
static const char *output;

static void little64(unsigned char *p, uint64_t value) {
    for (unsigned i = 0; i < 8; ++i) p[i] = value >> (i * 8);
}

static void execute(unsigned cpu, void *data) {
    (void)cpu;
    struct entry *entry = data;
    memcpy(buffer + used, entry->bytes, sizeof entry->bytes);
    used += sizeof entry->bytes;
    ++events;
    if (used == sizeof buffer) {
        CC_SHA256_Update(&state, buffer, sizeof buffer);
        used = 0;
    }
}

static uint64_t read_reg(struct qemu_plugin_register *reg) {
    g_byte_array_set_size(register_value, 0);
    if (!qemu_plugin_read_register(reg, register_value) || register_value->len < 2) {
        fprintf(stderr, "witness: register read failed\n"); abort();
    }
    uint64_t value = 0;
    for (unsigned i = 0; i < register_value->len && i < 8; ++i) value |= (uint64_t)register_value->data[i] << (8 * i);
    return value;
}

static void init_cpu(unsigned cpu, void *data) {
    (void)data;
    if (cpu != 0) abort();
    GArray *regs = qemu_plugin_get_registers();
    int found_dx = 0, found_ax = 0;
    for (unsigned i = 0; i < regs->len; ++i) {
        qemu_plugin_reg_descriptor r = g_array_index(regs, qemu_plugin_reg_descriptor, i);
        if (!strcmp(r.name, "rdx") || !strcmp(r.name, "edx")) { reg_dx = r.handle; found_dx = 1; }
        if (!strcmp(r.name, "rax") || !strcmp(r.name, "eax")) { reg_ax = r.handle; found_ax = 1; }
    }
    /* Register zero can be represented by a null opaque handle. */
    if (!found_dx || !found_ax) {
        fprintf(stderr, "witness: missing AX/DX registers; available:");
        for (unsigned i = 0; i < regs->len; ++i) fprintf(stderr, " %s", g_array_index(regs, qemu_plugin_reg_descriptor, i).name);
        fprintf(stderr, "\n"); abort();
    }
    g_array_free(regs, true);
    register_value = g_byte_array_new();
}

static int selected(const unsigned char *bytes, size_t size) {
    size_t p = opcode_offset(bytes, size);
    if (p == size) return 0;
    unsigned b = bytes[p++];
    if (b == 0xcc || b == 0xcd || b == 0xce || b == 0xcf || b == 0xf4 ||
        (b >= 0xe4 && b <= 0xe7) || (b >= 0xec && b <= 0xef) || (b >= 0x6c && b <= 0x6f)) return 1;
    if (b != 0x0f || p == size) return 0;
    b = bytes[p++];
    return b == 0x05 || b == 0x07 || b == 0x31 || b == 0x34 || b == 0x35 ||
           (b == 0x01 && p < size && bytes[p] == 0xf9);
}

static void instruction(unsigned cpu, void *data) {
    if (terminal) { ++post_terminal_events; return; }
    ++instruction_events;
    execute(cpu, data);
    struct entry *entry = data;
    if (entry->port_out && (entry->port_out == 2 || (read_reg(reg_dx) & 0xffff) == 0xf4)) {
        terminal_code = read_reg(reg_ax) & 0xff;
        terminal = 1;
    }
}

static void discontinuity(unsigned cpu, enum qemu_plugin_discon_type type,
                          uint64_t from, uint64_t to, void *data) {
    (void)data;
    if (terminal) { ++post_terminal_events; return; }
    struct entry entry = {{0}, NULL, 0};
    little64(entry.bytes, 0x100 + type);
    little64(entry.bytes + 8, from);
    little64(entry.bytes + 16, to);
    ++discontinuities;
    execute(cpu, &entry);
}

static void translate(struct qemu_plugin_tb *tb, void *data) {
    (void)data;
    for (size_t i = 0; i < qemu_plugin_tb_n_insns(tb); ++i) {
        struct qemu_plugin_insn *insn = qemu_plugin_tb_get_insn(tb, i);
        unsigned char bytes[16];
        size_t size = qemu_plugin_insn_data(insn, bytes, sizeof bytes);
        if (!size || size > 15) abort();
        if (!selected(bytes, size)) continue;
        struct entry *entry = calloc(1, sizeof *entry);
        if (!entry) abort();
        entry->next = allocated;
        allocated = entry;
        little64(entry->bytes + 8, qemu_plugin_insn_vaddr(insn));
        CC_SHA256(bytes, (CC_LONG)size, entry->bytes + 16);
        entry->port_out = terminal_out(bytes, size);
        qemu_plugin_register_vcpu_insn_exec_cb(insn, instruction,
                                             entry->port_out ? QEMU_PLUGIN_CB_R_REGS : QEMU_PLUGIN_CB_NO_REGS, entry);
    }
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
    fprintf(file, "{\"schema\":\"seedbox.tcg.boundaries.v2\",\"events\":%llu,\"instruction_events\":%llu,\"discontinuities\":%llu,\"terminal\":%s,\"terminal_code\":%d,\"post_terminal_events\":%llu,\"sha256\":\"",
            (unsigned long long)events, (unsigned long long)instruction_events, (unsigned long long)discontinuities,
            terminal ? "true" : "false", terminal_code, (unsigned long long)post_terminal_events);
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
    qemu_plugin_register_vcpu_init_cb(id, init_cpu, NULL);
    qemu_plugin_register_vcpu_tb_trans_cb(id, translate, NULL);
    qemu_plugin_register_vcpu_discon_cb(id, QEMU_PLUGIN_DISCON_ALL, discontinuity, NULL);
    qemu_plugin_register_flush_cb(id, flush, NULL);
    qemu_plugin_register_atexit_cb(id, finish, NULL);
    return 0;
}
