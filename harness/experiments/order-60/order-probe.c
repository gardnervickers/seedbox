/* EXPERIMENT ONLY (#60 ordering spike). Broadcast numbered UDP datagrams
 * every GAP ns of guest time until guest time END, stamping each with the
 * guest clock just before the send. busy: spin on the clock between sends
 * (the machine never halts); sleep: nanosleep between sends (it halts). */
#define _POSIX_C_SOURCE 200809L
#include <arpa/inet.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
static uint64_t now(void) { struct timespec t; if (clock_gettime(CLOCK_MONOTONIC, &t)) exit(3); return (uint64_t)t.tv_sec * 1000000000 + t.tv_nsec; }
int main(int argc, char **argv) {
	if (argc < 5) return 3;
	const char *name = argv[1]; int sleepy = !strcmp(argv[2], "sleep");
	uint64_t gap = strtoull(argv[3], 0, 10), end = strtoull(argv[4], 0, 10), seq = 0;
	int fd = socket(AF_INET, SOCK_DGRAM, 0), on = 1;
	if (fd < 0 || setsockopt(fd, SOL_SOCKET, SO_BROADCAST, &on, sizeof(on))) { perror("socket"); return 3; }
	struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(9092)};
	inet_pton(AF_INET, "10.0.0.255", &a.sin_addr);
	uint64_t first = now(), next = first;
	for (;;) {
		uint64_t t = now();
		if (t >= end) break;
		if (t < next) {
			if (sleepy) { struct timespec s = {0, (long)(next - t)}; nanosleep(&s, 0); }
			continue;
		}
		char b[48]; int n = snprintf(b, sizeof(b), "ORD %s %llu %llu", name, (unsigned long long)seq++, (unsigned long long)t);
		if (sendto(fd, b, n, 0, (void *)&a, sizeof(a)) != n) { perror("sendto"); return 3; }
		next = t + gap;
	}
	printf("ORDER-PROBE %s mode=%s sent=%llu first_ns=%llu end_ns=%llu\n", name, argv[2], (unsigned long long)seq, (unsigned long long)first, (unsigned long long)end);
	return 0;
}
