/* Print 16 bytes from getrandom(2) as hex. With "rdrand", execute RDRAND
 * without consulting CPUID (Seedbox hides it) and report what comes back:
 * "#UD" when the CPU refuses it, otherwise the carry flag and value. */
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/random.h>
#include <unistd.h>

static void refused(int sig) {
    (void)sig;
    static const char msg[] = "rdrand: #UD\n";
    if (write(1, msg, sizeof(msg) - 1) < 0) _exit(1);
    _exit(0);
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "rdrand")) {
        unsigned long long value;
        unsigned char ok;
        signal(SIGILL, refused);
        __asm__ volatile("rdrand %0; setc %1" : "=r"(value), "=qm"(ok));
        printf("rdrand: ok=%u value=%016llx\n", ok, value);
        return 0;
    }
    unsigned char bytes[16];
    if (getrandom(bytes, sizeof(bytes), 0) != sizeof(bytes)) { perror("getrandom"); return 1; }
    for (size_t i = 0; i < sizeof(bytes); i++) printf("%02x", bytes[i]);
    printf("\n");
    return 0;
}
