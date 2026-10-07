/*
 * Raw-block durability probe for /dev/vda (virtio-blk, writeback cache with
 * FLUSH, no native FUA). Run once per boot of the same machine.
 *
 * First boot (block 0 holds no probe header):
 *   block 0 <- header, then fsync                      (write + FLUSH)
 *   block 1 <- 'A', then fsync                         (write + FLUSH)
 *   block 2 <- 'F' through an O_DSYNC descriptor       (FUA, emulated by Linux as write + postflush)
 *   block 1 <- 'B', block 3 <- 'C', no flush           (volatile)
 *   read blocks 1 and 3 back: 'B' and 'C'              (read-your-writes)
 * then, with "nocrash", fsync and check B and C again and exit; otherwise
 * print "BLKPROBE ready" and wait for the machine to crash.
 *
 * Later boot (header present): blocks 1, 2 and 3 must read 'A', 'F' and
 * zeros: flushed and FUA writes survive, unflushed ones are gone.
 *
 * Every transfer is O_DIRECT, so the page cache never holds the data and
 * every write reaches the device. Exit status: 0 PASS, 1 FAIL, 2 error.
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define BLOCK 4096
#define MAGIC "SBXBLKPROBE1"

static char *buf;

static void die(const char *what) {
	perror(what);
	exit(2);
}

static void put(int fd, int block, char fill) {
	memset(buf, fill, BLOCK);
	if (fill == 'H')
		memcpy(buf, MAGIC, sizeof MAGIC);
	if (pwrite(fd, buf, BLOCK, (off_t)block * BLOCK) != BLOCK)
		die("pwrite");
}

/* The byte every byte of `block` holds, or '?' if they differ. */
static char get(int fd, int block) {
	if (pread(fd, buf, BLOCK, (off_t)block * BLOCK) != BLOCK)
		die("pread");
	for (int i = 1; i < BLOCK; i++)
		if (buf[i] != buf[0])
			return '?';
	return buf[0];
}

static int expect(int fd, int block, char want, const char *when) {
	char got = get(fd, block);
	printf("BLKPROBE %s block %d: %s\n", when, block,
	       got == want ? "ok" : "MISMATCH");
	if (got != want)
		printf("BLKPROBE   want %#x got %#x\n", want, got);
	return got == want;
}

int main(int argc, char **argv) {
	int nocrash = argc > 1 && !strcmp(argv[1], "nocrash");
	setvbuf(stdout, NULL, _IOLBF, 0);
	if (posix_memalign((void **)&buf, BLOCK, BLOCK))
		die("posix_memalign");
	int fd = open("/dev/vda", O_RDWR | O_DIRECT);
	if (fd < 0)
		die("open /dev/vda");
	if (pread(fd, buf, BLOCK, 0) != BLOCK)
		die("pread header");
	if (!memcmp(buf, MAGIC, sizeof MAGIC)) {
		int ok = expect(fd, 1, 'A', "recovered") & expect(fd, 2, 'F', "recovered") &
			 expect(fd, 3, 0, "recovered");
		printf("BLKPROBE %s\n", ok ? "PASS" : "FAIL");
		return !ok;
	}
	int dsync = open("/dev/vda", O_RDWR | O_DIRECT | O_DSYNC);
	if (dsync < 0)
		die("open /dev/vda O_DSYNC");
	put(fd, 0, 'H');
	if (fsync(fd))
		die("fsync header");
	put(fd, 1, 'A');
	if (fsync(fd))
		die("fsync A");
	put(dsync, 2, 'F');
	put(fd, 1, 'B');
	put(fd, 3, 'C');
	int ok = expect(fd, 1, 'B', "unflushed") & expect(fd, 3, 'C', "unflushed");
	if (nocrash) {
		if (fsync(fd))
			die("fsync B C");
		ok &= expect(fd, 1, 'B', "flushed") & expect(fd, 2, 'F', "flushed") &
		      expect(fd, 3, 'C', "flushed");
		printf("BLKPROBE %s\n", ok ? "PASS" : "FAIL");
		return !ok;
	}
	if (!ok) {
		printf("BLKPROBE FAIL\n");
		return 1;
	}
	printf("BLKPROBE ready\n");
	for (;;)
		pause();
}
