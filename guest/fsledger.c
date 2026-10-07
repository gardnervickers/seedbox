/*
 * Acknowledged-fsync ledger on a real filesystem mounted at /mnt. Run once
 * per boot of the same machine.
 *
 * First boot (no /mnt/ledger): create the ledger and fsync it and its
 * directory, append records 1..ACKED with an fsync after each (each is
 * acknowledged once its fsync returns), append ACKED+1..TOTAL with no
 * fsync, print "LEDGER ready" and wait for the machine to crash.
 *
 * Later boot: the ledger must hold records 1..M in order, whole, with
 * ACKED <= M <= TOTAL. Unacknowledged records may or may not survive (a
 * journal commit may have flushed them), but nothing acknowledged may be
 * missing and nothing may be torn or out of order.
 *
 * Exit status: 0 PASS, 1 FAIL, 2 error.
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define ACKED 50
#define TOTAL 70
#define PATH "/mnt/ledger"

static void die(const char *what) {
	perror(what);
	exit(2);
}

static void record(int fd, int n, int sync) {
	char line[32];
	int len = snprintf(line, sizeof line, "rec %06d\n", n);
	if (write(fd, line, len) != len)
		die("write");
	if (sync && fsync(fd))
		die("fsync");
}

static int verify(void) {
	FILE *f = fopen(PATH, "r");
	if (!f)
		die("open ledger");
	char line[64];
	int m = 0, ok = 1;
	while (fgets(line, sizeof line, f)) {
		char want[32];
		snprintf(want, sizeof want, "rec %06d\n", m + 1);
		if (strcmp(line, want)) {
			printf("LEDGER record %d is wrong or torn\n", m + 1);
			ok = 0;
			break;
		}
		m++;
	}
	fclose(f);
	printf("LEDGER recovered %d records (%d acknowledged, %d written)\n", m, ACKED, TOTAL);
	ok &= m >= ACKED && m <= TOTAL;
	printf("LEDGER %s\n", ok ? "PASS" : "FAIL");
	return !ok;
}

int main(void) {
	setvbuf(stdout, NULL, _IOLBF, 0);
	if (access(PATH, F_OK) == 0)
		return verify();
	int fd = open(PATH, O_WRONLY | O_CREAT | O_EXCL | O_APPEND, 0644);
	if (fd < 0)
		die("create ledger");
	if (fsync(fd))
		die("fsync ledger");
	int dir = open("/mnt", O_RDONLY | O_DIRECTORY);
	if (dir < 0 || fsync(dir))
		die("fsync /mnt");
	for (int n = 1; n <= TOTAL; n++)
		record(fd, n, n <= ACKED);
	printf("LEDGER acked %d, wrote %d\n", ACKED, TOTAL);
	printf("LEDGER ready\n");
	for (;;)
		pause();
}
