/* Guest-side Seedbox helper: report results to the VMM through I/O ports. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/io.h>
#include <unistd.h>

int main(int argc, char **argv) {
	if (argc == 3 && !strcmp(argv[1], "exit")) {
		if (ioperm(0xf4, 1, 1)) { perror("ioperm"); return 1; }
		outb(atoi(argv[2]), 0xf4);
		for (;;) pause();
	}
	fprintf(stderr, "usage: sbx exit CODE\n");
	return 2;
}
