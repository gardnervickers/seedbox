/* Host tests for guest_network.h: argument parsing everywhere, and interface
 * configuration of `lo` inside an unprivileged user and network namespace on
 * Linux. Prints "skip" for configuration when the host forbids namespaces. */
#define _GNU_SOURCE
#include "guest_network.h"
#include <arpa/inet.h>
#include <stdio.h>
#include <stdlib.h>
#if defined(__linux__)
#include <sched.h>
#endif

static int failures;
#define CHECK(condition)                                                        \
    do {                                                                        \
        if (!(condition)) {                                                     \
            fprintf(stderr, "%s:%d: %s\n", __FILE__, __LINE__, #condition);     \
            ++failures;                                                         \
        }                                                                       \
    } while (0)

static void accepts(const char *line, const char *address, const char *netmask) {
    struct sbx_ipv4 config;
    int found = sbx_network_parse(line, &config);
    CHECK(found == 1);
    if (found != 1) { fprintf(stderr, "  rejected: %s\n", line); return; }
    struct in_addr expected_address, expected_netmask;
    CHECK(inet_pton(AF_INET, address, &expected_address) == 1);
    CHECK(inet_pton(AF_INET, netmask, &expected_netmask) == 1);
    CHECK(config.address == expected_address.s_addr);
    CHECK(config.netmask == expected_netmask.s_addr);
}

static void rejects(const char *line) {
    struct sbx_ipv4 config;
    errno = 0;
    int found = sbx_network_parse(line, &config);
    CHECK(found == -1 && errno == EINVAL);
    if (found != -1) fprintf(stderr, "  accepted: %s\n", line);
}

static void absent(const char *line) {
    struct sbx_ipv4 config;
    CHECK(sbx_network_parse(line, &config) == 0);
}

static void parsing(void) {
    accepts("seedbox.ipv4=10.0.0.1/24", "10.0.0.1", "255.255.255.0");
    accepts("console=ttyS0 seedbox.ipv4=192.168.7.254/16 quiet\n", "192.168.7.254", "255.255.0.0");
    accepts("a\tseedbox.ipv4=0.0.0.0/0\n", "0.0.0.0", "0.0.0.0");
    accepts("seedbox.ipv4=255.255.255.255/32", "255.255.255.255", "255.255.255.255");
    accepts("seedbox.ipv4=1.2.3.4/1", "1.2.3.4", "128.0.0.0");
    accepts("seedbox.ipv4=1.2.3.4/31", "1.2.3.4", "255.255.255.254");
    absent("");
    absent("console=ttyS0 quiet\n");
    absent("xseedbox.ipv4=1.2.3.4/8 seedbox.ipv4x=1 seedbox.ipv6=::1/64");
    rejects("seedbox.ipv4=");
    rejects("seedbox.ipv4=10.0.0.1");
    rejects("seedbox.ipv4=10.0.0/24");
    rejects("seedbox.ipv4=10.0.0.1.2/24");
    rejects("seedbox.ipv4=10.0.0.256/24");
    rejects("seedbox.ipv4=10.0.0.01/24");
    rejects("seedbox.ipv4=10.0.0.1/33");
    rejects("seedbox.ipv4=10.0.0.1/08");
    rejects("seedbox.ipv4=10.0.0.1/");
    rejects("seedbox.ipv4=10.0.0.1/24x");
    rejects("seedbox.ipv4=10.0.0.1/24/");
    rejects("seedbox.ipv4=+10.0.0.1/24");
    rejects("seedbox.ipv4=10.0.0.-1/24");
    rejects("seedbox.ipv4=10.0.0.1000/24");
    rejects("seedbox.ipv4=10.0.0.1/100");
    rejects("seedbox.ipv4= 10.0.0.1/24");
    rejects("seedbox.ipv4=10.0.0.1/24 seedbox.ipv4=10.0.0.1/24");
    rejects("seedbox.ipv4=10.0.0.1/24 seedbox.ipv4=bad");
}

#if defined(__linux__)
static uint32_t read_back(const char *interface, unsigned long command) {
    struct ifreq request;
    int fd = sbx_network_request(interface, &request);
    CHECK(fd >= 0);
    CHECK(sbx_network_ioctl(fd, command, &request) == 0);
    struct sockaddr_in address;
    memcpy(&address, &request.ifr_addr, sizeof(address));
    return address.sin_addr.s_addr;
}

static int flags(const char *interface) {
    struct ifreq request;
    int fd = sbx_network_request(interface, &request);
    CHECK(fd >= 0);
    CHECK(sbx_network_ioctl(fd, SIOCGIFFLAGS, &request) == 0);
    return request.ifr_flags;
}

static int configuring(void) {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET)) return 0;
    CHECK(!(flags("lo") & IFF_UP));
    int loopback_up = sbx_network_up("lo");
    if (loopback_up == -1 && (errno == EPERM || errno == EACCES)) return 0;
    CHECK(loopback_up == 0);
    CHECK(flags("lo") & IFF_UP);
    /* The kernel assigns loopback's own address when it comes up. */
    CHECK(read_back("lo", SIOCGIFADDR) == htonl(INADDR_LOOPBACK));
    struct sbx_ipv4 config;
    const char *cases[][3] = {
        {"seedbox.ipv4=10.1.2.3/24", "10.1.2.3", "255.255.255.0"},
        {"seedbox.ipv4=10.1.2.4/0", "10.1.2.4", "0.0.0.0"},
        {"seedbox.ipv4=10.1.2.5/32", "10.1.2.5", "255.255.255.255"},
    };
    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); ++i) {
        CHECK(sbx_network_parse(cases[i][0], &config) == 1);
        CHECK(sbx_network_configure(&config, "lo") == 0);
        struct in_addr address, netmask;
        inet_pton(AF_INET, cases[i][1], &address);
        inet_pton(AF_INET, cases[i][2], &netmask);
        CHECK(read_back("lo", SIOCGIFADDR) == address.s_addr);
        CHECK(read_back("lo", SIOCGIFNETMASK) == netmask.s_addr);
        CHECK(flags("lo") & IFF_UP);
    }
    errno = 0;
    CHECK(sbx_network_configure(&config, "eth9") == -1 && errno == ENODEV);
    errno = 0;
    CHECK(sbx_network_up("eth9") == -1 && errno == ENODEV);
    errno = 0;
    CHECK(sbx_network_up("an-interface-name-too-long") == -1 && errno == EINVAL);
    return 1;
}
#else
static int configuring(void) { return 0; }
#endif

int main(void) {
    parsing();
    int configured = configuring();
    if (failures) return 1;
    printf("guest network parse passed; configure %s\n", configured ? "passed" : "skipped");
    return 0;
}
