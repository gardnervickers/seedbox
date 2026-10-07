/* Guest-side Seedbox helper: report results to the VMM through I/O ports. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/io.h>
#include <unistd.h>
#include <time.h>
#include <stdint.h>
#include "barrier.h"

int main(int argc, char **argv) {
	if (argc == 2 && !strcmp(argv[1], "time")) {
		struct timespec t;
		if (clock_gettime(CLOCK_MONOTONIC, &t)) { perror("clock_gettime"); return 1; }
		printf("%llu\n", (unsigned long long)((uint64_t)t.tv_sec * 1000000000 + t.tv_nsec));
		return 0;
	}
	if (argc == 3 && !strcmp(argv[1], "exit")) {
		if (ioperm(0xf4, 1, 1)) { perror("ioperm"); return 1; }
		outb(atoi(argv[2]), 0xf4);
		for (;;) pause();
	}
	if (argc == 2 && !strcmp(argv[1], "barrier")) {
		if (seedbox_launch_barrier("/proc/cmdline", "/dev/urandom")) { perror("barrier"); return 1; }
		return 0;
	}
	fprintf(stderr, "usage: sbx exit CODE | time | barrier\n");
	return 2;
}
