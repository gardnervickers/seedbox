/* Static IPv4 bootstrap for the guest's single Ethernet interface.
 *
 * The machine factory appends `seedbox.ipv4=A.B.C.D/P` to the kernel command
 * line only for a machine with a network interface. Init then brings `lo` up
 * and assigns that address and netmask to `eth0` before Ready. It adds
 * no gateway, DHCP or DNS. Without the argument nothing is configured, and
 * `lo` stays down as before.
 *
 * TODO: Re-evaluate: header-only static helpers so the guest build keeps its
 * fixed object list. Ioctls rather than netlink keep the parser and setup
 * small. A later route or multi-NIC surface would replace this argument. */
#ifndef SEEDBOX_GUEST_NETWORK_H
#define SEEDBOX_GUEST_NETWORK_H
#include <errno.h>
#include <net/if.h>
#include <netinet/in.h>
#include <stdint.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>
/* `eth0` exists when init runs: virtio-net probes synchronously, during the
 * kernel's initcalls when built in, or before `modprobe virtio_net` returns
 * when modular. A missing interface is an error, with no polling. */

#define SBX_NETWORK_ARGUMENT "seedbox.ipv4="

/* Both fields are in network byte order. */
struct sbx_ipv4 {
    uint32_t address;
    uint32_t netmask;
};

/* Decimal 0..max with no sign, no leading zero and at most three digits.
 * Returns the position after it, or NULL. */
static const char *sbx_network_number(const char *at, unsigned max, unsigned *out) {
    unsigned value = 0, digits = 0;
    while (*at >= '0' && *at <= '9') {
        if (digits == 1 && value == 0) return NULL;
        if (++digits > 3) return NULL;
        value = value * 10 + (unsigned)(*at++ - '0');
    }
    if (!digits || value > max) return NULL;
    *out = value;
    return at;
}

/* Parse the whole token value `A.B.C.D/P`, ending at whitespace or NUL. */
static int sbx_network_value(const char *at, struct sbx_ipv4 *out) {
    uint32_t address = 0;
    unsigned part, prefix;
    for (int i = 0; i < 4; ++i) {
        if (!(at = sbx_network_number(at, 255, &part))) return -1;
        if (*at++ != (i < 3 ? '.' : '/')) return -1;
        address = address << 8 | part;
    }
    if (!(at = sbx_network_number(at, 32, &prefix))) return -1;
    if (*at && *at != ' ' && *at != '\t' && *at != '\n') return -1;
    /* A 32-bit shift is undefined; prefix zero is the empty mask. */
    uint32_t mask = prefix ? UINT32_MAX << (32 - prefix) : 0;
    out->address = htonl(address);
    out->netmask = htonl(mask);
    return 0;
}

/* 1 when exactly one well-formed argument is present, 0 when absent, and -1
 * with errno EINVAL when malformed or repeated. Tokens are whitespace
 * separated; a token must start with the whole argument name. */
static int sbx_network_parse(const char *line, struct sbx_ipv4 *out) {
    int found = 0;
    const size_t name = sizeof(SBX_NETWORK_ARGUMENT) - 1;
    for (const char *at = line; *at;) {
        if (*at == ' ' || *at == '\t' || *at == '\n') { ++at; continue; }
        if (!strncmp(at, SBX_NETWORK_ARGUMENT, name)) {
            if (found++ || sbx_network_value(at + name, out)) { errno = EINVAL; return -1; }
        }
        while (*at && *at != ' ' && *at != '\t' && *at != '\n') ++at;
    }
    return found;
}

/* Name `request` and open a control socket for it; -1 on failure. */
static int sbx_network_request(const char *interface, struct ifreq *request) {
    if (strlen(interface) >= sizeof(request->ifr_name)) { errno = EINVAL; return -1; }
    memset(request, 0, sizeof(*request));
    memcpy(request->ifr_name, interface, strlen(interface) + 1);
    return socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
}

static int sbx_network_ioctl(int fd, unsigned long command, struct ifreq *request) {
    int result = ioctl(fd, command, request);
    int error = errno;
    close(fd);
    errno = error;
    return result;
}

/* Set IFF_UP on `interface`, keeping its other flags. */
static int sbx_network_up(const char *interface) {
    struct ifreq request;
    int fd = sbx_network_request(interface, &request);
    if (fd < 0) return -1;
    if (ioctl(fd, SIOCGIFFLAGS, &request)) {
        int error = errno;
        close(fd);
        errno = error;
        return -1;
    }
    request.ifr_flags |= IFF_UP;
    return sbx_network_ioctl(fd, SIOCSIFFLAGS, &request);
}

static int sbx_network_set(const char *interface, unsigned long command, uint32_t value) {
    struct ifreq request;
    int fd = sbx_network_request(interface, &request);
    if (fd < 0) return -1;
    struct sockaddr_in address = {.sin_family = AF_INET, .sin_addr.s_addr = value};
    memcpy(&request.ifr_addr, &address, sizeof(address));
    return sbx_network_ioctl(fd, command, &request);
}

/* Assign the address, then the netmask (SIOCSIFADDR sets a classful one), then
 * bring the interface up. */
static int sbx_network_configure(const struct sbx_ipv4 *config, const char *interface) {
    if (sbx_network_set(interface, SIOCSIFADDR, config->address) ||
        sbx_network_set(interface, SIOCSIFNETMASK, config->netmask))
        return -1;
    return sbx_network_up(interface);
}
#endif
