/* **********************************************************
 * Copyright (c) 2026 Google, Inc.  All rights reserved.
 * **********************************************************/

/*
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 *
 * * Redistributions of source code must retain the above copyright notice,
 *   this list of conditions and the following disclaimer.
 *
 * * Redistributions in binary form must reproduce the above copyright notice,
 *   this list of conditions and the following disclaimer in the documentation
 *   and/or other materials provided with the distribution.
 *
 * * Neither the name of Google, Inc. nor the names of its contributors may be
 *   used to endorse or promote products derived from this software without
 *   specific prior written permission.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
 * AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED. IN NO EVENT SHALL GOOGLE, INC. OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 * SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 * CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
 * LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
 * OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH
 * DAMAGE.
 */

/* Signal ABI and DR-internal-path properties of AArch64 execution under
 * (static) DynamoRIO.  Each case is a hand-written scenario template (below,
 * in assembly) with seeded parameters: initial GPRs/V registers/NZCV/DIT,
 * the signal, its source, its sigaction flags and masks, and the effects the
 * handler applies to the interrupted context.  Every case runs natively (the
 * paired oracle) and then under DR, and each run is also checked against an
 * independent model of the scenario:
 *
 *   - every handler invocation sees a permitted application context: a PC the
 *     scenario allows at that point (legal deferral of asynchronous delivery
 *     is allowed: any PC of the spin loop the scenario waits in) with exactly
 *     the modelled GPRs, SP, NZCV, DIT, V registers and FPCR, and a frame
 *     whose record list is valid by the kernel ABI (a64p_check_frame_records)
 *     and identical to the native one;
 *   - handler-written GPR, NZCV, DIT, V register, PC (a labelled continuation)
 *     and SP (moved within the live stack) take effect after sigreturn, and
 *     state the handler does not write is preserved (notably DIT);
 *   - nesting (SA_NODEFER, or a second signal raised from the handler) is
 *     delivered inside the outer handler and the inner handler's edit of the
 *     outer handler's context takes effect; sa_mask blocks a signal raised in
 *     the handler until sigreturn, which then delivers it with the restored
 *     mask; sigprocmask inside the block keeps a signal pending until it is
 *     unblocked; sigaltstack enabled and disabled from the block moves the
 *     frame onto and off the alternate stack and reports it in uc_stack and
 *     sigaltstack(); real-time payloads arrive exactly once, lowest signal
 *     first and FIFO per signal; a blocking pipe read interrupted with
 *     SA_RESTART is restarted (frame PC at the svc, x0 the fd) and without it
 *     returns EINTR (frame PC after the svc, x0 -EINTR);
 *   - synchronous signals (UDF, BRK, a load from a guard page) are taken at
 *     the faulting instruction with the native si_code, si_addr, fault_address
 *     and ESR record;
 *   - live stack above the interrupted SP (canaries) is never written;
 *   - nothing hangs (every wait is bounded; a child is killed and reported
 *     after a timeout).
 *
 * Modes:
 *   abi: the scenarios natively and under DR (default, -max_bb_instrs 3 and
 *     -disable_traces, plus -steal_reg 27 and resets at every pending signal
 *     against a debug DR).
 *   paths: the same scenarios plus an IBL-churning spin loop under an
 *     in-process client that adds drreg reservations with clobbered values,
 *     aflags clobbers, drx counters and clean calls (all of them with
 *     A64_DR_PROP_CLEANCALL=1, which one variant always sets) to every
 *     scenario instruction; then "storm": threads exiting while signals are in
 *     flight.  Four threads (the main one included) wait in read() or an
 *     unbounded ppoll() of their own empty pipe, each interrupted by its own
 *     SIGEV_THREAD_ID timer and by tgkills at random moments, while a
 *     SIGEV_THREAD timer spawns notification threads that exit, other threads
 *     with the signals unblocked are created and exit (at most -churn_max, 8,
 *     at a time), a process-directed POSIX timer runs, and another thread sends
 *     sequence-numbered process-directed rt signals one at a time: each must be
 *     delivered exactly once, every expiration of the process timer must be
 *     accounted (deliveries plus overruns, counted exactly at a cut), and no
 *     waiter may stop being interrupted for 3 s (the shape of the hang fixed by
 *     "Deliver a signal that arrives on the way into a syscall before it").
 *   storm: just the storm (-waiters N, -poker 0|1 and -churn_max N vary it).
 *
 * Usage: a64_prop_signal <abi|paths|storm> [-seed S] [-n N] [-only I]
 *        [-rounds R] [-variants a,b] [-waiters N] [-poker 0|1] [-churn_max N] [-v]
 * -n is the number of cases per scenario kind; -only runs one case index.
 *
 * Known deviation (counted as suppressed): rt-overflow-order, see
 * DR_MAX_PENDING_SIGNALS below.  Known issue: memcache-check-race, see
 * STORM_DEBUG_DR_OPS below.
 *
 * Counts are printed as generated / accepted / executed / checked /
 * suppressed / inconclusive, with floors; "all properties hold" is only
 * printed if every check and floor passed.
 */

/* Linux signal ABI validation extracted from a64_prop_support.c/h at
 * 356c1821045a4b5678d0813e132074324623df6e. Optional first-UDF snapshots
 * observe accepted fields without changing validation. No DR-internal layout. */
struct a64p_rec_head {
    uint32_t magic, size;
};
#define FPSIMD_MAGIC_ 0x46508001
#define ESR_MAGIC_ 0x45535201
#define EXTRA_MAGIC_ 0x45585401
struct a64p_fpsimd_rec {
    struct a64p_rec_head h;
    uint32_t fpsr, fpcr;
    __uint128_t v[32];
};
struct a64p_esr_rec {
    struct a64p_rec_head h;
    uint64_t esr;
};
struct a64p_extra_rec {
    struct a64p_rec_head h;
    uint64_t datap;
    uint32_t size;
};

struct a64p_rec_head *
a64p_find_rec(ucontext_t *uc, uint32_t magic)
{
    uint8_t *p = (uint8_t *)uc->uc_mcontext.__reserved;
    uint8_t *end = p + sizeof(uc->uc_mcontext.__reserved);
    for (int n = 0; n < 64 && p + sizeof(struct a64p_rec_head) <= end; n++) {
        struct a64p_rec_head *h = (struct a64p_rec_head *)p;
        if (h->magic == 0)
            break;
        if (h->magic == magic)
            return h;
        if (h->magic == EXTRA_MAGIC_) {
            struct a64p_extra_rec *e = (struct a64p_extra_rec *)h;
            p = (uint8_t *)(uintptr_t)e->datap;
            end = p + e->size;
            continue;
        }
        if (h->size == 0)
            break;
        p += h->size;
    }
    return NULL;
}

#define SVE_MAGIC_ 0x53564501
#define ZA_MAGIC_ 0x54366345
#define ZT_MAGIC_ 0x5a544e01
#define TPIDR2_MAGIC_ 0x54504902
#define FPMR_MAGIC_ 0x46504d52
#define POE_MAGIC_ 0x504f4530
#define GCS_MAGIC_ 0x47435300

/* First-UDF diagnostics only: no addresses/padding, at most 12 record headers
 * (10 distinct kinds plus two terminators) and the parser's maximum VL=256.
 * Full SVE copies at most 8736 named register bytes, plus 512 FPSIMD bytes.
 * Other optional records have metadata only. Header-only SVE has no payload.
 * Caller supplies a zero-initialized snapshot once per process. Handler-side
 * capture uses scalar volatile byte copies; formatting happens after return.
 */
#define A64P_SNAPSHOT_RECORDS 12
#define A64P_SNAPSHOT_VL 256
struct a64p_frame_snapshot {
    volatile sig_atomic_t attempted; /* reentry guard, set before walking */
    int valid, metadata_truncated;
    unsigned nrecords;
    struct {
        uint32_t magic, size;
        size_t offset;
        int extra;
    } records[A64P_SNAPSHOT_RECORDS];
    uint32_t extra_size;
    int extra_present, extra_after_reserved;
    size_t extra_distance;
    int fpsimd_present, sve_present, sve_payload_present;
    uint32_t fpcr, fpsr;
    uint16_t sve_vl, sve_flags;
    uint8_t v[32 * 16];
    uint8_t z[32 * A64P_SNAPSHOT_VL];
    uint8_t p[16 * (A64P_SNAPSHOT_VL / 8)];
    uint8_t ffr[A64P_SNAPSHOT_VL / 8];
};

static void
a64p_snapshot_header(struct a64p_frame_snapshot *s, uint32_t magic, uint32_t size,
                     size_t offset, int extra)
{
    if (s == NULL)
        return;
    if (s->nrecords == A64P_SNAPSHOT_RECORDS) {
        s->metadata_truncated = 1;
        return;
    }
    unsigned i = s->nrecords++;
    s->records[i].magic = magic;
    s->records[i].size = size;
    s->records[i].offset = offset;
    s->records[i].extra = extra;
}

static void
a64p_snapshot_bytes(uint8_t *dst, const uint8_t *src, size_t size)
{
    /* Do not add libc calls or SIMD activity to the interrupted thread. */
    volatile uint8_t *d = dst;
    const volatile uint8_t *p = src;
    for (size_t i = 0; i < size; i++)
        d[i] = p[i];
}

static void
a64p_snapshot_payload(struct a64p_frame_snapshot *s, const uint8_t *record,
                      uint32_t magic, uint32_t size)
{
    if (s == NULL)
        return;
    /* Called only after the owning parser accepts bounds, kind, extent and
     * uniqueness. Copy named fields, never record padding or pointers. */
    if (magic == FPSIMD_MAGIC_) {
        const struct a64p_fpsimd_rec *fp = (const void *)record;
        a64p_snapshot_bytes((uint8_t *)&s->fpcr,
                             (const uint8_t *)&fp->fpcr, sizeof(s->fpcr));
        a64p_snapshot_bytes((uint8_t *)&s->fpsr,
                             (const uint8_t *)&fp->fpsr, sizeof(s->fpsr));
        a64p_snapshot_bytes(s->v, (const uint8_t *)fp->v, sizeof(s->v));
        s->fpsimd_present = 1;
    } else if (magic == SVE_MAGIC_) {
        s->sve_present = 1;
        s->sve_vl = *(const uint16_t *)(record + 8);
        s->sve_flags = *(const uint16_t *)(record + 10);
        size_t vl = s->sve_vl, pl = vl / 8;
        /* The existing parser permits VL=0. Do not invent payload for it.
         * Header-only records carry no scalable register data. */
        if (size == 16 || vl < 16 || vl > A64P_SNAPSHOT_VL)
            return;
        size_t full = 16 + 32 * vl + 17 * pl;
        if (size < full)
            return;
        a64p_snapshot_bytes(s->z, record + 16, 32 * vl);
        a64p_snapshot_bytes(s->p, record + 16 + 32 * vl, 16 * pl);
        a64p_snapshot_bytes(s->ffr, record + 16 + 32 * vl + 16 * pl, pl);
        s->sve_payload_present = 1;
    }
}

/* Walks one record list of limit bytes at base; extra (if not NULL) receives
 * an EXTRA record.  Returns 0 or writes an error.
 */
static int
check_record_list(const uint8_t *base, size_t limit, int top, uint64_t *layout,
                  uint32_t *seen, int *sve_vl, int *za_vl,
                  const struct a64p_extra_rec **extra, char *err, size_t errlen,
                  struct a64p_frame_snapshot *snapshot)
{
    size_t off = 0;
    for (int n = 0;; n++) {
        if (off + sizeof(struct a64p_rec_head) > limit) {
            snprintf(err, errlen, "%s list not terminated within %zu bytes",
                     top ? "__reserved" : "extra", limit);
            return 1;
        }
        const struct a64p_rec_head *h = (const struct a64p_rec_head *)(base + off);
        uint32_t magic = h->magic, size = h->size;
        a64p_snapshot_header(snapshot, magic, size, off, !top);
        *layout = (*layout ^ magic) * 0x100000001b3ULL;
        *layout = (*layout ^ size) * 0x100000001b3ULL;
        if (magic == 0) {
            if (size != 0) {
                snprintf(err, errlen, "terminator at +%zu has size %u", off, size);
                return 1;
            }
            return 0;
        }
        if (top && n == 0 && magic != FPSIMD_MAGIC_) {
            snprintf(err, errlen, "first record is 0x%x, not FPSIMD", magic);
            return 1;
        }
        if (size < 16 || size % 16 != 0 || size > limit - off) {
            snprintf(err, errlen, "record 0x%x at +%zu has bad size %u (limit %zu)",
                     magic, off, size, limit);
            return 1;
        }
        int bit = -1;
        uint32_t need = 0, exact = 1;
        switch (magic) {
        case FPSIMD_MAGIC_:
            bit = 0;
            need = sizeof(struct a64p_fpsimd_rec);
            break;
        case ESR_MAGIC_:
            bit = 1;
            need = 16;
            break;
        case EXTRA_MAGIC_:
            bit = 2;
            need = 32;
            if (!top) {
                snprintf(err, errlen, "EXTRA record inside the extra space");
                return 1;
            }
            *extra = (const struct a64p_extra_rec *)h;
            break;
        case SVE_MAGIC_: {
            bit = 3;
            uint16_t vl = *(const uint16_t *)(base + off + 8);
            uint16_t flags = *(const uint16_t *)(base + off + 10);
            *sve_vl = vl;
            *layout = (*layout ^ ((uint64_t)vl << 16 | flags)) * 0x100000001b3ULL;
            if (vl % 16 != 0 || vl > 256 || (flags & ~1u) != 0) {
                snprintf(err, errlen, "SVE record vl %u flags 0x%x", vl, flags);
                return 1;
            }
            uint32_t vq = vl / 16;
            /* Header only, or the full register data for vl. */
            uint32_t full = 16 + vq * 16 * 32 + vq * 2 * 16 + vq * 2;
            if (size != 16 && size < full) {
                snprintf(err, errlen,
                         "SVE record size %u: neither 16 nor >= %u for vl %u", size, full,
                         vl);
                return 1;
            }
            exact = 0;
            break;
        }
        case ZA_MAGIC_: {
            bit = 4;
            uint16_t vl = *(const uint16_t *)(base + off + 8);
            *za_vl = vl;
            *layout = (*layout ^ vl) * 0x100000001b3ULL;
            uint32_t vq = vl / 16;
            if (vl % 16 != 0 || vl > 256 || (size != 16 && size < 16 + vl * vl)) {
                snprintf(err, errlen, "ZA record vl %u size %u (vq %u)", vl, size, vq);
                return 1;
            }
            exact = 0;
            break;
        }
        case ZT_MAGIC_: {
            bit = 5;
            uint16_t nregs = *(const uint16_t *)(base + off + 8);
            if (nregs == 0 || size != 16 + 64u * nregs) {
                snprintf(err, errlen, "ZT record nregs %u size %u", nregs, size);
                return 1;
            }
            exact = 0;
            break;
        }
        case TPIDR2_MAGIC_:
            bit = 6;
            need = 16;
            break;
        case FPMR_MAGIC_:
            bit = 7;
            need = 16;
            break;
        case POE_MAGIC_:
            bit = 8;
            need = 16;
            break;
        case GCS_MAGIC_:
            bit = 9;
            need = 32;
            break;
        default:
            snprintf(err, errlen, "unknown record magic 0x%x size %u at +%zu", magic,
                     size, off);
            return 1;
        }
        if (exact && size != need) {
            snprintf(err, errlen, "record 0x%x size %u, expected %u", magic, size, need);
            return 1;
        }
        if (*seen & (1u << bit)) {
            snprintf(err, errlen, "duplicate record 0x%x", magic);
            return 1;
        }
        a64p_snapshot_payload(snapshot, base + off, magic, size);
        *seen |= 1u << bit;
        off += size;
    }
}

int
a64p_check_frame_records_snapshot(const ucontext_t *uc, uint64_t *layout, int *sve_vl,
                                  int *za_vl, char *err, size_t errlen,
                                  struct a64p_frame_snapshot *snapshot)
{
    if (snapshot != NULL)
        snapshot->attempted = 1;
    const uint8_t *base = (const uint8_t *)uc->uc_mcontext.__reserved;
    uint32_t seen = 0;
    const struct a64p_extra_rec *extra = NULL;
    *layout = 0xcbf29ce484222325ULL;
    *sve_vl = *za_vl = -1;
    err[0] = '\0';
    if (((uintptr_t)uc & 15) != 0 || ((uintptr_t)base & 15) != 0) {
        snprintf(err, errlen, "ucontext %p / __reserved %p not 16-byte aligned", uc,
                 base);
        return 1;
    }
    if (check_record_list(base, sizeof(uc->uc_mcontext.__reserved), 1, layout, &seen,
                          sve_vl, za_vl, &extra, err, errlen, snapshot))
        return 1;
    if (extra != NULL) {
        const uint8_t *p = (const uint8_t *)(uintptr_t)extra->datap;
        if (((uintptr_t)p & 15) != 0 || extra->size < 16) {
            snprintf(err, errlen, "EXTRA datap %p size %u", p, extra->size);
            return 1;
        }
        if (snapshot != NULL) {
            uintptr_t end = (uintptr_t)base + sizeof(uc->uc_mcontext.__reserved);
            snapshot->extra_present = 1;
            snapshot->extra_size = extra->size;
            snapshot->extra_after_reserved = (uintptr_t)p >= end;
            snapshot->extra_distance = (uintptr_t)p >= end
                ? (uintptr_t)p - end : end - (uintptr_t)p;
        }
        if (check_record_list(p, extra->size, 0, layout, &seen, sve_vl, za_vl, &extra,
                              err, errlen, snapshot))
            return 1;
    }
    if (snapshot != NULL)
        snapshot->valid = 1;
    return 0;
}

int
a64p_check_frame_records(const ucontext_t *uc, uint64_t *layout, int *sve_vl, int *za_vl,
                         char *err, size_t errlen)
{
    return a64p_check_frame_records_snapshot(uc, layout, sve_vl, za_vl, err, errlen, NULL);
}
