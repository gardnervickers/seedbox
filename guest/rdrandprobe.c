/* What a guest sees of RDRAND and RDSEED. Seedbox hides both from CPUID, but
 * AMD SVM cannot intercept the instructions, so only code that honours CPUID
 * stays deterministic.
 *
 *   rdrandprobe cpuid   print the CPUID bits (leaf 1 ECX 30, leaf 7 EBX 18)
 *   rdrandprobe honest  use RDRAND only if CPUID offers it, as runtimes do
 *   rdrandprobe raw     execute RDRAND and RDSEED regardless: "#UD" when the
 *                       CPU refuses, otherwise the carry flag and value */
#include <cpuid.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static const char *insn = "rdrand";

static void refused(int sig) {
    (void)sig;
    char msg[32];
    int n = snprintf(msg, sizeof(msg), "%s: #UD\n", insn);
    if (write(1, msg, (size_t)n) < 0) _exit(1);
    _exit(0);
}

static void offered(int *rdrand, int *rdseed) {
    unsigned a, b, c, d;
    *rdrand = __get_cpuid(1, &a, &b, &c, &d) && (c >> 30 & 1);
    *rdseed = __get_cpuid_count(7, 0, &a, &b, &c, &d) && (b >> 18 & 1);
}

static void rdrand(void) {
    unsigned long long v; unsigned char ok;
    __asm__ volatile("rdrand %0; setc %1" : "=r"(v), "=qm"(ok));
    printf("rdrand: ok=%u value=%016llx\n", ok, v);
}

static void rdseed(void) {
    unsigned long long v; unsigned char ok;
    __asm__ volatile("rdseed %0; setc %1" : "=r"(v), "=qm"(ok));
    printf("rdseed: ok=%u value=%016llx\n", ok, v);
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "cpuid";
    int has_rdrand, has_rdseed;
    offered(&has_rdrand, &has_rdseed);
    if (!strcmp(mode, "cpuid")) {
        printf("cpuid: rdrand=%d rdseed=%d\n", has_rdrand, has_rdseed);
    } else if (!strcmp(mode, "honest")) {
        if (has_rdrand) rdrand(); else printf("rdrand: not offered\n");
        if (has_rdseed) rdseed(); else printf("rdseed: not offered\n");
    } else if (!strcmp(mode, "raw")) {
        signal(SIGILL, refused);
        rdrand();
        fflush(stdout);
        insn = "rdseed";
        rdseed();
    } else {
        fprintf(stderr, "usage: rdrandprobe cpuid | honest | raw\n");
        return 2;
    }
    return 0;
}
