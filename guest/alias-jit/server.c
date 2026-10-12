/* A TCP service that publishes and executes code through distinct shared RW/RX
 * aliases. Ordinary ARM cache maintenance is issued only through the RW alias. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <unistd.h>

static void require(int ok)
{
    if (!ok) { perror("alias-jit"); exit(2); }
}
static uint64_t exercise(int append, unsigned count)
{
    size_t size = (count + 1) * 16;
    int fd = memfd_create("alias-jit", 0);
    require(fd >= 0 && ftruncate(fd, size) == 0);
    uint32_t *rw = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    uint32_t *rx = mmap(NULL, size, PROT_READ | PROT_EXEC, MAP_SHARED, fd, 0);
    require(rw != MAP_FAILED && rx != MAP_FAILED);
    close(fd);
    rw[0] = 0x528009a0; rw[1] = 0xd65f03c0;
    __builtin___clear_cache((char *)rw, (char *)(rw + 2));
    unsigned (*neighbor)(void) = (unsigned (*)(void))rx;
    require(neighbor() == 77);
    uint64_t sum = 0;
    for (unsigned i = 1; i <= count; ++i) {
        size_t offset = (append ? i : 1) * 4;
        rw[offset] = 0x52800000u | (i << 5);
        rw[offset + 1] = 0xd65f03c0;
        __builtin___clear_cache((char *)(rw + offset), (char *)(rw + offset + 2));
        unsigned (*current)(void) = (unsigned (*)(void))(rx + offset);
        for (unsigned j = 0; j < 128; ++j) {
            unsigned a = current(), b = neighbor();
            require(a == i && b == 77);
            sum += a + b;
        }
    }
    require(munmap(rw, size) == 0 && munmap(rx, size) == 0);
    return sum;
}
int main(void)
{
    int server = socket(AF_INET, SOCK_STREAM, 0), reuse = 1;
    require(server >= 0);
    require(setsockopt(server, SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof(reuse)) == 0);
    struct sockaddr_in address = {.sin_family = AF_INET, .sin_port = htons(12345),
                                  .sin_addr.s_addr = htonl(INADDR_ANY)};
    require(bind(server, (struct sockaddr *)&address, sizeof(address)) == 0);
    require(listen(server, 4) == 0);
    for (;;) {
        int client = accept(server, NULL, NULL);
        require(client >= 0);
        char request[80] = {0}, response[120];
        size_t used = 0;
        while (used < sizeof(request) - 1 && !strchr(request, '\n')) {
            ssize_t n = recv(client, request + used, sizeof(request) - 1 - used, 0);
            require(n > 0); used += (size_t)n;
        }
        int stop = !strcmp(request, "stop\n");
        if (stop) strcpy(response, "STOPPED\n");
        else {
            char mode[16]; unsigned count;
            require(sscanf(request, "%15s %u", mode, &count) == 2 && count > 0 && count <= 60000);
            require(!strcmp(mode, "append") || !strcmp(mode, "patch"));
            uint64_t sum = exercise(!strcmp(mode, "append"), count);
            snprintf(response, sizeof(response), "%llu\n", (unsigned long long)sum);
            printf("alias-jit: mode=%s publications=%u calls=%u checksum=%llu\n",
                   mode, count, count * 256, (unsigned long long)sum);
            fflush(stdout);
        }
        size_t sent = 0, size = strlen(response);
        while (sent < size) {
            ssize_t n = send(client, response + sent, size - sent, MSG_NOSIGNAL);
            require(n > 0); sent += (size_t)n;
        }
        close(client);
        if (stop) break;
    }
    close(server);
    puts("alias-jit: completed");
    return 0;
}
