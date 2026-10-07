/* EXPERIMENT ONLY (#60 link-policy controls). Each machine broadcasts a
 * numbered datagram every GAP ns of guest time and drains every datagram it
 * receives, until guest time END. Per source and per 100 ms window of the
 * SENDER's stamp it records receipts, min/max/sum one-way delay (receiver
 * world clock minus sender stamp) and out-of-order receipts, then prints them. It
 * also prints how many datagrams it sent per window: the guest clock only
 * advances at deterministic points, so the real send rate varies with the
 * exit pattern and can't be assumed from GAP. */
#define _POSIX_C_SOURCE 200809L
#include <arpa/inet.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#define WIN 100000000ULL
#define NWIN 64
#define NSRC 8
/* World time in ns: user RDTSC traps (CR4.TSD) and the guest kernel returns
 * the VMM's virtual TSC (1 GHz). CLOCK_MONOTONIC would not do: it starts when
 * the guest's timekeeping starts, tens of ms after world time 0, so stamps
 * would not line up with fault times. */
static uint64_t now(void) { uint32_t lo, hi; __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi)); return (uint64_t)hi << 32 | lo; }
struct cell { uint64_t n, min, max, sum; };
static struct cell cells[NSRC][NWIN];
static char names[NSRC][16];
static uint64_t last_seq[NSRC], reorders[NSRC], have[NSRC], sent[NWIN];
static int src_index(const char *n) {
	for (int i = 0; i < NSRC; i++) {
		if (!names[i][0]) { snprintf(names[i], sizeof(names[i]), "%s", n); return i; }
		if (!strcmp(names[i], n)) return i;
	}
	return -1;
}
int main(int argc, char **argv) {
	if (argc < 4) return 3;
	const char *name = argv[1];
	uint64_t gap = strtoull(argv[2], 0, 10), end = strtoull(argv[3], 0, 10), seq = 0;
	int fd = socket(AF_INET, SOCK_DGRAM, 0), on = 1;
	if (fd < 0 || setsockopt(fd, SOL_SOCKET, SO_BROADCAST, &on, sizeof(on))) { perror("socket"); return 3; }
	struct sockaddr_in me = {.sin_family = AF_INET, .sin_port = htons(9093), .sin_addr.s_addr = htonl(INADDR_ANY)};
	if (bind(fd, (void *)&me, sizeof(me))) { perror("bind"); return 3; }
	struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(9093)};
	inet_pton(AF_INET, "10.0.0.255", &a.sin_addr);
	uint64_t next = now();
	for (;;) {
		uint64_t t = now();
		if (t >= end) break;
		if (t >= next) {
			char b[64]; int n = snprintf(b, sizeof(b), "LNK %s %llu %llu", name, (unsigned long long)seq++, (unsigned long long)t);
			if (sendto(fd, b, n, 0, (void *)&a, sizeof(a)) != n) { perror("sendto"); return 3; }
			next = t + gap;
			if (t / WIN < NWIN) sent[t / WIN]++;
		}
		char r[64];
		int n;
		while ((n = recv(fd, r, sizeof(r) - 1, MSG_DONTWAIT)) > 0) {
			uint64_t at = now();
			r[n] = 0;
			char src[16]; unsigned long long s, stamp;
			if (sscanf(r, "LNK %15s %llu %llu", src, &s, &stamp) != 3 || !strcmp(src, name)) continue;
			int i = src_index(src);
			if (i < 0) continue;
			if (have[i] && s <= last_seq[i]) reorders[i]++;
			have[i] = 1; last_seq[i] = s;
			uint64_t w = stamp / WIN, d = at - stamp;
			if (w >= NWIN) continue;
			struct cell *c = &cells[i][w];
			if (!c->n || d < c->min) c->min = d;
			if (d > c->max) c->max = d;
			c->n++; c->sum += d;
		}
	}
	printf("LINK-PROBE %s sent=%llu gap=%llu\n", name, (unsigned long long)seq, (unsigned long long)gap);
	for (int w = 0; w < NWIN; w++)
		if (sent[w]) printf("LINK-SENT %s w=%d n=%llu\n", name, w, (unsigned long long)sent[w]);
	for (int i = 0; i < NSRC && names[i][0]; i++) {
		printf("LINK-SRC %s<-%s reorders=%llu\n", name, names[i], (unsigned long long)reorders[i]);
		for (int w = 0; w < NWIN; w++) {
			struct cell *c = &cells[i][w];
			if (c->n) printf("LINK-WIN %s<-%s w=%d n=%llu min=%llu max=%llu mean=%llu\n", name, names[i], w,
				(unsigned long long)c->n, (unsigned long long)c->min, (unsigned long long)c->max, (unsigned long long)(c->sum / c->n));
		}
	}
	fflush(stdout);
	return 0;
}
